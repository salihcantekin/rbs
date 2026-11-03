//! The PipeWire backend on Linux. Overview in the crate documentation.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::SpaTypes;

use rbs_audio_io::ring::{self, RingConsumer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointKind,
    ErrorDomain, InputRequest, InputStats, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{AudioFormat, MAX_CHANNELS};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::Clock;
use rbs_core::timing::SampleRate;

use crate::ADAPTER_ID;
use crate::graph;
use crate::packets::Packets;

/// Longest wait for the daemon in `open_input` and `start` (the port allows 500 ms for
/// `start`). Both take about 3 ms.
const DAEMON_TIMEOUT: Duration = Duration::from_millis(450);

/// Longest wait for the daemon in `enumerate`. A hang guard (ADR-0032): an
/// enumeration takes about 3 ms (the port's budget is 100 ms).
const ENUMERATE_TIMEOUT: Duration = Duration::from_millis(250);

/// Largest graph cycle when the daemon publishes no `clock.max-quantum`: PipeWire's
/// own upper limit.
const DEFAULT_MAX_QUANTUM: usize = 8_192;

/// Most delay accepted from `pw_stream_get_time` when placing a packet in time.
const MAX_DELAY: Duration = Duration::from_secs(1);

/// Name of the capture stream nodes this adapter creates.
const STREAM_NAME: &str = "rbs-audio-input";

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(pw::init);
}

fn backend(code: i32) -> AudioIoError {
    AudioIoError::Backend {
        domain: ErrorDomain::Errno,
        code: i64::from(code),
        recoverable: true,
    }
}

fn pw_error(_e: pw::Error) -> AudioIoError {
    backend(libc::ECONNREFUSED)
}

/// Whether the socket the default connection uses exists.
pub(crate) fn socket_present() -> bool {
    if let Some(remote) = std::env::var_os("PIPEWIRE_REMOTE") {
        let path = std::path::PathBuf::from(remote);
        if path.is_absolute() {
            return path.exists();
        }
    }
    let Some(dir) =
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
    else {
        return false;
    };
    let name = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    std::path::Path::new(&dir).join(name).exists()
}

/// Nanoseconds on `CLOCK_MONOTONIC`, the clock of PipeWire's cycle times.
fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer; clock_gettime has no other preconditions.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

// ---------------------------------------------------------------------------------
// Registry snapshot

/// An audio node from the registry.
#[derive(Debug, Clone)]
struct NodeInfo {
    id: u32,
    name: String,
    description: String,
    class: String,
    app: Option<String>,
}

/// An audio port from the registry.
#[derive(Debug, Clone)]
struct PortInfo {
    id: u32,
    node: u32,
    output: bool,
    monitor: bool,
    channel: String,
    index: u32,
}

impl PortInfo {
    fn from_props(id: u32, props: &spa::utils::dict::DictRef) -> Option<Self> {
        Some(Self {
            id,
            node: parse_u32(props.get("node.id")?)?,
            output: props.get("port.direction")? == "out",
            monitor: props.get("port.monitor") == Some("true"),
            channel: props.get("audio.channel").unwrap_or("UNK").to_owned(),
            index: props.get("port.id").and_then(parse_u32).unwrap_or(0),
        })
    }
}

/// What the daemon says about audio nodes, ports, defaults and the graph clock.
#[derive(Debug, Default)]
struct Snapshot {
    nodes: Vec<NodeInfo>,
    ports: Vec<PortInfo>,
    default_source: Option<String>,
    default_sink: Option<String>,
    rate: Option<u32>,
    force_rate: Option<u32>,
    max_quantum: Option<u32>,
}

/// An endpoint and the channel positions its stream asks for.
struct Found {
    endpoint: AudioEndpoint,
    positions: Vec<String>,
}

impl Snapshot {
    /// The graph rate every node's ports run at.
    fn rate(&self) -> SampleRate {
        self.force_rate
            .filter(|r| *r > 0)
            .or(self.rate)
            .and_then(SampleRate::new)
            .unwrap_or(SampleRate::HZ_48000)
    }

