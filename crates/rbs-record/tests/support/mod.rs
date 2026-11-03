//! Reference adapters and the clock driver shared by the recording and studio tests:
//! synthetic capture and audio, a fake H.264 encoder, the Media Foundation AAC session
//! over its fake engine, and a muxer and transport that log what they get.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Each test binary uses a part.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rbs_audio::AudioGraphConfig;
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, RateControl, VideoEncoder, VideoEncoderConfig,
    VideoEncoderFactory, caps as codec_caps,
};
use rbs_codec_mf::testing::FakeAacFactory;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, FramePayload, MuxChunk,
    Size, StreamDescriptor, StreamIndex, StreamParams, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Engine, EngineConfig, Serviced};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::description::PartitionId;
use rbs_graph::node::NodeTypeId;
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory, caps as mux_caps};
use rbs_record::{
    AUDIO, AUDIO_ENCODER, OUTPUT, RENDER, RecordAdapters, Recording, Studio, VIDEO_ENCODER,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::{CanvasId, ItemContent, Layout, SceneScope, Settings};
use rbs_sources::types;
use rbs_transport::{
    SendOutcome, Transport, TransportConfig, TransportError, TransportEvent, TransportFactory,
    TransportItem, TransportStats, caps as transport_caps,
};

pub const W: u32 = 64;
pub const H: u32 = 36;
pub const FPS: FrameRate = FrameRate::FPS_30;
/// Hang guard only; the loop ends when the packet counts are reached.
pub const LIMIT: Duration = Duration::from_secs(20);
/// Clock step per loop round: half an audio block, so the audio worker never falls
/// behind its catch-up window.
pub const STEP: Duration = Duration::from_millis(5);

pub fn gpu() -> Option<Arc<GpuContext>> {
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

pub fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
}

// ---- Video encoders ------------------------------------------------------------------

/// Lists H.264 with system NV12 input; `broken` sessions fail to create (a device that
/// probes but cannot open), so the recording must fall back to the next candidate.
pub struct FakeVideoFactory {
    id: &'static str,
    broken: bool,
    submitted: Arc<AtomicU64>,
    /// The configuration of the last session created.
    configs: Arc<Mutex<Option<VideoEncoderConfig>>>,
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
        if matches!(config.rate_control, RateControl::CbrPadded { .. }) {
            return Err(CreateError::Unsupported {
                what: "cbr_padded (test)".into(),
            });
        }
        *self.configs.lock().unwrap() = Some(config.clone());
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

pub struct FakeVideo {
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
pub struct Written {
    pub stream: u16,
    pub pts: Timestamp,
    pub codec: CodecId,
}

#[derive(Default)]
pub struct Log {
    pub packets: Mutex<Vec<Written>>,
    pub headers: AtomicU64,
    pub trailers: AtomicU64,
    pub media: AtomicU64,
    pub finished: AtomicU64,
    pub streams: Mutex<Vec<StreamDescriptor>>,
}

impl Log {
    pub fn count(&self, stream: u16) -> usize {
        self.packets
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.stream == stream)
            .count()
    }
}

pub struct LogMuxerFactory(Arc<Log>);

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

pub struct LogMuxer(Arc<Log>);

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

pub struct LogTransportFactory(Arc<Log>);

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
        config: &TransportConfig,
    ) -> Result<Box<dyn Transport>, rbs_transport::CreateError> {
        // `mem://fail` is a destination that cannot be opened.
        if config.url == "mem://fail" {
            return Err(rbs_transport::CreateError::InvalidConfig(
                "cannot open (test)".into(),
            ));
        }
        Ok(Box::new(LogTransport(Arc::clone(&self.0))))
    }
}

pub struct LogTransport(Arc<Log>);

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
pub struct Rig {
    pub adapters: RecordAdapters,
    pub capture: Arc<TestCaptureFactory>,
    pub audio: Arc<TestAudioBackend>,
    pub log: Arc<Log>,
    pub submitted: Arc<AtomicU64>,
    /// The configuration of the last video encoder session created.
    pub video_config: Arc<Mutex<Option<VideoEncoderConfig>>>,
}

pub fn rig() -> Rig {
    let log = Arc::new(Log::default());
    let submitted = Arc::new(AtomicU64::new(0));
    let video_config = Arc::new(Mutex::new(None));
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
                configs: Arc::clone(&video_config),
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
        video_config,
    }
}

