//! A whole recording on the reference adapters (spec `recording.md`, "Conformance
//! tests"): synthetic display, camera, microphone and loopback, a fake H.264 encoder,
//! the Media Foundation AAC session over its fake engine, and a muxer and transport
//! that log what they get. The engine runs on a `TestClock`; the test moves it and
//! wakes the workers, and waits for packet counts, never for a duration (ADR-0032).
//! Needs a GPU adapter (software is fine: lavapipe, WARP); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rbs_audio::AudioGraphConfig;
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory,
    caps as codec_caps,
};
use rbs_codec_mf::testing::FakeAacFactory;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, FramePayload, MuxChunk,
    Size, StreamDescriptor, StreamIndex, StreamParams, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate, SampleRate};
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::description::PartitionId;
use rbs_graph::node::NodeTypeId;
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory, caps as mux_caps};
use rbs_record::{
    AUDIO, AUDIO_ENCODER, OUTPUT, Port, RENDER, RecordAdapters, RecordError, RecordSettings,
    Recording, VIDEO_ENCODER,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::{CanvasId, ItemContent, Layout, SceneScope, Settings};
use rbs_sources::types;
use rbs_transport::{
    SendOutcome, Transport, TransportConfig, TransportError, TransportEvent, TransportFactory,
    TransportItem, TransportStats, caps as transport_caps,
};

const W: u32 = 64;
const H: u32 = 36;
const FPS: FrameRate = FrameRate::FPS_30;
/// Hang guard only; the loop ends when the packet counts are reached.
const LIMIT: Duration = Duration::from_secs(20);
/// Clock step per loop round: half an audio block, so the audio worker never falls
/// behind its catch-up window.
const STEP: Duration = Duration::from_millis(5);

fn gpu() -> Option<Arc<GpuContext>> {
    // One device per test binary: software Vulkan drivers crash with several.
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("{e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
}

// ---- Video encoders ------------------------------------------------------------------

/// Lists H.264 with system NV12 input; `broken` sessions fail to create (a device that
/// probes but cannot open), so the recording must fall back to the next candidate.
struct FakeVideoFactory {
    id: &'static str,
    broken: bool,
    submitted: Arc<AtomicU64>,
}

impl AdapterFactory for FakeVideoFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(self.id)
    }
    fn display_name(&self) -> &str {
        self.id
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(codec_caps::VIDEO_ENCODE, set(&["h264"]))
            .with(codec_caps::INPUT_MEMORY, set(&["system"]))
            .with(codec_caps::INPUT_PIXEL_FORMAT, set(&["nv12"])))
    }
}

impl VideoEncoderFactory for FakeVideoFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        if self.broken {
            return Err(CreateError::Unsupported {
                what: "no device (test)".into(),
            });
        }
        Ok(Box::new(FakeVideo {
            stream: StreamDescriptor::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                Bytes::from_static(b"sps-pps"),
                StreamParams::video(config.format, config.frame_rate),
            ),
            format: config.format,
            pending: Vec::with_capacity(64),
            count: 0,
            submitted: Arc::clone(&self.submitted),
        }))
    }
}

struct FakeVideo {
    stream: StreamDescriptor,
    format: rbs_core::media::VideoFormat,
    pending: Vec<EncodedPacket>,
    count: u64,
    submitted: Arc<AtomicU64>,
}

impl VideoEncoder for FakeVideo {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        if !matches!(frame.payload(), FramePayload::System(_)) || *frame.format() != self.format {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        let t = frame.timestamp();
        let packet = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            t,
            t,
            self.count.is_multiple_of(30),
            Bytes::from_static(b"frame"),
        )
        .unwrap();
        self.pending.push(packet);
        self.count += 1;
        self.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.pending);
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.drain(out)
    }
}

// ---- Muxer and transport -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct Written {
    stream: u16,
    pts: Timestamp,
    codec: CodecId,
}