    fn max_quantum(&self) -> usize {
        self.max_quantum
            .map_or(DEFAULT_MAX_QUANTUM, |q| q as usize)
            .max(1)
    }

    fn endpoints(&self, kind: EndpointKind) -> Vec<Found> {
        let rate = self.rate();
        let mut found: Vec<Found> = Vec::new();
        for node in self
            .nodes
            .iter()
            .filter(|n| graph::class_matches(kind, &n.class))
        {
            if kind == EndpointKind::Application {
                let Some(app) = node.app.as_deref() else {
                    continue;
                };
                let id = graph::endpoint_id(kind, app);
                if found.iter().any(|f| f.endpoint.id == id) {
                    continue;
                }
                let format = AudioFormat::STEREO_48K;
                let format = AudioFormat::new(rate, format.channels).unwrap_or(format);
                found.push(Found {
                    endpoint: AudioEndpoint::new(id, kind, app, format),
                    positions: vec!["FL".into(), "FR".into()],
                });
                continue;
            }
            let monitor = kind == EndpointKind::OutputLoopback;
            let mut ports: Vec<&PortInfo> = self
                .ports
                .iter()
                .filter(|p| p.node == node.id && p.output && p.monitor == monitor)
                .collect();
            ports.sort_by_key(|p| p.index);
            let channels: Vec<String> = ports.iter().map(|p| p.channel.clone()).collect();
            let (layout, positions) = graph::layout_of(&channels);
            let Ok(format) = AudioFormat::new(rate, layout) else {
                continue;
            };
            let default = match kind {
                EndpointKind::Input => self.default_source.as_deref(),
                _ => self.default_sink.as_deref(),
            };
            let endpoint = AudioEndpoint::new(
                graph::endpoint_id(kind, &node.name),
                kind,
                node.description.clone(),
                format,
            )
            .with_default(default == Some(node.name.as_str()));
            found.push(Found {
                endpoint,
                positions,
            });
        }
        found
    }
}

/// Reads the registry and the `default` and `settings` metadata with one short
/// connection (control thread only). Keeps nothing.
fn query(timeout: Duration) -> Result<Snapshot, AudioIoError> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_error)?;
    let core = context.connect_rc(None).map_err(pw_error)?;
    let registry = core.get_registry_rc().map_err(pw_error)?;

    let snapshot = Rc::new(RefCell::new(Snapshot::default()));
    // Bound metadata objects and their listeners; they must outlive the loop.
    let bound: Rc<RefCell<Vec<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(Vec::new()));
    // The sequence number that ends the loop: the first sync, or the one sent after
    // binding a metadata object (its properties arrive before that sync is done).
    let last = Rc::new(Cell::new(core.sync(0).map_err(pw_error)?));
    let done = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done({
            let done = Rc::clone(&done);
            let last = Rc::clone(&last);
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == last.get() {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let snapshot = Rc::clone(&snapshot);
            let bound = Rc::clone(&bound);
            let registry = registry.clone();
            let core = core.clone();
            let last = Rc::clone(&last);
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        let Some(class) = props.get("media.class") else {
                            return;
                        };
                        if !class.starts_with("Audio/") && class != "Stream/Output/Audio" {
                            return;
                        }
                        let Some(name) = props.get("node.name") else {
                            return;
                        };
                        let description = props
                            .get("node.description")
                            .or_else(|| props.get("node.nick"))
                            .unwrap_or(name);
                        snapshot.borrow_mut().nodes.push(NodeInfo {
                            id: global.id,
                            name: name.to_owned(),
                            description: description.to_owned(),
                            class: class.to_owned(),
                            app: props.get("application.name").map(str::to_owned),
                        });
                    }
                    pw::types::ObjectType::Port => {
                        if let Some(port) = PortInfo::from_props(global.id, props) {
                            snapshot.borrow_mut().ports.push(port);
                        }
                    }
                    pw::types::ObjectType::Metadata => {
                        let name = props.get("metadata.name");
                        if name != Some("default") && name != Some("settings") {
                            return;
                        }
                        let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global)
                        else {
                            return;
                        };
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let snapshot = Rc::clone(&snapshot);
                                move |subject, key, _type, value| {
                                    if subject != pw::core::PW_ID_CORE {
                                        return 0;
                                    }
                                    let (Some(key), Some(value)) = (key, value) else {
                                        return 0;
                                    };
                                    let mut s = snapshot.borrow_mut();
                                    match key {
                                        "default.audio.source" => {
                                            s.default_source =
                                                graph::default_name(value).map(str::to_owned);
                                        }
                                        "default.audio.sink" => {
                                            s.default_sink =
                                                graph::default_name(value).map(str::to_owned);
                                        }
                                        "clock.rate" => s.rate = parse_u32(value),
                                        "clock.force-rate" => s.force_rate = parse_u32(value),
                                        "clock.max-quantum" => s.max_quantum = parse_u32(value),
                                        _ => {}
                                    }
                                    0
                                }
                            })
                            .register();
                        if let Ok(seq) = core.sync(0) {
                            last.set(seq);
                        }
                        bound.borrow_mut().push((metadata, listener));
                    }
                    _ => {}
                }
            }
        })
        .register();
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let _ = timer.update_timer(Some(timeout), None);
    mainloop.run();
    if !done.get() {
        return Err(backend(libc::ETIMEDOUT));
    }
    bound.borrow_mut().clear();
    let snapshot = std::mem::take(&mut *snapshot.borrow_mut());
    Ok(snapshot)
}