pub const SOURCES: [(&str, &str); 4] = [
    ("Display", types::DISPLAY_SOURCE),
    ("Camera", types::CAMERA_SOURCE),
    ("Microphone", types::AUDIO_INPUT_SOURCE),
    ("System audio", types::AUDIO_OUTPUT_SOURCE),
];

/// Display full canvas, camera in the corner, microphone, system audio, on air.
pub fn scene() -> (SceneCollection, CanvasId) {
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
pub const START: Timestamp = Timestamp::from_nanos(3_217_000_000);

/// Ticks of a cadence due by `now` after `offset`: the count of indices whose time plus
/// `offset` is at or before `now`.
pub fn due(cadence: &Cadence, offset: Duration, now: Timestamp) -> u64 {
    now.as_nanos()
        .checked_sub(offset.as_nanos() as u64)
        .and_then(|t| cadence.index_at_or_before(Timestamp::from_nanos(t)))
        .map_or(0, |i| i + 1)
}

/// What [`run_until`] drives: a recording or a studio.
pub trait Driven {
    fn engine(&self) -> &Engine;
    fn service(&mut self) -> Serviced;
    fn failure(&self) -> Option<String>;
}

impl Driven for Recording {
    fn engine(&self) -> &Engine {
        Recording::engine(self)
    }
    fn service(&mut self) -> Serviced {
        Recording::service(self)
    }
    fn failure(&self) -> Option<String> {
        Recording::failure(self)
    }
}

impl Driven for Studio {
    fn engine(&self) -> &Engine {
        Studio::engine(self)
    }
    fn service(&mut self) -> Serviced {
        Studio::service(self)
    }
    fn failure(&self) -> Option<String> {
        Studio::failure(self).or_else(|| self.recording_failure())
    }
}

/// The cadence partitions: id, cadence, offset.
fn cadences() -> [(PartitionId, Cadence, Duration); 3] {
    let frames = Cadence::frames(Timestamp::ZERO, FPS);
    [
        (RENDER, frames, Duration::ZERO),
        (VIDEO_ENCODER, frames, FPS.frame_duration() / 4),
        (
            AUDIO,
            AudioGraphConfig::default().cadence(START),
            Duration::ZERO,
        ),
    ]
}

/// Moves the clock in `STEP`s, waking every worker, until `done` or the hang guard.
/// Before `done` is first checked and after each step it waits until every cadence
/// worker that runs has reached the tick due now: its next index is past it (the tick
/// ran, was skipped late, or a drained worker's final tick already took it), so no
/// worker falls behind the test clock (one that did would skip ticks, which is right in
/// real time but not what these tests check).
pub fn run_until<T: Driven>(
    driven: &mut T,
    clock: &TestClock,
    mut done: impl FnMut(&T) -> bool,
) -> bool {
    let all = [RENDER, AUDIO, VIDEO_ENCODER, AUDIO_ENCODER, OUTPUT];
    let gpu = gpu().expect("run_until needs the GPU the studio runs on");
    let wall = Instant::now();
    let catch_up = |driven: &T| {
        let now = clock.now();
        for (p, cadence, offset) in cadences() {
            if !driven.engine().has_worker(p) {
                continue;
            }
            let stats = driven.engine().worker_stats(p).unwrap();
            let want = due(&cadence, offset, now);
            while stats.next_index() < want && wall.elapsed() < LIMIT {
                driven.engine().wake(p);
                std::thread::yield_now();
            }
        }
    };
    catch_up(driven);
    while wall.elapsed() < LIMIT {
        if done(driven) {
            return true;
        }
        clock.advance(STEP);
        for p in all {
            driven.engine().wake(p);
        }
        catch_up(driven);
        // The render worker only polls the device without blocking; software adapters
        // (WARP) may then leave readbacks pending indefinitely. Waiting here makes every
        // readback submitted by this step complete before the next render tick, as in
        // the rbs-nodes GPU pipeline tests.
        gpu.wait_idle().unwrap();
        let serviced = driven.service();
        assert!(serviced.errors.is_empty(), "{:?}", serviced.errors);
        assert_eq!(driven.failure(), None);
    }
    false
}

pub fn engine_at_start() -> (Engine, TestClock) {
    let clock = TestClock::new();
    clock.set(START);
    let engine = Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default());
    (engine, clock)
}

pub fn pts_of(log: &Log, stream: u16) -> Vec<Timestamp> {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.stream == stream)
        .map(|p| p.pts)
        .collect()
}