#[derive(Default)]
struct Log {
    packets: Mutex<Vec<Written>>,
    headers: AtomicU64,
    trailers: AtomicU64,
    media: AtomicU64,
    finished: AtomicU64,
    streams: Mutex<Vec<StreamDescriptor>>,
}

impl Log {
    fn count(&self, stream: u16) -> usize {
        self.packets
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.stream == stream)
            .count()
    }
}

struct LogMuxerFactory(Arc<Log>);

impl AdapterFactory for LogMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("org.rbs.test.log-muxer")
    }
    fn display_name(&self) -> &str {
        "log muxer"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(mux_caps::CONTAINER, set(&["matroska"])))
    }
}

impl MuxerFactory for LogMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, rbs_mux::CreateError> {
        *self.0.streams.lock().unwrap() = config.streams.clone();
        Ok(Box::new(LogMuxer(Arc::clone(&self.0))))
    }
}

struct LogMuxer(Arc<Log>);

impl Muxer for LogMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(MuxChunk::new(ChunkKind::Header, Bytes::from_static(b"h")));
        Ok(())
    }
    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), rbs_mux::MuxError> {
        self.0.packets.lock().unwrap().push(Written {
            stream: stream.0,
            pts: packet.pts(),
            codec: packet.codec(),
        });
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe: packet.keyframe(),
                timestamp: packet.dts(),
            },
            packet.data().clone(),
        ));
        Ok(())
    }
    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(MuxChunk::new(ChunkKind::Trailer, Bytes::from_static(b"t")));
        Ok(())
    }
}

struct LogTransportFactory(Arc<Log>);

impl AdapterFactory for LogTransportFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("org.rbs.test.log-transport")
    }
    fn display_name(&self) -> &str {
        "log transport"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(transport_caps::SCHEMES, set(&["mem"]))
            .with(
                transport_caps::INPUT,
                set(&[ContainerId::Matroska.transport_input()]),
            ))
    }
}

impl TransportFactory for LogTransportFactory {
    fn create(
        &self,
        _: &TransportConfig,
    ) -> Result<Box<dyn Transport>, rbs_transport::CreateError> {
        Ok(Box::new(LogTransport(Arc::clone(&self.0))))
    }
}

struct LogTransport(Arc<Log>);