// ---------------------------------------------------------------------------------
// Backend

/// Backend of the PipeWire audio adapter.
#[derive(Debug, Default)]
pub struct PipeWireAudioBackend {
    _private: (),
}

impl PipeWireAudioBackend {
    /// Creates the backend. Does nothing until used.
    pub fn new() -> Self {
        Self::default()
    }
}

impl AdapterFactory for PipeWireAudioBackend {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "PipeWire audio"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap: only checks that the daemon's socket exists; no connection.
        if !socket_present() {
            return Err(ProbeError::new("no PipeWire daemon socket for this user"));
        }
        let kinds = || {
            CapabilityValue::Set(
                EndpointKind::ALL
                    .iter()
                    .map(|k| k.as_str().to_owned())
                    .collect(),
            )
        };
        Ok(CapabilitySet::new()
            .with(caps::INPUT_KINDS, kinds())
            .with(caps::INPUT_ENUMERATE, kinds())
            .with(
                caps::INPUT_CHANNELS_MAX,
                CapabilityValue::Int(MAX_CHANNELS as i64),
            )
            .with(caps::INPUT_PERMISSION, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl AudioBackend for PipeWireAudioBackend {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        if !EndpointKind::ALL.contains(&kind) {
            return Err(AudioIoError::Unsupported("unknown endpoint kind"));
        }
        Ok(query(ENUMERATE_TIMEOUT)?
            .endpoints(kind)
            .into_iter()
            .map(|f| f.endpoint)
            .collect())
    }

    fn open_input(
        &self,
        request: &InputRequest,
    ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
        let (kind, _) = graph::parse_id(&request.endpoint).ok_or(AudioIoError::EndpointNotFound)?;
        let snapshot = query(DAEMON_TIMEOUT)?;
        let found = snapshot
            .endpoints(kind)
            .into_iter()
            .find(|f| f.endpoint.id == request.endpoint)
            .ok_or(AudioIoError::EndpointNotFound)?;
        let format = request.format_for(&found.endpoint);
        let (_, name) =
            graph::parse_id(&found.endpoint.id).ok_or(AudioIoError::EndpointNotFound)?;
        Ok(Box::new(PipeWireStream {
            target: Target {
                kind,
                name: name.to_owned(),
                positions: found.positions,
            },
            format,
            clock: Arc::clone(&request.clock),
            buffer: request.clamped_buffer(),
            max_packet: snapshot.max_quantum(),
            last_stamp: Arc::new(AtomicU64::new(0)),
            active: None,
            totals: InputStats::default(),
        }))
    }
}

// ---------------------------------------------------------------------------------
// Stream

/// What a stream captures.
#[derive(Debug, Clone)]
struct Target {
    kind: EndpointKind,
    /// `node.name` for devices and monitors, `application.name` for applications.
    name: String,
    /// The channel positions to ask for, in planar order.
    positions: Vec<String>,
}

/// Flags the stream thread shares with the stream.
#[derive(Default)]
struct Status {
    lost: AtomicBool,
}

struct Active {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    consumer: RingConsumer,
    status: Arc<Status>,
}

struct PipeWireStream {
    target: Target,
    format: AudioFormat,
    clock: Arc<dyn Clock>,
    buffer: Duration,
    /// Largest packet PipeWire may deliver (the graph's maximum quantum).
    max_packet: usize,
    /// Master time of the last packet, in nanoseconds, so a restarted stream stamps
    /// later.
    last_stamp: Arc<AtomicU64>,
    active: Option<Active>,
    /// Counters of rings closed by `stop`.
    totals: InputStats,
}

impl PipeWireStream {
    fn activate(&mut self) -> Result<Active, AudioIoError> {
        init();
        let rate = self.format.sample_rate;
        let capacity = ring::capacity_frames(rate, self.buffer, self.max_packet);
        let (producer, consumer) = ring::channel(self.format, capacity, ring::DEFAULT_MARKS);
        let status = Arc::new(Status::default());
        let capture = Capture {
            packets: Packets::new(producer, rate, Arc::clone(&self.last_stamp)),
            clock: Arc::clone(&self.clock),
            channels: self.format.channels.channels(),
        };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = pw::channel::channel();
        let target = self.target.clone();
        let format = self.format;
        let thread_status = Arc::clone(&status);
        let thread = std::thread::Builder::new()
            .name("rbs-pw-audio".into())
            .spawn(move || {
                run_stream(
                    &target,
                    format,
                    capture,
                    &thread_status,
                    started_tx,
                    stop_rx,
                )
            })
            .map_err(|e| backend(e.raw_os_error().unwrap_or(libc::EAGAIN)))?;
        let started = started_rx
            .recv_timeout(DAEMON_TIMEOUT)
            .unwrap_or(Err(backend(libc::ETIMEDOUT)));
        if let Err(e) = started {
            let _ = stop_tx.send(());
            let _ = thread.join();
            let _ = consumer.close();
            return Err(e);
        }
        Ok(Active {
            stop: stop_tx,
            thread: Some(thread),
            consumer,
            status,
        })
    }
}

impl AudioInputStream for PipeWireStream {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.active.is_some() {
            return Ok(());
        }
        let active = self.activate()?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let _ = active.stop.send(());
        if let Some(thread) = active.thread.take() {
            let _ = thread.join();
        }
        self.totals = self.totals.plus(active.consumer.close());
    }

    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let channels = self.format.channels.channels();
        if out.len() != channels || out.iter().any(|c| c.len() != out[0].len()) {
            return Err(AudioIoError::BufferMismatch);
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(AudioRead::EMPTY);
        };
        if active.status.lost.load(Ordering::Acquire) {
            return Err(AudioIoError::EndpointLost);
        }
        active.consumer.read(out)
    }

