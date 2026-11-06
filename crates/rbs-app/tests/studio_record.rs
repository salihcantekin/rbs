//! Recording through the studio host: encoder choice, refusals and a clean stop, on the
//! reference sources, fake encoders, a fake muxer and the file transport. The engine
//! runs on a `TestClock` that moves only to let a stop end; the test waits for the
//! host's updates, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_app::studio_host::{
    Command, Fault, HostConfig, RecordRequest, RecordingUpdate, SourceKind, SourceRow,
    SourceStatus, StudioHost, Update,
};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, RateControl, VideoEncoder, VideoEncoderConfig,
    VideoEncoderFactory, caps as codec_caps,
};
use rbs_codec_mf::testing::FakeAacFactory;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk,
    StreamDescriptor, StreamIndex, StreamParams, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory, caps as mux_caps};
use rbs_record::{END_TAIL_LIMIT, RecordAdapters, VideoSettings};

/// Hang guard only: every wait ends as soon as its update arrives.
const LIMIT: Duration = Duration::from_secs(20);

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

/// H.264 from system NV12; a `broken` one probes but cannot open a session.
struct FakeVideoFactory {
    id: &'static str,
    broken: bool,
}

fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
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
            pending: Vec::new(),
        }))
    }
}

struct FakeVideo {
    stream: StreamDescriptor,
    pending: Vec<EncodedPacket>,
}

impl VideoEncoder for FakeVideo {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        let t = frame.timestamp();
        let packet = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            t,
            t,
            true,
            Bytes::from_static(b"frame"),
        )
        .map_err(|e| EncodeError::InvalidInput(format!("{e:?}")))?;
        self.pending.push(packet);
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

/// Writes a header, the packets' bytes and a trailer.
struct FakeMuxerFactory;

impl AdapterFactory for FakeMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.muxer")
    }
    fn display_name(&self) -> &str {
        "test muxer"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(mux_caps::CONTAINER, set(&["matroska"])))
    }
}

impl MuxerFactory for FakeMuxerFactory {
    fn create(&self, _: &MuxerConfig) -> Result<Box<dyn Muxer>, rbs_mux::CreateError> {
        Ok(Box::new(FakeMuxer))
    }
}

struct FakeMuxer;

const HEADER: &[u8] = b"RBS-TEST";
const TRAILER: &[u8] = b"END";

impl Muxer for FakeMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(MuxChunk::new(ChunkKind::Header, Bytes::from_static(HEADER)));
        Ok(())
    }
    fn write(
        &mut self,
        _: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), rbs_mux::MuxError> {
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
        out.push(MuxChunk::new(
            ChunkKind::Trailer,
            Bytes::from_static(TRAILER),
        ));
        Ok(())
    }
}

const BROKEN: &str = "test.video-broken";
const WORKING: &str = "test.video-working";

fn host(gpu: Arc<GpuContext>, clock: &TestClock) -> (StudioHost, Receiver<Update>) {
    host_with(gpu, clock, None)
}

/// [`host`], panicking once at `fault` (ADR-0055).
fn host_with(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    fault: Option<Fault>,
) -> (StudioHost, Receiver<Update>) {
    let (tx, rx) = mpsc::channel();
    let clock = clock.clone();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.fault = fault;
    let host = StudioHost::spawn(
        config,
        || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources
                .register_audio(Arc::new(TestAudioBackend::new()))
                .unwrap();
            for (id, broken) in [(BROKEN, true), (WORKING, false)] {
                a.video_encoders
                    .register(Arc::new(FakeVideoFactory { id, broken }))
                    .unwrap();
            }
            a.audio_encoders
                .register(Arc::new(FakeAacFactory::new()))
                .unwrap();
            a.muxers.register(Arc::new(FakeMuxerFactory)).unwrap();
            a.transports
                .register(Arc::new(rbs_transport_file::FileTransportFactory))
                .unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        move || Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default()),
        move |u| {
            let _ = tx.send(u);
        },
    )
    .unwrap();
    (host, rx)
}

/// The next update `pick` accepts; others are skipped.
fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let u = rx.recv_timeout(left).expect("the host answered");
        if let Some(t) = pick(u) {
            return t;
        }
    }
}

/// Like [`next`], calling `step` before each short wait for an update.
fn next_while<T>(
    rx: &Receiver<Update>,
    mut step: impl FnMut(),
    mut pick: impl FnMut(Update) -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the host never answered"
        );
        step();
        if let Ok(u) = rx.recv_timeout(Duration::from_millis(10))
            && let Some(t) = pick(u)
        {
            return t;
        }
    }
}

fn recording(rx: &Receiver<Update>) -> RecordingUpdate {
    next(rx, |u| match u {
        Update::Recording(r) => Some(r),
        _ => None,
    })
}

fn rows_where(rx: &Receiver<Update>, mut ok: impl FnMut(&[SourceRow]) -> bool) -> Vec<SourceRow> {
    next(rx, |u| match u {
        Update::Sources(rows) if ok(&rows) => Some(rows),
        _ => None,
    })
}

fn request(path: PathBuf, encoder: Option<&str>) -> RecordRequest {
    let mut video = VideoSettings::default();
    video.rate_control = RateControl::Cbr {
        bitrate_bps: 6_000_000,
    };
    RecordRequest {
        path,
        container: ContainerId::Matroska,
        encoder: encoder.map(str::to_owned),
        video,
        audio_bitrate_bps: 128_000,
    }
}