impl Transport for LogTransport {
    fn start(&mut self, _: &[StreamDescriptor]) -> Result<(), TransportError> {
        Ok(())
    }
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        if let TransportItem::Chunk(chunk) = item {
            let counter = match chunk.kind {
                ChunkKind::Header => &self.0.headers,
                ChunkKind::Trailer => &self.0.trailers,
                _ => &self.0.media,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Ok(SendOutcome::Queued)
    }
    fn poll_events(&mut self, _: &mut Vec<TransportEvent>) {}
    fn stats(&self) -> TransportStats {
        TransportStats::default()
    }
    fn finish(&mut self, _: Duration) -> Result<(), TransportError> {
        self.0.finished.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

// ---- Rig -----------------------------------------------------------------------------

/// The reference adapters, kept so a test can count what they opened.
struct Rig {
    adapters: RecordAdapters,
    capture: Arc<TestCaptureFactory>,
    audio: Arc<TestAudioBackend>,
    log: Arc<Log>,
    submitted: Arc<AtomicU64>,
}

fn rig() -> Rig {
    let log = Arc::new(Log::default());
    let submitted = Arc::new(AtomicU64::new(0));
    let capture = Arc::new(TestCaptureFactory::new());
    let audio = Arc::new(TestAudioBackend::new());
    let mut a = RecordAdapters::new();
    a.sources.register_capture(capture.clone()).unwrap();
    a.sources.register_audio(audio.clone()).unwrap();
    for (id, broken) in [("org.rbs.test.broken", true), ("org.rbs.test.h264", false)] {
        a.video_encoders
            .register(Arc::new(FakeVideoFactory {
                id,
                broken,
                submitted: Arc::clone(&submitted),
            }))
            .unwrap();
    }
    a.audio_encoders
        .register(Arc::new(FakeAacFactory::new()))
        .unwrap();
    a.muxers
        .register(Arc::new(LogMuxerFactory(Arc::clone(&log))))
        .unwrap();
    a.transports
        .register(Arc::new(LogTransportFactory(Arc::clone(&log))))
        .unwrap();
    a.probe();
    Rig {
        adapters: a,
        capture,
        audio,
        log,
        submitted,
    }
}

const SOURCES: [(&str, &str); 4] = [
    ("Display", types::DISPLAY_SOURCE),
    ("Camera", types::CAMERA_SOURCE),
    ("Microphone", types::AUDIO_INPUT_SOURCE),
    ("System audio", types::AUDIO_OUTPUT_SOURCE),
];

/// Display full canvas, camera in the corner, microphone, system audio, on air.
fn scene() -> (SceneCollection, CanvasId) {
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    for (name, t) in SOURCES {
        let layout = match t {
            types::DISPLAY_SOURCE => Layout::new([0.0, 0.0], [W as f32, H as f32]),
            types::CAMERA_SOURCE => Layout::new([40.0, 22.0], [20.0, 11.0]),
            _ => Layout::new([0.0, 0.0], [1.0, 1.0]),
        };
        let (id, _) = col
            .add_source(name, NodeTypeId::new_static(t), Settings::new())
            .unwrap();
        col.add_item(scene, ItemContent::Source(id), layout, None)
            .unwrap();
    }
    col.set_program(canvas, Some(scene)).unwrap();
    (col, canvas)
}

/// The engine time when the recording starts: off the frame and block grids, so the
/// test sees the real offsets between the cadences.
const START: Timestamp = Timestamp::from_nanos(3_217_000_000);

/// Ticks of a cadence due by `now` after `offset`: the count of indices whose time plus
/// `offset` is at or before `now`.
fn due(cadence: &Cadence, offset: Duration, now: Timestamp) -> u64 {
    now.as_nanos()
        .checked_sub(offset.as_nanos() as u64)
        .and_then(|t| cadence.index_at_or_before(Timestamp::from_nanos(t)))
        .map_or(0, |i| i + 1)
}

/// Moves the clock in `STEP`s, waking every worker, until `done` or the hang guard.
/// After each step it waits until every cadence worker has run (or skipped) the tick
/// due at the start and each one due since, so no worker falls behind the test clock (one that
/// did would skip ticks, which is right in real time but not what this test checks).
fn run_until(
    recording: &mut Recording,
    clock: &TestClock,
    audio: bool,
    mut done: impl FnMut(&Recording) -> bool,
) -> bool {
    let frames = Cadence::frames(Timestamp::ZERO, FPS);
    let mut cadences = vec![
        (RENDER, frames, Duration::ZERO),
        (VIDEO_ENCODER, frames, FPS.frame_duration() / 4),
    ];
    if audio {
        cadences.push((
            AUDIO,
            AudioGraphConfig::default().cadence(START),
            Duration::ZERO,
        ));
    }
    let partitions: &[PartitionId] = if audio {
        &[RENDER, AUDIO, VIDEO_ENCODER, AUDIO_ENCODER, OUTPUT]
    } else {
        &[RENDER, VIDEO_ENCODER, OUTPUT]
    };
    let start = clock.now();
    let wall = Instant::now();
    // Each worker first runs the tick already due at the start, before the clock moves
    // (a worker that started after the next tick was due would skip it).
    for (p, _, _) in &cadences {
        let stats = recording.engine().worker_stats(*p).unwrap();
        while stats.ticks() == 0 && wall.elapsed() < LIMIT {
            recording.engine().wake(*p);
            std::thread::yield_now();
        }
    }
    while wall.elapsed() < LIMIT {
        if done(recording) {
            return true;
        }
        clock.advance(STEP);
        let now = clock.now();
        for p in partitions {
            recording.engine().wake(*p);
        }
        for (p, cadence, offset) in &cadences {
            let stats = recording.engine().worker_stats(*p).unwrap();
            let want = due(cadence, *offset, now) - due(cadence, *offset, start) + 1;
            while stats.ticks() + stats.late() < want && wall.elapsed() < LIMIT {
                recording.engine().wake(*p);
                std::thread::yield_now();
            }
        }
        let serviced = recording.service();
        assert!(serviced.errors.is_empty(), "{:?}", serviced.errors);
        assert_eq!(recording.failure(), None);
    }
    false
}

fn engine_at_start() -> (Engine, TestClock) {
    let clock = TestClock::new();
    clock.set(START);
    let engine = Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default());
    (engine, clock)
}

fn pts_of(log: &Log, stream: u16) -> Vec<Timestamp> {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.stream == stream)
        .map(|p| p.pts)
        .collect()
}

#[test]
fn a_scene_records_video_and_audio_in_sync_on_the_master_clock() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let log = &r.log;
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://recording", ContainerId::Matroska);
    let mut recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();