    fn available(&self) -> usize {
        self.active.as_ref().map_or(0, |a| a.consumer.available())
    }

    fn stats(&self) -> InputStats {
        match &self.active {
            Some(active) => self.totals.plus(active.consumer.stats()),
            None => self.totals,
        }
    }
}

impl Drop for PipeWireStream {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------
// Real-time side

/// State of the process callback. Lives on PipeWire's data thread once the stream is
/// connected and is touched by nothing else; every field is `Send`.
struct Capture {
    packets: Packets,
    clock: Arc<dyn Clock>,
    channels: usize,
}

const _: fn() = || {
    fn send<T: Send>() {}
    send::<Capture>();
};

impl Capture {
    /// One graph cycle. **Real-time**: no allocation, locks, I/O or logging.
    fn on_process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let now = self.clock.now();
        let os_now = monotonic_nanos();
        let channels = self.channels;
        let datas = buffer.datas_mut();
        if datas.len() < channels {
            return;
        }
        let mut planes: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        let mut frames = usize::MAX;
        for (plane, data) in planes.iter_mut().zip(datas.iter_mut()).take(channels) {
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            let Some(bytes) = offset
                .checked_add(size)
                .and_then(|end| bytes.get(offset..end))
            else {
                return;
            };
            // SAFETY: every bit pattern is a valid `f32`, and `align_to` only puts
            // correctly aligned elements in the middle slice. PipeWire does not write
            // the buffer while it is dequeued.
            let (head, samples, _) = unsafe { bytes.align_to::<f32>() };
            if !head.is_empty() {
                return;
            }
            frames = frames.min(samples.len());
            *plane = samples;
        }
        if frames == usize::MAX {
            return;
        }
        for plane in &mut planes[..channels] {
            *plane = &plane[..frames];
        }
        // The cycle's time minus the delay from the device is when the end of this
        // packet was captured.
        let captured_end = match stream.time() {
            Ok(time) if time.now() > 0 => {
                let rate = time.rate();
                let delay = if rate.denom > 0 {
                    let nanos =
                        i128::from(time.delay().max(0)) * i128::from(rate.num) * 1_000_000_000
                            / i128::from(rate.denom);
                    u64::try_from(nanos).unwrap_or(u64::MAX)
                } else {
                    0
                };
                let delay = delay.min(u64::try_from(MAX_DELAY.as_nanos()).unwrap_or(0));
                u64::try_from(time.now())
                    .unwrap_or(os_now)
                    .saturating_sub(delay)
            }
            _ => os_now,
        };
        self.packets
            .push(&planes[..channels], captured_end, os_now, now);
    }
}