#[test]
fn recordings_start_with_the_picked_encoder_and_stop_into_a_closed_file() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = host(gpu, &clock);
    let (encoders, cannot) = next(&rx, |u| match u {
        Update::Started {
            encoders,
            cannot_record,
            ..
        } => Some((encoders, cannot_record)),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(cannot, None);
    let ids: Vec<&str> = encoders.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&BROKEN) && ids.contains(&WORKING), "{ids:?}");

    // A display: closed while nothing shows or records it.
    host.send(Command::ListTargets(SourceKind::Screen));
    let display = next(&rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: display,
    });
    rows_where(&rx, |r| r.len() == 1 && r[0].status == SourceStatus::Idle);

    // The picked encoder is the only one tried: it cannot open, so nothing records and
    // no file is left behind.
    let first = dir.join("sub").join("first.mkv");
    host.send(Command::StartRecording(request(
        first.clone(),
        Some(BROKEN),
    )));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains(BROKEN), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(!first.exists());

    // An encoder this system does not have.
    host.send(Command::StartRecording(request(
        first.clone(),
        Some("nope"),
    )));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains("not available"), "{why}"),
        other => panic!("{other:?}"),
    }

    // Automatic: the broken one is passed over. The folder is made; the display opens
    // because the recording shows it.
    host.send(Command::StartRecording(request(first.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::Started {
            path,
            video_encoder,
            audio_encoder,
        } => {
            assert_eq!(path, first);
            assert_eq!(video_encoder, WORKING);
            assert!(audio_encoder.is_some());
        }
        other => panic!("{other:?}"),
    }
    rows_where(&rx, |r| {
        r.len() == 1 && r[0].status == SourceStatus::Running
    });

    // One recording at a time.
    host.send(Command::StartRecording(request(
        dir.join("second.mkv"),
        None,
    )));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains("already"), "{why}"),
        other => panic!("{other:?}"),
    }

    // Stop ends the video and waits for its tail (ADR-0045). Nothing renders on the
    // still clock, so the host stops once `END_TAIL_LIMIT` has passed on it: the test
    // moves the clock until the answer comes (it cannot know when the host ended).
    host.send(Command::StopRecording);
    let stopped = next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(r) => Some(r),
            _ => None,
        },
    );
    match stopped {
        RecordingUpdate::Stopped { path, failure, .. } => {
            assert_eq!(path, first);
            assert_eq!(failure, None);
        }
        other => panic!("{other:?}"),
    }
    // The output wrote the header and, on stop, the trailer, and closed the file.
    let written = std::fs::read(&first).unwrap();
    assert!(written.starts_with(HEADER), "{written:?}");
    assert!(written.ends_with(TRAILER), "{written:?}");
    rows_where(&rx, |r| r.len() == 1 && r[0].status == SourceStatus::Idle);

    // An existing file is never replaced.
    host.send(Command::StartRecording(request(first.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains("already exists"), "{why}"),
        other => panic!("{other:?}"),
    }

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A started host with one display source, ready to record.
fn ready(gpu: Arc<GpuContext>, clock: &TestClock, fault: Fault) -> (StudioHost, Receiver<Update>) {
    let (host, rx) = host_with(gpu, clock, Some(fault));
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::ListTargets(SourceKind::Screen));
    let display = next(&rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: display,
    });
    rows_where(&rx, |r| r.len() == 1);
    (host, rx)
}

/// Stops the recording and returns how it ended, moving the clock so its tail ends.
fn stop_recording(host: &StudioHost, rx: &Receiver<Update>, clock: &TestClock) -> RecordingUpdate {
    host.send(Command::StopRecording);
    next_while(
        rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(r) => Some(r),
            Update::Failed(e) => panic!("the studio failed: {e}"),
            _ => None,
        },
    )
}

#[test]
fn a_start_that_panics_after_the_output_started_stops_that_output() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-fault-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = ready(gpu, &clock, Fault::AfterOutputStart);

    // The start panics once the output runs: the UI gets its answer, and the output
    // the host never tracked is stopped.
    let lost = dir.join("lost.mkv");
    host.send(Command::StartRecording(request(lost.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert_eq!(why, rbs_crash::CAUGHT_TEXT),
        other => panic!("{other:?}"),
    }
    let stopped = next(&rx, |u| match u {
        Update::Recovered { stopped } => Some(stopped),
        _ => None,
    });
    assert_eq!(stopped, 1);
    // Nothing counts as running: the next recording starts and stops normally.
    let next_file = dir.join("next.mkv");
    host.send(Command::StartRecording(request(next_file.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert_eq!(path, next_file),
        other => panic!("{other:?}"),
    }
    match stop_recording(&host, &rx, &clock) {
        RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None),
        other => panic!("{other:?}"),
    }
    assert!(std::fs::read(&next_file).unwrap().ends_with(TRAILER));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stop_that_panics_half_way_still_ends_the_recording() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-fault-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = ready(gpu, &clock, Fault::InRecordingStop);

    let file = dir.join("take.mkv");
    host.send(Command::StartRecording(request(file.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::Started { .. } => {}
        other => panic!("{other:?}"),
    }
    // The stop panics after the output stopped: recovery ends the recording, says why,
    // and the file is complete.
    match stop_recording(&host, &rx, &clock) {
        RecordingUpdate::Stopped { path, failure, .. } => {
            assert_eq!(path, file);
            assert!(failure.is_some_and(|f| f.contains("internal error")));
        }
        other => panic!("{other:?}"),
    }
    assert!(std::fs::read(&file).unwrap().ends_with(TRAILER));

    // The host does not think a recording still runs.
    let again = dir.join("again.mkv");
    host.send(Command::StartRecording(request(again.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert_eq!(path, again),
        other => panic!("{other:?}"),
    }
    let _ = stop_recording(&host, &rx, &clock);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}