    let started = recording.started().clone();
    assert_eq!(started.video_encoder.adapter.as_str(), "org.rbs.test.h264");
    assert_eq!(
        started.audio_encoder.as_ref().unwrap().adapter.as_str(),
        "org.rbs.codec.mf-aac.fake"
    );
    let names: Vec<&str> = started.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(
        started.sources.iter().all(|s| s.failed.is_none()),
        "{started:?}"
    );
    assert!(started.problems.is_empty(), "{started:?}");

    let reached = run_until(&mut recording, &clock, true, |_| {
        log.count(0) >= 12 && log.count(1) >= 20
    });
    assert!(reached, "packets: {:?}", recording.progress());
    let end = clock.now();
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    let p = &report.progress;
    assert_eq!(p.mux_drops, 0);
    assert_eq!(p.audio_queue_drops, 0);
    assert_eq!(p.audio_packet_drops, 0);
    assert_eq!(p.late_audio_ticks, 0);
    assert_eq!(p.late_render_ticks, 0);
    for s in &p.sources {
        assert!(s.frames > 0, "{} delivered nothing: {report:?}", s.name);
    }

    // Streams: video then audio, as the output declared them.
    let streams = log.streams.lock().unwrap().clone();
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].codec, CodecId::H264);
    assert_eq!(streams[1].codec, CodecId::Aac);
    assert_eq!(log.headers.load(Ordering::Relaxed), 1);
    assert_eq!(log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(log.finished.load(Ordering::Relaxed), 1);
    assert!(
        log.packets
            .lock()
            .unwrap()
            .iter()
            .all(|p| (p.stream == 0) == (p.codec == CodecId::H264))
    );
    let video = pts_of(log, 0);
    let audio = pts_of(log, 1);

    // Video: master time on the canvas cadence (origin 0), strictly increasing, from
    // the first render tick after the start on (the first frame needs a capture and a
    // readback, a few ticks).
    let frames = Cadence::frames(Timestamp::ZERO, FPS);
    for pair in video.windows(2) {
        assert!(pair[0] < pair[1], "video pts not increasing: {pair:?}");
    }
    for t in &video {
        let i = frames.index_at_or_before(*t).unwrap();
        assert_eq!(frames.time_of(i), *t, "video pts off the canvas cadence");
    }
    let first_tick = frames.time_of(due(&frames, Duration::ZERO, START) - 1);
    assert!(
        video[0] >= first_tick && video[0] <= first_tick + FPS.frame_duration() * 4,
        "first video pts {:?}, first render tick {first_tick:?}",
        video[0]
    );

    // Audio: the first packet is the first mixed block, which carries the time of its
    // first sample, the tick time minus the graph latency (ADR-0037), not the tick time
    // itself. The audio cadence starts at `START`, where the worker runs its first tick;
    // the input may need one more block before it delivers.
    let graph = settings.audio_graph;
    let ticks = graph.cadence(START);
    let first_tick = audio[0] + graph.latency;
    assert!(
        first_tick == ticks.time_of(0) || first_tick == ticks.time_of(1),
        "first audio pts {:?} is not a block time of the first audio ticks",
        audio[0]
    );
    let aac = SampleRate::HZ_48000.duration_of(1024);
    for pair in audio.windows(2) {
        let step = pair[1].saturating_since(pair[0]);
        assert!(
            step.abs_diff(aac) <= Duration::from_micros(1),
            "audio gap or overlap: {pair:?}"
        );
    }
    assert!(*video.last().unwrap() <= end && *audio.last().unwrap() <= end);
    assert!(r.submitted.load(Ordering::Relaxed) >= 12);
}