// ---------------------------------------------------------------------------------
// Stream thread

/// PipeWire's id for a channel position name.
fn channel_id(name: &str) -> u32 {
    use spa::sys as s;
    const NAMES: [(&str, u32); 25] = [
        ("MONO", s::SPA_AUDIO_CHANNEL_MONO),
        ("FL", s::SPA_AUDIO_CHANNEL_FL),
        ("FR", s::SPA_AUDIO_CHANNEL_FR),
        ("FC", s::SPA_AUDIO_CHANNEL_FC),
        ("LFE", s::SPA_AUDIO_CHANNEL_LFE),
        ("SL", s::SPA_AUDIO_CHANNEL_SL),
        ("SR", s::SPA_AUDIO_CHANNEL_SR),
        ("FLC", s::SPA_AUDIO_CHANNEL_FLC),
        ("FRC", s::SPA_AUDIO_CHANNEL_FRC),
        ("RC", s::SPA_AUDIO_CHANNEL_RC),
        ("RL", s::SPA_AUDIO_CHANNEL_RL),
        ("RR", s::SPA_AUDIO_CHANNEL_RR),
        ("TC", s::SPA_AUDIO_CHANNEL_TC),
        ("TFL", s::SPA_AUDIO_CHANNEL_TFL),
        ("TFC", s::SPA_AUDIO_CHANNEL_TFC),
        ("TFR", s::SPA_AUDIO_CHANNEL_TFR),
        ("TRL", s::SPA_AUDIO_CHANNEL_TRL),
        ("TRC", s::SPA_AUDIO_CHANNEL_TRC),
        ("TRR", s::SPA_AUDIO_CHANNEL_TRR),
        ("RLC", s::SPA_AUDIO_CHANNEL_RLC),
        ("RRC", s::SPA_AUDIO_CHANNEL_RRC),
        ("FLW", s::SPA_AUDIO_CHANNEL_FLW),
        ("FRW", s::SPA_AUDIO_CHANNEL_FRW),
        ("LFE2", s::SPA_AUDIO_CHANNEL_LFE2),
        ("BC", s::SPA_AUDIO_CHANNEL_BC),
    ];
    if let Some((_, id)) = NAMES.iter().find(|(n, _)| *n == name) {
        return *id;
    }
    match name.strip_prefix("AUX").and_then(parse_u32) {
        Some(n) => s::SPA_AUDIO_CHANNEL_AUX0.saturating_add(n),
        None => s::SPA_AUDIO_CHANNEL_UNKNOWN,
    }
}

/// The one format the stream accepts: `f32` planar at the stream's rate with the
/// target's channel positions.
fn format_param(format: AudioFormat, positions: &[String]) -> Option<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32P);
    info.set_rate(format.sample_rate.hz());
    let channels = format.channels.channels();
    info.set_channels(u32::try_from(channels).ok()?);
    let mut position = [0_u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    if positions.len() == channels {
        for (slot, name) in position.iter_mut().zip(positions) {
            *slot = channel_id(name);
        }
        info.set_flags(spa::param::audio::AudioInfoRawFlags::empty());
        info.set_position(position);
    }
    let object = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

/// What the stream thread tracks in the registry.
#[derive(Default)]
struct Watch {
    /// Input and monitor streams: the id of the target node once listed.
    node: Option<u32>,
    /// Application streams: the application's stream nodes.
    app_nodes: HashSet<u32>,
    /// Application streams: the clients owning those nodes. The application has
    /// exited when the last one disconnects.
    app_clients: HashSet<u32>,
    /// Application streams: audio ports seen in the registry.
    ports: Vec<PortInfo>,
    /// Application streams: this stream's node, once connected.
    own_node: Option<u32>,
    /// Application streams: links this stream created, by (output port, input port).
    links: Vec<(u32, u32, pw::link::Link)>,
}

impl Watch {
    /// Links every application port to the stream's matching ports. Control side of
    /// the stream thread; allocates.
    fn relink(&mut self, core: &pw::core::CoreRc) {
        let Some(own) = self.own_node else {
            return;
        };
        let ours: Vec<&PortInfo> = self
            .ports
            .iter()
            .filter(|p| p.node == own && !p.output)
            .collect();
        let mut wanted = Vec::new();
        for app in self
            .ports
            .iter()
            .filter(|p| p.output && !p.monitor && self.app_nodes.contains(&p.node))
        {
            for port in &ours {
                if graph::feeds(&app.channel, app.index, &port.channel, port.index)
                    && !self
                        .links
                        .iter()
                        .any(|(o, i, _)| *o == app.id && *i == port.id)
                {
                    wanted.push((app.node, app.id, port.id));
                }
            }
        }
        for (node, output, input) in wanted {
            let props = pw::properties::properties! {
                "link.output.node" => node.to_string(),
                "link.output.port" => output.to_string(),
                "link.input.node" => own.to_string(),
                "link.input.port" => input.to_string(),
                "object.linger" => "false",
            };
            if let Ok(link) = core.create_object::<pw::link::Link>("link-factory", &props) {
                self.links.push((output, input, link));
            }
        }
    }

    /// Forgets a removed global. Returns whether the target is gone.
    fn remove(&mut self, kind: EndpointKind, id: u32) -> bool {
        if kind != EndpointKind::Application {
            return self.node == Some(id);
        }
        self.app_nodes.remove(&id);
        self.ports.retain(|p| p.id != id && p.node != id);
        self.links.retain(|(o, i, _)| *o != id && *i != id);
        self.app_clients.remove(&id) && self.app_clients.is_empty()
    }
}

/// Runs one stream's PipeWire main loop until told to stop.
fn run_stream(
    target: &Target,
    format: AudioFormat,
    capture: Capture,
    status: &Arc<Status>,
    started: mpsc::SyncSender<Result<(), AudioIoError>>,
    stop: pw::channel::Receiver<()>,
) {
    let started = Rc::new(RefCell::new(Some(started)));
    let report = {
        let started = Rc::clone(&started);
        move |result: Result<(), AudioIoError>| {
            if let Some(tx) = started.borrow_mut().take() {
                let _ = tx.send(result);
            }
        }
    };
    let fail = |report: &dyn Fn(Result<(), AudioIoError>)| report(Err(backend(libc::ECONNREFUSED)));
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        return fail(&report);
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        return fail(&report);
    };
    let Ok(core) = context.connect_rc(None) else {
        return fail(&report);
    };
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let Ok(registry) = core.get_registry_rc() else {
        return fail(&report);
    };

    let kind = target.kind;
    let app = kind == EndpointKind::Application;
    let watch = Rc::new(RefCell::new(Watch::default()));
    let lose = {
        let status = Arc::clone(status);
        move || status.lost.store(true, Ordering::Release)
    };
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let watch = Rc::clone(&watch);
            let core = core.clone();
            let name = target.name.clone();
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                let mut watch = watch.borrow_mut();
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        let class = props.get("media.class").unwrap_or("");
                        if !graph::class_matches(kind, class) {
                            return;
                        }
                        if !app {
                            if props.get("node.name") == Some(name.as_str()) {
                                watch.node = Some(global.id);
                            }
                            return;
                        }
                        if props.get("application.name") == Some(name.as_str()) {
                            watch.app_nodes.insert(global.id);
                            if let Some(client) = props.get("client.id").and_then(parse_u32) {
                                watch.app_clients.insert(client);
                            }
                            watch.relink(&core);
                        }
                    }
                    pw::types::ObjectType::Port if app => {
                        if let Some(port) = PortInfo::from_props(global.id, props) {
                            watch.ports.push(port);
                            watch.relink(&core);
                        }
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let watch = Rc::clone(&watch);
            let lose = lose.clone();
            move |id| {
                if watch.borrow_mut().remove(kind, id) {
                    lose();
                }
            }
        })
        .register();
    // The target may have gone between `open_input` and `start`: if the first
    // registry round trip does not list it, it is lost rather than silent.
    let listed = core.sync(0).ok();
    let _core_listener = core
        .add_listener_local()
        .done({
            let watch = Rc::clone(&watch);
            let lose = lose.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || Some(seq) != listed {
                    return;
                }
                let watch = watch.borrow();
                if (!app && watch.node.is_none()) || (app && watch.app_clients.is_empty()) {
                    lose();
                }
            }
        })
        .register();

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_NAME => STREAM_NAME,
        *pw::keys::APP_NAME => "RBS",
        // Scheduled every cycle even while nothing plays into a monitor or an
        // application, so those streams deliver continuous (silent) audio.
        "node.always-process" => "true",
        // Never link to another node (the default microphone) when the target is gone.
        "node.dont-fallback" => "true",
    };
    if !app {
        props.insert("target.object", target.name.as_str());
    }
    if kind == EndpointKind::OutputLoopback {
        props.insert("stream.capture.sink", "true");
    }
    let Ok(stream) = pw::stream::StreamRc::new(core.clone(), STREAM_NAME, props) else {
        return fail(&report);
    };
    // The process callback has its own listener and state, so nothing it touches is
    // shared with the main-loop callbacks below (it runs on the data thread).
    let Ok(_process) = stream
        .add_local_listener_with_user_data(capture)
        .process(|stream, capture| capture.on_process(stream))
        .register()
    else {
        return fail(&report);
    };
    let Ok(_state) = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let watch = Rc::clone(&watch);
            let core = core.clone();
            let lose = lose.clone();
            let report = report.clone();
            move |stream, (), _, new| match new {
                pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                    if app {
                        let mut watch = watch.borrow_mut();
                        if watch.own_node.is_none() {
                            watch.own_node = Some(stream.node_id());
                            watch.relink(&core);
                        }
                    }
                    report(Ok(()));
                }
                pw::stream::StreamState::Error(_) => {
                    report(Err(backend(libc::EIO)));
                    lose();
                }
                pw::stream::StreamState::Unconnected => lose(),
                pw::stream::StreamState::Connecting => {}
            }
        })
        .register()
    else {
        return fail(&report);
    };
    let Some(param) = format_param(format, &target.positions) else {
        return report(Err(AudioIoError::FormatUnsupported));
    };
    let Some(pod) = Pod::from_bytes(&param) else {
        return report(Err(AudioIoError::FormatUnsupported));
    };
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS
        | pw::stream::StreamFlags::DONT_RECONNECT;
    if !app {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    if stream
        .connect(spa::utils::Direction::Input, None, flags, &mut [pod])
        .is_err()
    {
        return fail(&report);
    }
    mainloop.run();
    let _ = stream.disconnect();
    // Links go with the proxies; drop them before the connection.
    watch.borrow_mut().links.clear();
}