#[test]
fn without_audio_only_video_is_recorded_and_audio_sources_stay_closed() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut settings = RecordSettings::new(canvas, "mem://video", ContainerId::Matroska);
    settings.audio = None;
    let mut recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();
    let started = recording.started().clone();
    let names: Vec<&str> = started.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Display", "Camera"]);
    assert_eq!(started.audio_encoder, None);
    assert_eq!(r.audio.active_streams(), 0, "no audio stream opened");
    assert!(!recording.engine().has_worker(AUDIO));
    let reached = run_until(&mut recording, &clock, false, |_| r.log.count(0) >= 5);
    assert!(reached, "{:?}", recording.progress());
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.log.streams.lock().unwrap().len(), 1);
    assert_eq!(r.log.count(1), 0);
}

#[test]
fn problems_outside_the_recorded_scene_do_not_stop_it() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    // An unknown plugin source in a scene that is not on air.
    let (idle, _) = col.add_scene("Idle", SceneScope::Shared).unwrap();
    let (plugin, _) = col
        .add_source(
            "Plugin",
            NodeTypeId::new_static("com.example.plugin"),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        idle,
        ItemContent::Source(plugin),
        Layout::new([0.0, 0.0], [8.0, 8.0]),
        None,
    )
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Matroska);
    let recording =
        Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap();
    assert_eq!(recording.started().problems.len(), 1);
    assert_eq!(recording.started().sources.len(), 4);
    let _ = recording.finish();

    // The same source on air stops the start before anything opens.
    let r = rig();
    let program = col.canvas(canvas).unwrap().program.unwrap();
    col.add_item(
        program,
        ItemContent::Source(plugin),
        Layout::new([0.0, 0.0], [8.0, 8.0]),
        None,
    )
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let err = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap_err();
    assert!(matches!(err, RecordError::Problems(_)), "{err}");
    assert_eq!(r.log.headers.load(Ordering::Relaxed), 0);
    assert_eq!(r.capture.live_buffers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}

#[test]
fn an_unknown_canvas_or_a_missing_encoder_fails_before_anything_runs() {
    let Some(gpu) = gpu() else { return };
    let (col, canvas) = scene();

    // An empty video encoder registry.
    let mut r = rig();
    r.adapters.video_encoders = rbs_core::registry::Registry::new();
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Matroska);
    let err = Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap_err();
    assert!(
        matches!(
            err,
            RecordError::NoAdapter {
                port: Port::VideoEncoder,
                ..
            }
        ),
        "{err}"
    );

    // A container no muxer lists.
    let r = rig();
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Mp4Fragmented);
    let err = Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap_err();
    assert!(
        matches!(
            err,
            RecordError::NoAdapter {
                port: Port::Muxer,
                ..
            }
        ),
        "{err}"
    );

    // A canvas that is not in the collection.
    let (other, _) = {
        let mut c = SceneCollection::new();
        c.add_canvas("A", Size::new(W, H), FPS).unwrap();
        c.add_canvas("B", Size::new(W, H), FPS).unwrap()
    };
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(other, "mem://x", ContainerId::Matroska);
    let err = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap_err();
    assert!(matches!(err, RecordError::UnknownCanvas(_)), "{err}");
    assert_eq!(
        r.log.headers.load(Ordering::Relaxed),
        0,
        "nothing was opened"
    );
    assert_eq!(r.capture.live_buffers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}
