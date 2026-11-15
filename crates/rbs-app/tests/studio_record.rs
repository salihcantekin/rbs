//! Recording through the studio host: encoder choice, refusals and a clean stop, on the
//! reference sources, fake encoders, a fake muxer and the file transport. The engine
//! runs on a `TestClock` that moves only to let a stop end; the test waits for the
//! host's updates, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::edit_history::EditKind;
use rbs_app::studio_host::protection::SystemProbeFactory;
use rbs_app::studio_host::{
    Command, CropRegion, Fault, HostConfig, RecordPatch, RecordRequest, RecordingUpdate,
    SourceKind, SourceRow, SourceStatus, StudioHost, Update,
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
use rbs_record::protection::{Probed, SystemProbe, VolumeKey, VolumeSpace};
use rbs_record::{END_TAIL_LIMIT, RecordAdapters, VideoSettings};
use rbs_scene::model::VideoAs;

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
    /// Declares itself a hardware encoder (ranked before the others).
    hardware: bool,
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
        let caps = CapabilitySet::new()
            .with(codec_caps::VIDEO_ENCODE, set(&["h264"]))
            .with(codec_caps::INPUT_MEMORY, set(&["system"]))
            .with(codec_caps::INPUT_PIXEL_FORMAT, set(&["nv12"]));
        Ok(if self.hardware {
            caps.with(codec_caps::HARDWARE, CapabilityValue::Flag(true))
        } else {
            caps
        })
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
    let mut config = HostConfig::desktop();
    config.fault = fault;
    spawn_host(gpu, clock, config)
}

/// The test host with `config` (its preview is turned off).
fn spawn_host(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    config: HostConfig,
) -> (StudioHost, Receiver<Update>) {
    spawn_host_hw(gpu, clock, config, false)
}

/// [`spawn_host`], the broken encoder declaring itself a hardware one when
/// `broken_hardware`: a separate file then never falls to the working (software) one.
fn spawn_host_hw(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    mut config: HostConfig,
    broken_hardware: bool,
) -> (StudioHost, Receiver<Update>) {
    let (tx, rx) = mpsc::channel();
    let clock = clock.clone();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        move || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources
                .register_audio(Arc::new(TestAudioBackend::new()))
                .unwrap();
            for (id, broken) in [(BROKEN, true), (WORKING, false)] {
                a.video_encoders
                    .register(Arc::new(FakeVideoFactory {
                        id,
                        broken,
                        hardware: broken && broken_hardware,
                    }))
                    .unwrap();
            }
            a.audio_encoders
                .register(Arc::new(FakeAacFactory::new()))
                .unwrap();
            a.muxers.register(Arc::new(FakeMuxerFactory)).unwrap();
            // Separate audio files are 24-bit PCM in WAV (ADR-0086).
            a.audio_encoders
                .register(Arc::new(rbs_codec_pcm::PcmEncoderFactory))
                .unwrap();
            a.muxers
                .register(Arc::new(rbs_mux_wav::WavMuxerFactory))
                .unwrap();
            a.transports
                .register(Arc::new(rbs_transport_file::FileTransportFactory))
                .unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        move || {
            Engine::with_clock(
                Arc::new(clock.clone()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
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
        video: Some(video),
        audio_bitrate_bps: 128_000,
        bit_depth: None,
        size: None,
        iso: false,
        editor_project: None,
        markers: Default::default(),
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
            separate,
        } => {
            assert!(separate.is_empty());
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

#[test]
fn a_crop_keeps_the_scale_is_one_undo_step_and_regions_fit_the_canvas() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-crop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    // A recording keeps the display running, so its capture reports a picture size.
    let (host, rx) = host(gpu, &clock);
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
    host.send(Command::StartRecording(request(dir.join("crop.mkv"), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    // The picture size arrives once the capture opened (ADR-0058).
    let rows = rows_where(&rx, |r| r.len() == 1 && r[0].picture.is_some());
    let (id, (w, h)) = (rows[0].id, rows[0].picture.unwrap());
    let before = rows[0].layout.unwrap();
    let scale = before.size[0] / w as f32;

    // The right half cut: half as wide, same scale, the left edge stays.
    let right = w - w / 2;
    host.send(Command::SetSourceCrop {
        seq: 1,
        gesture: 0,
        id,
        crop: [0, 0, right, 0],
        keep_scale: true,
        canvas: rbs_app::studio_host::OutputCanvas::Main,
    });
    let rows = rows_where(&rx, |r| r[0].layout.is_some_and(|l| l.crop.right == right));
    let cut = rows[0].layout.unwrap();
    assert!(
        (cut.size[0] - (w / 2) as f32 * scale).abs() < 0.01,
        "{cut:?}"
    );
    assert_eq!(cut.size[1], before.size[1]);
    assert_eq!(cut.position, before.position);
    // The rows, then the undo history, then the answer.
    let undo = next(&rx, |u| match u {
        Update::History { undo, .. } => Some(undo),
        _ => None,
    });
    assert_eq!(undo.map(|s| s.kind), Some(EditKind::Crop));
    assert!(next(&rx, |u| match u {
        Update::CropPlaced { seq: 1, ok } => Some(ok),
        _ => None,
    }));

    // A crop that leaves nothing is refused and answered.
    host.send(Command::SetSourceCrop {
        seq: 2,
        gesture: 0,
        id,
        crop: [w / 2, 0, w - w / 2, 0],
        keep_scale: true,
        canvas: rbs_app::studio_host::OutputCanvas::Main,
    });
    next(&rx, |u| match u {
        Update::Error(_) => Some(()),
        Update::Sources(r) => panic!("a refused crop changed the rows: {r:?}"),
        _ => None,
    });
    assert!(!next(&rx, |u| match u {
        Update::CropPlaced { seq: 2, ok } => Some(ok),
        _ => None,
    }));

    // Undo brings the whole picture and its box back.
    host.send(Command::Undo);
    let rows = rows_where(&rx, |r| r[0].layout.is_some_and(|l| l.crop.right == 0));
    assert_eq!(rows[0].layout.unwrap(), before);

    // The right half, fitted to the canvas.
    host.send(Command::SetSourceRegion {
        id,
        region: CropRegion::RightHalf,
    });
    let rows = rows_where(&rx, |r| r[0].layout.is_some_and(|l| l.crop.left == w / 2));
    let half = rows[0].layout.unwrap();
    let (cw, ch) = (1920.0, 1080.0);
    let (hw, hh) = ((w - w / 2) as f32, h as f32);
    let k = (cw / hw).min(ch / hh);
    assert!((half.size[0] - hw * k).abs() < 0.01, "{half:?}");
    assert!((half.size[1] - hh * k).abs() < 0.01, "{half:?}");
    assert!(
        (half.position[0] - (cw - hw * k) / 2.0).abs() < 0.01,
        "{half:?}"
    );
    stop_recording(&host, &rx, &clock);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0086: a microphone marked to record separately gets its own WAV next to the
/// master, in one folder with the manifest; `iso: false` and an unmarked collection
/// record one file as before, and the mark is one undo step.
#[test]
fn a_marked_microphone_records_its_own_file_next_to_the_master() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-iso-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = host(gpu, &clock);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    for kind in [SourceKind::Screen, SourceKind::AudioInput] {
        host.send(Command::ListTargets(kind));
        let target = next(&rx, |u| match u {
            Update::Targets(k, t) if k == kind => Some(t[0].clone()),
            _ => None,
        });
        host.send(Command::AddSource { kind, target });
    }
    let rows = rows_where(&rx, |r| r.len() == 2);
    let mic = rows
        .iter()
        .find(|r| r.kind == Some(SourceKind::AudioInput))
        .unwrap()
        .clone();
    assert!(rows.iter().all(|r| r.record_separately.is_none()));

    // Nothing marked: one file, as before.
    let plain = dir.join("plain.mkv");
    host.send(Command::StartRecording(request(plain.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    stop_recording(&host, &rx, &clock);
    assert!(plain.is_file());
    assert!(!dir.join("plain").exists());

    host.send(Command::SetRecordSeparately {
        id: mic.id,
        patch: RecordPatch::enabled(true),
    });
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == mic.id && r.record_separately.audio)
    });

    // Marked, `iso` off (as `record.start_to_file` and `iso: false` ask): still one file.
    let single = dir.join("single.mkv");
    host.send(Command::StartRecording(request(single.clone(), None)));
    match recording(&rx) {
        RecordingUpdate::Started { separate, .. } => assert!(separate.is_empty()),
        other => panic!("{other:?}"),
    }
    stop_recording(&host, &rx, &clock);
    assert!(single.is_file());

    // Marked and `iso` on: a folder with the master, the microphone and the manifest.
    let take = dir.join("take.mkv");
    let folder = dir.join("take");
    let mut with_tracks = request(take.clone(), None);
    with_tracks.iso = true;
    host.send(Command::StartRecording(with_tracks.clone()));
    match recording(&rx) {
        RecordingUpdate::Started { path, separate, .. } => {
            assert_eq!(path, folder.join("master.mkv"));
            // The Outputs card names the sources written as files of their own.
            assert_eq!(separate, vec![format!("{} (sound)", mic.name)]);
        }
        other => panic!("{other:?}"),
    }
    let manifest = std::fs::read_to_string(folder.join("recording.rbs.toml")).unwrap();
    assert!(
        manifest.contains("kind = \"recording-manifest\""),
        "{manifest}"
    );
    assert!(manifest.contains("name = \"master.mkv\""), "{manifest}");
    assert!(
        manifest.contains(&format!("source_name = \"{}\"", mic.name)),
        "{manifest}"
    );
    // The file is named after the source, made safe for the file system.
    let wavs: Vec<PathBuf> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    assert_eq!(wavs.len(), 1, "{wavs:?}");
    let track = wavs[0].clone();
    let track_name = track.file_name().unwrap().to_str().unwrap();
    assert!(
        manifest.contains(&format!("name = \"{track_name}\"")),
        "{manifest}"
    );
    assert!(
        !manifest.contains(&dir.display().to_string()),
        "no path: {manifest}"
    );
    host.send(Command::StopRecording);
    let stopped = next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(r) => Some(r),
            _ => None,
        },
    );
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let wav = std::fs::read(&track).unwrap();
    assert!(wav_data_len(&wav) > 0, "the WAV holds sound");
    assert!(
        wav.starts_with(b"RIFF") || wav.starts_with(b"RF64"),
        "{:?}",
        &wav[..wav.len().min(8)]
    );
    let master = std::fs::read(folder.join("master.mkv")).unwrap();
    assert!(master.starts_with(HEADER) && master.ends_with(TRAILER));
    // The same name again: the folder exists, so nothing starts and nothing is replaced.
    host.send(Command::StartRecording(with_tracks));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains("already exists"), "{why}"),
        other => panic!("{other:?}"),
    }

    // The mark is one undo step.
    host.send(Command::Undo);
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == mic.id && r.record_separately.is_none())
    });

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0086 step 3.5: a screen marked to record separately gets its own video file
/// next to the master, in the recording's container, listed in the manifest; a
/// microphone cannot be marked for a picture, nor a screen as images; ambiguous or
/// unsaved patches are refused.
#[test]
fn a_marked_screen_records_its_own_video_file_next_to_the_master() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-iso-video-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, display, mic) = marked(gpu, &clock, None);

    for (id, patch) in [
        (
            mic.id,
            RecordPatch {
                video: Some(true),
                ..RecordPatch::default()
            },
        ),
        (
            display.id,
            RecordPatch {
                video: Some(true),
                video_as: Some(VideoAs::Image),
                ..RecordPatch::default()
            },
        ),
        // `enabled` stands for the picture here: both at once are refused.
        (
            display.id,
            RecordPatch {
                enabled: Some(true),
                video: Some(false),
                ..RecordPatch::default()
            },
        ),
        // A form for a picture that is not marked would not be saved.
        (
            display.id,
            RecordPatch {
                video_as: Some(VideoAs::Video),
                ..RecordPatch::default()
            },
        ),
    ] {
        host.send(Command::SetRecordSeparately { id, patch });
        next(&rx, |u| match u {
            Update::Error(why) => Some(why),
            Update::Sources(r) => panic!("a refused mark changed the rows: {r:?}"),
            _ => None,
        });
    }
    host.send(Command::SetRecordSeparately {
        id: display.id,
        patch: RecordPatch::enabled(true),
    });
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == display.id && r.record_separately.video)
    });

    let folder = dir.join("take");
    host.send(Command::StartRecording(with_tracks(
        dir.join("take.mkv"),
        None,
    )));
    match recording(&rx) {
        RecordingUpdate::Started { path, separate, .. } => {
            assert_eq!(path, folder.join("master.mkv"));
            assert_eq!(separate.len(), 2, "{separate:?}");
            assert!(
                separate.contains(&format!("{} (video)", display.name)),
                "{separate:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let manifest = std::fs::read_to_string(folder.join("recording.rbs.toml")).unwrap();
    assert!(manifest.contains("role = \"video\""), "{manifest}");
    let videos: Vec<PathBuf> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "mkv"))
        .filter(|p| p.file_stem().is_some_and(|s| s != "master"))
        .collect();
    assert_eq!(videos.len(), 1, "{videos:?}");
    let name = videos[0].file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        manifest.contains(&format!("name = \"{name}\"")),
        "{manifest}"
    );
    match stop_recording(&host, &rx, &clock) {
        RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None),
        other => panic!("{other:?}"),
    }
    let file = std::fs::read(&videos[0]).unwrap();
    assert!(file.starts_with(HEADER) && file.ends_with(TRAILER));

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bytes after the header of a WAV file's `data` chunk, as written.
fn wav_data_len(wav: &[u8]) -> usize {
    let mut at = 12;
    while at + 8 <= wav.len() {
        let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().unwrap()) as usize;
        if &wav[at..at + 4] == b"data" {
            return wav.len() - (at + 8);
        }
        at += 8 + size + (size & 1);
    }
    0
}

/// A host with a display and a microphone, the microphone marked to record separately.
fn marked(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    fault: Option<Fault>,
) -> (StudioHost, Receiver<Update>, SourceRow, SourceRow) {
    let (host, rx) = host_with(gpu, clock, fault);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    for kind in [SourceKind::Screen, SourceKind::AudioInput] {
        host.send(Command::ListTargets(kind));
        let target = next(&rx, |u| match u {
            Update::Targets(k, t) if k == kind => Some(t[0].clone()),
            _ => None,
        });
        host.send(Command::AddSource { kind, target });
    }
    let rows = rows_where(&rx, |r| r.len() == 2);
    let find = |k: SourceKind| rows.iter().find(|r| r.kind == Some(k)).unwrap().clone();
    let (display, mic) = (find(SourceKind::Screen), find(SourceKind::AudioInput));
    host.send(Command::SetRecordSeparately {
        id: mic.id,
        patch: RecordPatch::enabled(true),
    });
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == mic.id && r.record_separately.audio)
    });
    (host, rx, display, mic)
}

fn with_tracks(path: PathBuf, encoder: Option<&str>) -> RecordRequest {
    let mut r = request(path, encoder);
    r.iso = true;
    r
}

/// The one WAV in `folder`.
fn only_wav(folder: &std::path::Path) -> PathBuf {
    let wavs: Vec<PathBuf> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    assert_eq!(wavs.len(), 1, "{wavs:?}");
    wavs[0].clone()
}

/// ADR-0086 decision 5 and ADR-0055: a start that panics after the master started
/// leaves no track running; a master that cannot start removes the folder and the
/// tracks; a picture source cannot be marked for sound; deleting the marked source
/// during the take ends cleanly.
#[test]
fn separate_files_clean_up_after_a_refused_or_panicked_start() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-iso-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, display, mic) = marked(gpu, &clock, Some(Fault::AfterOutputStart));

    // A picture source has no sound of its own to mark.
    host.send(Command::SetRecordSeparately {
        id: display.id,
        patch: RecordPatch {
            audio: Some(true),
            ..RecordPatch::default()
        },
    });
    next(&rx, |u| match u {
        Update::Error(why) => Some(why),
        _ => None,
    });

    // The start panics once the master runs (the fault fires once): recovery stops
    // the master and the track, so later takes with tracks start (no "already
    // running"). A crash may leave that take's files; they are not cleaned.
    host.send(Command::StartRecording(with_tracks(
        dir.join("lost.mkv"),
        None,
    )));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => assert_eq!(why, rbs_crash::CAUGHT_TEXT),
        other => panic!("{other:?}"),
    }
    next(&rx, |u| match u {
        Update::Recovered { .. } => Some(()),
        _ => None,
    });

    // The master's only encoder cannot open: nothing is left behind.
    host.send(Command::StartRecording(with_tracks(
        dir.join("broken.mkv"),
        Some(BROKEN),
    )));
    assert!(matches!(recording(&rx), RecordingUpdate::NotStarted(_)));
    assert!(!dir.join("broken").exists());

    let folder = dir.join("take");
    host.send(Command::StartRecording(with_tracks(
        dir.join("take.mkv"),
        None,
    )));
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert_eq!(path, folder.join("master.mkv")),
        other => panic!("{other:?}"),
    }

    // The marked microphone is deleted during the take: the take goes on and stops
    // cleanly, and both files are complete.
    host.send(Command::DeleteSource(mic.id));
    rows_where(&rx, |r| r.len() == 1);
    match stop_recording(&host, &rx, &clock) {
        RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None),
        other => panic!("{other:?}"),
    }
    let wav = std::fs::read(only_wav(&folder)).unwrap();
    assert!(wav.starts_with(b"RIFF") || wav.starts_with(b"RF64"));
    assert!(
        std::fs::read(folder.join("master.mkv"))
            .unwrap()
            .ends_with(TRAILER)
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A disk whose free space the test sets; memory is unsupported.
struct FakeDisk(Arc<AtomicU64>);

impl SystemProbe for FakeDisk {
    fn volume_space(&mut self, _folder: &std::path::Path) -> Probed<VolumeSpace> {
        Probed::Value(VolumeSpace {
            volume: VolumeKey(1),
            free_bytes: self.0.load(AtomicOrdering::SeqCst),
        })
    }
    fn available_memory(&mut self) -> Probed<u64> {
        Probed::Unsupported
    }
}

#[test]
fn a_recording_stops_cleanly_with_the_reason_when_its_disk_reaches_the_reserve() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-protect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let free = Arc::new(AtomicU64::new(500 << 20));
    let mut config = HostConfig::desktop();
    let disk = Arc::clone(&free);
    config.system_probe = Some(SystemProbeFactory::new(move || {
        Box::new(FakeDisk(Arc::clone(&disk)))
    }));
    let (host, rx) = spawn_host(gpu, &clock, config);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let (api, mut c) = Client::connect(&host);
    let none = serde_json::json!({});
    // Nothing runs: no guard.
    let idle = c.call("protection.status", none.clone());
    assert_eq!(idle["result"]["data"]["guarded"], false, "{idle}");
    assert_eq!(idle["result"]["data"]["level"], "ok", "{idle}");
    let path = dir.join("full.mkv");
    host.send(Command::StartRecording(request(
        path.clone(),
        Some(WORKING),
    )));
    match recording(&rx) {
        RecordingUpdate::Started { .. } => {}
        other => panic!("{other:?}"),
    }
    // 500 MiB is below the 1 GiB reserve: once the probe answered, the guard asks the
    // recording to end, the user is told why, and the tail ends it with that reason.
    // The clock moves a second per wait so the guard steps; no real time is waited for.
    let told = next_while(
        &rx,
        || clock.advance(Duration::from_secs(1)),
        |u| match u {
            // Moving the test clock a whole second skips render ticks, so a render
            // warning may come first; the disk's sentence is the one waited for.
            Update::Error(e) if e.contains("disk") => Some(e),
            Update::Recording(r) => panic!("stopped before the user was told: {r:?}"),
            Update::Failed(e) => panic!("the studio failed: {e}"),
            _ => None,
        },
    );
    assert!(told.contains("before the disk was full"), "{told}");
    // While the tail runs (the clock stands still), the API shows why.
    let st = c.call("protection.status", none.clone());
    let data = &st["result"]["data"];
    assert_eq!(data["guarded"], true, "{st}");
    assert_eq!(data["level"], "critical", "{st}");
    assert_eq!(data["disks"][0]["recordings"][0], "main", "{st}");
    assert_eq!(data["alerts"][0]["kind"], "disk", "{st}");
    assert_eq!(data["alerts"][0]["text"], told.as_str(), "{st}");
    assert_eq!(data["memory"]["level"], "unavailable", "{st}");
    let stopped = next_while(
        &rx,
        || clock.advance(Duration::from_secs(1)),
        |u| match u {
            Update::Recording(r) => Some(r),
            Update::Failed(e) => panic!("the studio failed: {e}"),
            _ => None,
        },
    );
    match stopped {
        RecordingUpdate::Stopped {
            path: p, failure, ..
        } => {
            assert_eq!(p, path);
            assert_eq!(failure.as_deref(), Some(told.as_str()));
        }
        other => panic!("{other:?}"),
    }
    assert!(path.exists(), "the file is complete, not removed");
    // The guard is gone with the last output, after its next round.
    next_while(
        &rx,
        || clock.advance(Duration::from_secs(1)),
        |u| match u {
            Update::Stats(_) => Some(()),
            _ => None,
        },
    );
    let idle = c.call("protection.status", none);
    assert_eq!(idle["result"]["data"]["guarded"], false, "{idle}");
    drop((api, host));
    let _ = std::fs::remove_dir_all(&dir);
}

struct ChannelSender(Sender<serde_json::Value>);

impl ConnectionSender for ChannelSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        self.0
            .send(serde_json::from_str(text).unwrap())
            .map_err(|_| SendError::Closed)
    }

    fn close(&mut self, _reason: CloseReason) {}
}

/// A same-user IPC client of the control API, in memory.
struct Client {
    handle: ConnectionHandle,
    rx: Receiver<serde_json::Value>,
    next: i64,
}

impl Client {
    fn connect(host: &StudioHost) -> (ApiServer, Self) {
        let config = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
        let (api, endpoint) =
            ApiServer::start(config, TokenStore::new(), ApiOptions::new(host.api_waker())).unwrap();
        host.attach_api(endpoint, None);
        let (out_tx, out_rx) = mpsc::channel();
        let handle = api
            .inbox()
            .connect(
                PeerInfo::same_user_ipc(),
                Box::new(ChannelSender(out_tx)),
                Box::new(|| {}),
            )
            .unwrap();
        let mut c = Self {
            handle,
            rx: out_rx,
            next: 1,
        };
        let hello = c.call(
            "session.hello",
            serde_json::json!({"protocols": ["1.28"], "client": {"name": "test", "version": "1"}}),
        );
        assert_eq!(
            hello["result"]["protocol"],
            rbs_api::session::PROTOCOL_VERSION,
            "{hello}"
        );
        (api, c)
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next;
        self.next += 1;
        let text =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an answer");
            if v["id"] == id {
                return v;
            }
        }
    }
}

/// A control API connection to `host` (same-user IPC: every scope).
struct ApiClient {
    _api: rbs_api::ApiServer,
    handle: rbs_api::transport::ConnectionHandle,
    rx: Receiver<serde_json::Value>,
    next: i64,
    /// Events that arrived while an answer was awaited.
    events: Vec<serde_json::Value>,
}

struct JsonSender(mpsc::Sender<serde_json::Value>);

impl rbs_api::transport::ConnectionSender for JsonSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), rbs_api::transport::SendError> {
        self.0
            .send(serde_json::from_str(text).unwrap())
            .map_err(|_| rbs_api::transport::SendError::Closed)
    }

    fn close(&mut self, _reason: rbs_api::transport::CloseReason) {}
}

impl ApiClient {
    fn attach(host: &StudioHost) -> Self {
        let server =
            rbs_api::session::ServerConfig::new(rbs_api::request::ClientInfo::new("rbs", "test"))
                .unwrap();
        let (api, endpoint) = rbs_api::ApiServer::start(
            server,
            rbs_api::TokenStore::new(),
            rbs_api::thread::ApiOptions::new(host.api_waker()),
        )
        .unwrap();
        host.attach_api(endpoint, None);
        let (handle, rx) = Self::connect(&api);
        let mut c = Self {
            _api: api,
            handle,
            rx,
            next: 1,
            events: Vec::new(),
        };
        c.hello();
        c
    }

    fn connect(
        api: &rbs_api::ApiServer,
    ) -> (
        rbs_api::transport::ConnectionHandle,
        Receiver<serde_json::Value>,
    ) {
        let (tx, rx) = mpsc::channel();
        let handle = api
            .inbox()
            .connect(
                rbs_api::transport::PeerInfo::same_user_ipc(),
                Box::new(JsonSender(tx)),
                Box::new(|| {}),
            )
            .unwrap();
        (handle, rx)
    }

    fn hello(&mut self) {
        self.call(
            "session.hello",
            serde_json::json!({"protocols": ["1.50"], "client": {"name": "test", "version": "1"}}),
        );
    }

    /// A new session on the same server, with its own `marker.add` budget.
    fn reconnect(&mut self) {
        let (handle, rx) = Self::connect(&self._api);
        self.handle = handle;
        self.rx = rx;
        self.hello();
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next;
        self.next += 1;
        let text =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an answer");
            if v["id"] == id {
                return v;
            }
            if v["method"] == "event" {
                self.events.push(v);
            }
        }
    }

    /// The data of the next `topic` event that `ok` accepts (earlier ones are
    /// dropped); waits with the hang guard only.
    fn event(
        &mut self,
        topic: &str,
        mut ok: impl FnMut(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        if let Some(i) = self
            .events
            .iter()
            .position(|e| e["params"]["topic"] == topic && ok(&e["params"]["data"]))
        {
            let e = self.events.remove(i);
            self.events.drain(..i);
            return e["params"]["data"].clone();
        }
        self.events.clear();
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an event");
            if v["params"]["topic"] == topic && ok(&v["params"]["data"]) {
                return v["params"]["data"].clone();
            }
        }
    }
}

/// Moves the clock in frame-sized steps until the running recording's output took
/// packets (the Stats update counts them), so its file has a span.
fn written_some(rx: &Receiver<Update>, clock: &TestClock) {
    next_while(
        rx,
        || clock.advance(Duration::from_millis(5)),
        |u| match u {
            Update::Stats(s) if s.recording.is_some_and(|r| r.packets >= 4) => Some(()),
            _ => None,
        },
    );
}

fn error_kind(answer: &serde_json::Value) -> &serde_json::Value {
    &answer["error"]["data"]["kind"]
}

/// ADR-0089: `marker.add` and `marker.remove` on the running recording; the markers
/// go into `<stem>.rbs.toml` beside a plain recording, and into the recording folder's
/// `recording.rbs.toml`, when it stops. No marker, no sidecar. The stop rewrite
/// completes the manifest of a take that kept its timing (separate files, or the
/// editor project setting), which writes its project or makes one with
/// `project.export`; a plain take without the setting stays incomplete.
#[test]
fn markers_go_into_the_manifest_when_the_recording_stops() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, _display, mic) = marked(gpu, &clock, None);
    // `project.export` looks in the profile's recordings folder.
    let mut profile = rbs_app::record_profile::RecordProfile::new();
    profile.folder = dir.clone();
    host.send(Command::SetProfile(profile));
    let mut api = ApiClient::attach(&host);

    let idle = api.call("marker.add", serde_json::json!({}));
    assert_eq!(error_kind(&idle), "conflict", "{idle}");
    // Every `marker.add` counts toward the session's two a second.
    api.reconnect();

    // A plain recording (`iso` off).
    let plain = dir.join("plain.mkv");
    host.send(Command::StartRecording(request(plain.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(3));
    let added = api.call(
        "marker.add",
        serde_json::json!({"label": "Good take", "color": "green"}),
    );
    assert_eq!(
        added["result"],
        serde_json::json!({"markers": [{"canvas": "main", "index": 1, "t_ns": 3_000_000_000u64}]}),
        "{added}"
    );
    let bad = api.call("marker.add", serde_json::json!({"color": "pink"}));
    assert_eq!(error_kind(&bad), "invalid_params", "{bad}");
    clock.advance(Duration::from_secs(1));
    let second = api.call("marker.add", serde_json::json!({}));
    assert_eq!(second["result"]["markers"][0]["index"], 2, "{second}");
    let removed = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "main", "index": 2}),
    );
    assert!(removed.get("error").is_none(), "{removed}");
    let again = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "main", "index": 2}),
    );
    assert_eq!(error_kind(&again), "not_found", "{again}");
    let vertical = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "vertical", "index": 1}),
    );
    assert_eq!(
        error_kind(&vertical),
        "not_found",
        "no vertical canvas: {vertical}"
    );
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("plain.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(manifest.files[0].file_name, "plain.mkv");
    assert_eq!(manifest.markers.len(), 1, "{text}");
    let m = &manifest.markers[0];
    assert_eq!(
        (m.index, m.t_ns, m.label.as_str(), m.color),
        (
            1,
            3_000_000_000,
            "Good take",
            rbs_manifest::MarkerColor::Green
        )
    );
    assert!(
        !text.contains(&dir.display().to_string()),
        "no path: {text}"
    );
    assert!(
        !manifest.is_complete(),
        "no timing without separate files or the editor project setting: {text}"
    );
    assert!(manifest.files[0].timing.is_none(), "{text}");
    api.reconnect();
    let refused = api.call(
        "project.export",
        serde_json::json!({"recording": "plain.mkv", "format": "fcpxml"}),
    );
    assert_eq!(error_kind(&refused), "failed", "{refused}");
    for (name, kind) in [
        ("../plain.mkv", "invalid_params"),
        ("quiet.mkv", "not_found"),
        ("nothing", "not_found"),
    ] {
        let answer = api.call(
            "project.export",
            serde_json::json!({"recording": name, "format": "fcpxml"}),
        );
        assert_eq!(error_kind(&answer), kind, "{name}: {answer}");
    }

    // No marker: no sidecar.
    let quiet = dir.join("quiet.mkv");
    host.send(Command::StartRecording(request(quiet.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    stop_recording(&host, &rx, &clock);
    assert!(quiet.is_file());
    assert!(!dir.join("quiet.rbs.toml").exists());

    // A recording folder: the markers join the start manifest.
    api.reconnect();
    let take = dir.join("take.mkv");
    host.send(Command::StartRecording(with_tracks(take, None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(2));
    let added = api.call("marker.add", serde_json::json!({"canvas": "main"}));
    assert_eq!(added["result"]["markers"][0]["index"], 1, "{added}");
    written_some(&rx, &clock);
    stop_recording(&host, &rx, &clock);
    let text = std::fs::read_to_string(dir.join("take").join("recording.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert_eq!(manifest.markers.len(), 1, "{text}");
    assert_eq!(manifest.markers[0].label, "Marker 1");
    assert!(
        manifest
            .files
            .iter()
            .any(|f| f.source_name.as_deref() == Some(mic.name.as_str())),
        "{text}"
    );
    assert!(manifest.markers[0].t_ns >= 2_000_000_000, "{text}");
    // The stop rewrite: complete, and every file placed from `t0` with its format.
    assert!(manifest.is_complete(), "{text}");
    let master = manifest.files.iter().find(|f| f.role == "master").unwrap();
    let timing = master.timing.unwrap_or_else(|| panic!("{text}"));
    assert!(timing.duration_ns > 0, "{text}");
    // Nothing renders on the still clock, so the first frame comes once the clock
    // moves (after the marker): seconds after `t0` here, a frame or so in real use.
    assert!(timing.start_offset_ns < 5_000_000_000, "{text}");
    assert!(timing.picture.is_some() && timing.sound.is_some(), "{text}");
    let audio = manifest.files.iter().find(|f| f.role == "audio").unwrap();
    let sound = audio.timing.unwrap_or_else(|| panic!("{text}"));
    assert!(sound.picture.is_none() && sound.sound.is_some(), "{text}");
    let done = manifest.completion.unwrap();
    assert_eq!(done.picture, timing.picture);
    assert!(
        done.duration_ns >= timing.start_offset_ns + timing.duration_ns,
        "{text}"
    );
    // `project.export` writes the project next to the manifest, never replacing one.
    api.reconnect();
    let first = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "fcpxml"}),
    );
    assert_eq!(
        first["result"],
        serde_json::json!({"file": "take.fcpxml", "missing": []}),
        "{first}"
    );
    let second = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "fcpxml"}),
    );
    assert_eq!(second["result"]["file"], "take 2.fcpxml", "{second}");
    let project = std::fs::read_to_string(dir.join("take").join("take.fcpxml")).unwrap();
    assert!(project.contains("<fcpxml version=\"1.9\">"), "{project}");
    assert!(project.contains("master.mkv"), "{project}");
    // FCP7 XML, the format DaVinci Resolve reads markers from.
    let xml = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "xmeml"}),
    );
    assert_eq!(xml["result"]["file"], "take.xml", "{xml}");
    let project = std::fs::read_to_string(dir.join("take").join("take.xml")).unwrap();
    assert!(project.contains("<xmeml version=\"5\">"), "{project}");
    assert!(project.contains("master.mkv"), "{project}");

    // The editor project setting: a plain take keeps its timing and writes its
    // project when it stops, with no marker.
    let mut with_project = request(dir.join("edit.mkv"), None);
    with_project.editor_project = Some(rbs_app::record_profile::EditorProject::Fcpxml);
    host.send(Command::StartRecording(with_project));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    written_some(&rx, &clock);
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("edit.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert!(manifest.is_complete(), "{text}");
    assert!(manifest.markers.is_empty(), "{text}");
    assert!(
        manifest.files[0].timing.unwrap().start_offset_ns < 5_000_000_000,
        "{text}"
    );
    assert!(dir.join("edit.fcpxml").is_file());

    drop(api);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `record.estimate` (protocol 1.53, ADR-0087 step C2a): the plan of a start of the
/// saved profile next to what runs, its separate files listed with their cost, and
/// the live limit of separate video files (3 while the encoder's capacity is
/// unknown). It opens nothing.
#[test]
fn the_estimate_lists_a_start_and_its_separate_files_without_opening_anything() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let (host, rx, display, mic) = marked(gpu, &clock, None);
    host.send(Command::SetRecordSeparately {
        id: display.id,
        patch: RecordPatch {
            video: Some(true),
            ..RecordPatch::default()
        },
    });
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == display.id && r.record_separately.video)
    });
    let mut api = ApiClient::attach(&host);

    let answer = api.call("record.estimate", serde_json::json!({"profile": "default"}));
    let e = &answer["result"];
    assert!(answer.get("error").is_none(), "{answer}");
    assert!(
        ["fits", "heavy"].contains(&e["level"].as_str().unwrap()),
        "{answer}"
    );
    assert_eq!(e["max_tracks"]["video"]["at_most_now"], 3, "{answer}");
    let tracks: Vec<(u64, &str)> = e["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["source"].as_u64().unwrap(), t["role"].as_str().unwrap()))
        .collect();
    assert!(
        tracks.contains(&(u64::from(display.id.get()), "video")),
        "{answer}"
    );
    assert!(
        tracks.contains(&(u64::from(mic.id.get()), "audio")),
        "{answer}"
    );
    // The master and the screen's file, each one session on the test encoder; whole
    // numbers on the wire.
    let sessions = e["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource"] == "encoder_sessions")
        .unwrap_or_else(|| panic!("{answer}"));
    assert_eq!(sessions["need"], 2, "{answer}");
    assert!(sessions["scope"].is_string(), "{answer}");

    // Without the separate files: the master alone, no live limit.
    let plain = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "iso": false}),
    );
    assert_eq!(plain["result"]["tracks"], serde_json::json!([]), "{plain}");
    assert!(
        plain["result"]["max_tracks"]["video"]
            .get("at_most_now")
            .is_none(),
        "{plain}"
    );
    // `all` without a vertical canvas is the main one; an explicit vertical is not
    // found; an unknown profile too.
    let all = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "canvas": "all"}),
    );
    assert_eq!(all["result"]["tracks"], e["tracks"], "{all}");
    let vertical = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "canvas": "vertical"}),
    );
    assert_eq!(error_kind(&vertical), "not_found", "{vertical}");
    let other = api.call("record.estimate", serde_json::json!({"profile": "other"}));
    assert_eq!(error_kind(&other), "not_found", "{other}");
    // Nothing started.
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");
}

/// Points the API's `default` profile at `folder`, and waits until the host has it.
fn profile_in(host: &StudioHost, rx: &Receiver<Update>, folder: PathBuf) {
    let mut p = rbs_app::record_profile::RecordProfile::new();
    p.folder = folder;
    host.send(Command::SetProfile(p));
    // The host takes commands in order: once this answer comes, the profile is set.
    host.send(Command::ListTargets(SourceKind::Screen));
    next(rx, |u| match u {
        Update::Targets(SourceKind::Screen, _) => Some(()),
        _ => None,
    });
}

/// The one take folder in `folder`, and its manifest.
fn take_manifest(folder: &std::path::Path) -> (PathBuf, rbs_manifest::RecordingManifest) {
    let takes: Vec<PathBuf> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(takes.len(), 1, "{takes:?}");
    let text = std::fs::read_to_string(takes[0].join("recording.rbs.toml")).unwrap();
    (
        takes[0].clone(),
        rbs_manifest::RecordingManifest::from_toml(&text).unwrap(),
    )
}

/// A host with `n` screens, each marked to record its picture separately; their ids in
/// order.
fn screens(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    n: usize,
) -> (StudioHost, Receiver<Update>, Vec<u32>) {
    let (host, rx) = host(gpu, clock);
    let ids = mark_screens(&host, &rx, n);
    (host, rx, ids)
}

/// Adds `n` screens to a host that just spawned, each marked to record its picture
/// separately; their ids in order.
fn mark_screens(host: &StudioHost, rx: &Receiver<Update>, n: usize) -> Vec<u32> {
    next(rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::ListTargets(SourceKind::Screen));
    let target = next(rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    for _ in 0..n {
        host.send(Command::AddSource {
            kind: SourceKind::Screen,
            target: target.clone(),
        });
    }
    let rows = rows_where(rx, |r| r.len() == n);
    for r in &rows {
        host.send(Command::SetRecordSeparately {
            id: r.id,
            patch: RecordPatch::enabled(true),
        });
    }
    rows_where(rx, |r| {
        r.iter().filter(|r| r.record_separately.video).count() == n
    });
    let mut ids: Vec<u32> = rows.iter().map(|r| r.id.get()).collect();
    ids.sort_unstable();
    ids
}

/// Control API 1.54 (ADR-0087 decisions 5 and 7): with four screens marked and the
/// encoder's capacity unknown (cap 3), `ask` refuses before anything is on disk,
/// `skip` and an older request start with three files and list the fourth, also in
/// the manifest's `[[skipped]]`; `cpu` is refused.
#[test]
fn the_start_gate_refuses_or_skips_video_files_past_the_encoder_cap() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, ids) = screens(gpu, &clock, 4);
    let mut api = ApiClient::attach(&host);
    let fourth = serde_json::json!([{"source": ids[3], "role": "video"}]);

    profile_in(&host, &rx, dir.join("cpu"));
    let cpu = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "cpu"}),
    );
    assert_eq!(error_kind(&cpu), "invalid_params", "{cpu}");

    let ask_dir = dir.join("ask");
    profile_in(&host, &rx, ask_dir.clone());
    let ask = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "ask"}),
    );
    assert_eq!(ask["error"]["code"], -32040, "{ask}");
    assert_eq!(error_kind(&ask), "failed", "{ask}");
    assert_eq!(ask["error"]["data"]["reason"], "hardware_limit", "{ask}");
    assert_eq!(ask["error"]["data"]["tracks"], fourth, "{ask}");
    assert!(
        ask["error"]["data"]["estimate"]["level"].is_string(),
        "{ask}"
    );
    // Refused before the folder was made.
    assert!(
        std::fs::read_dir(&ask_dir).map_or(true, |mut d| d.next().is_none()),
        "nothing on disk"
    );
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");

    for (name, params) in [
        (
            "skip",
            serde_json::json!({"profile": "default", "on_hardware_limit": "skip"}),
        ),
        (
            "older",
            serde_json::json!({"profile": "default", "acknowledge_heavy": null, "on_hardware_limit": null}),
        ),
    ] {
        let folder = dir.join(name);
        profile_in(&host, &rx, folder.clone());
        let started = api.call("record.start", params);
        assert!(started.get("error").is_none(), "{name}: {started}");
        // The UI is told as if its own button started it.
        assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
        assert_eq!(
            started["result"]["skipped"],
            serde_json::json!([{"source": ids[3], "role": "video", "reason": "limit"}]),
            "{name}: {started}"
        );
        assert!(
            started["result"]["estimate"].is_object(),
            "{name}: {started}"
        );
        let (take, manifest) = take_manifest(&folder);
        let videos = manifest.files.iter().filter(|f| f.role == "video").count();
        assert_eq!(videos, 3, "{name}: {manifest:?}");
        assert_eq!(manifest.skipped.len(), 1, "{name}: {manifest:?}");
        assert_eq!(manifest.skipped[0].source.get(), ids[3]);
        assert_eq!(manifest.skipped[0].reason, "limit");
        assert_eq!(manifest.skipped[0].role, "video");
        match stop_recording(&host, &rx, &clock) {
            RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        // The stop rewrite keeps the skipped source.
        let text = std::fs::read_to_string(take.join("recording.rbs.toml")).unwrap();
        let after = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
        assert_eq!(after.skipped, manifest.skipped, "{name}");
    }

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control API 1.54 (ADR-0087 decisions 6 and 7), with a calibrated recording folder
/// whose write speed makes the master with one separate file heavy and the master
/// alone fit: a new request without `acknowledge_heavy` is refused (`heavy`), one with
/// it starts the file; an older request is never refused, trims the file and, with
/// nothing else marked, records one file as 1.46 does.
#[test]
fn a_heavy_plan_is_refused_without_acknowledgement_and_an_older_start_trims_to_one_file() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-heavy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let folder = dir.join("takes");
    let clock = TestClock::new();
    let disk_need = |answer: &serde_json::Value| {
        answer["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["resource"] == "disk_speed")
            .unwrap_or_else(|| panic!("{answer}"))["need"]
            .as_f64()
            .unwrap()
    };
    // What the start writes, from an uncalibrated host's estimate.
    let (full, alone) = {
        let (host, rx, _) = screens(gpu.clone(), &clock, 1);
        profile_in(&host, &rx, folder.clone());
        let mut api = ApiClient::attach(&host);
        let full = api.call("record.estimate", serde_json::json!({"profile": "default"}));
        let alone = api.call(
            "record.estimate",
            serde_json::json!({"profile": "default", "iso": false}),
        );
        (disk_need(&full), disk_need(&alone))
    };
    assert!(full > alone && alone > 0.0, "{full} {alone}");
    // The master with its file at 85 % of the disk: heavy; the master alone fits.
    let write_speed = (full / 0.85).ceil() as u64;
    assert!(alone < 0.7 * write_speed as f64, "{alone} of {write_speed}");
    let mut config = HostConfig::desktop();
    config
        .calibration
        .folders
        .push(rbs_app::calibration_file::FolderEntry {
            path: folder.clone(),
            write_speed,
            measured_at: 0,
            partial: false,
        });
    let (host, rx) = spawn_host(gpu, &clock, config);
    let ids = mark_screens(&host, &rx, 1);
    profile_in(&host, &rx, folder.clone());
    let mut api = ApiClient::attach(&host);

    let heavy = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "skip"}),
    );
    assert_eq!(heavy["error"]["data"]["reason"], "heavy", "{heavy}");
    assert_eq!(
        heavy["error"]["data"]["estimate"]["gate_level"], "heavy",
        "{heavy}"
    );
    assert!(heavy["error"]["data"].get("tracks").is_none(), "{heavy}");
    assert!(
        std::fs::read_dir(&folder).map_or(true, |mut d| d.next().is_none()),
        "nothing on disk"
    );

    let acknowledged = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "acknowledge_heavy": true}),
    );
    assert!(acknowledged.get("error").is_none(), "{acknowledged}");
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    assert!(
        acknowledged["result"].get("skipped").is_none(),
        "{acknowledged}"
    );
    let (_, manifest) = take_manifest(&folder);
    assert_eq!(
        manifest.files.iter().filter(|f| f.role == "video").count(),
        1,
        "{manifest:?}"
    );
    stop_recording(&host, &rx, &clock);
    std::fs::remove_dir_all(&folder).unwrap();

    let older = api.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(older.get("error").is_none(), "{older}");
    match recording(&rx) {
        RecordingUpdate::Started { path, separate, .. } => {
            // One file, not a folder (ADR-0087 decision 7).
            assert_eq!(path.parent(), Some(folder.as_path()), "{path:?}");
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mkv"));
            assert!(separate.is_empty(), "{separate:?}");
        }
        other => panic!("{other:?}"),
    }
    let skipped = serde_json::json!([{"source": ids[0], "role": "video", "reason": "limit"}]);
    assert_eq!(older["result"]["skipped"], skipped, "{older}");
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["skipped"], skipped, "{status}");
    stop_recording(&host, &rx, &clock);

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control API 1.54 (ADR-0087 decision 5, control API "Opening the files"): a file
/// whose hardware encoder refuses a session never falls to a software encoder. Under
/// `ask` the start is refused after the master ran, and what opened is stopped and
/// removed; an older request skips the file (`open_failed`) and keeps its folder.
#[test]
fn a_file_whose_hardware_encoder_refuses_is_skipped_or_refuses_the_start_under_ask() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-hw-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = spawn_host_hw(gpu, &clock, HostConfig::desktop(), true);
    let ids = mark_screens(&host, &rx, 1);
    let mut api = ApiClient::attach(&host);
    let fallback = |answer: &serde_json::Value, at: &str| {
        answer[at]["estimate"]["reasons"]
            .as_array()
            .is_some_and(|r| {
                r.iter()
                    .any(|r| r["code"] == "software_fallback" && r["track"]["source"] == ids[0])
            })
    };

    let ask_dir = dir.join("ask");
    profile_in(&host, &rx, ask_dir.clone());
    let ask = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "ask"}),
    );
    assert_eq!(ask["error"]["data"]["reason"], "hardware_limit", "{ask}");
    assert_eq!(
        ask["error"]["data"]["tracks"],
        serde_json::json!([{"source": ids[0], "role": "video"}]),
        "{ask}"
    );
    assert!(fallback(&ask["error"], "data"), "{ask}");
    // The master ran and was stopped; its folder is gone; nothing is recording.
    assert!(
        std::fs::read_dir(&ask_dir).map_or(true, |mut d| d.next().is_none()),
        "{:?}",
        std::fs::read_dir(&ask_dir).map(|d| d.map(|e| e.unwrap().path()).collect::<Vec<_>>())
    );
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");

    let older_dir = dir.join("older");
    profile_in(&host, &rx, older_dir.clone());
    let older = api.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(older.get("error").is_none(), "{older}");
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    assert_eq!(
        older["result"]["skipped"],
        serde_json::json!([{"source": ids[0], "role": "video", "reason": "open_failed"}]),
        "{older}"
    );
    assert!(fallback(&older, "result"), "{older}");
    let (_, manifest) = take_manifest(&older_dir);
    assert_eq!(manifest.skipped.len(), 1, "{manifest:?}");
    assert_eq!(manifest.skipped[0].reason, "open_failed");
    stop_recording(&host, &rx, &clock);

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------
// ADR-0087 step D1: *Test this computer* (control API 1.55).

/// The calibration run's clock moves only through its pacer and sink.
struct CalPacer(TestClock);

impl rbs_record::calibration::Pacer for CalPacer {
    fn yield_now(&self) {
        self.0.advance(Duration::from_micros(50));
    }
    fn wait_until(&self, t: rbs_core::time::Timestamp) {
        use rbs_core::time::Clock as _;
        if self.0.now() < t {
            self.0.set(t);
        }
    }
}

/// A write sink in memory; `create` waits on `gate` when one is given.
struct CalSink {
    clock: TestClock,
    gate: Option<Arc<std::sync::Mutex<Receiver<()>>>>,
}

impl rbs_record::calibration::WriteSink for CalSink {
    fn list(&mut self, _: &std::path::Path) -> std::io::Result<Vec<(String, u64)>> {
        Ok(Vec::new())
    }
    fn remove(&mut self, _: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }
    fn create(&mut self, _: &std::path::Path) -> std::io::Result<()> {
        if let Some(g) = &self.gate {
            let _ = g.lock().unwrap().recv();
        }
        Ok(())
    }
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        self.clock.advance(Duration::from_millis(10));
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.clock.advance(Duration::from_millis(10));
        Ok(())
    }
}

fn calibration_setup(
    clock: &TestClock,
    gate: Option<Receiver<()>>,
) -> rbs_app::studio_host::CalibrationSetup {
    let gate = gate.map(|g| Arc::new(std::sync::Mutex::new(g)));
    let sink_clock = clock.clone();
    rbs_app::studio_host::CalibrationSetup {
        clock: Arc::new(clock.clone()),
        pacer: Arc::new(CalPacer(clock.clone())),
        sink: Arc::new(move || {
            Box::new(CalSink {
                clock: sink_clock.clone(),
                gate: gate.clone(),
            })
        }),
        write_bytes: 64 * 1024 * 1024,
        now_unix: Arc::new(|| 1_000_000),
        export_rig: fake_export_rig(clock, true),
    }
}

/// A scripted export rig on the run's clock (step D2): each tick costs 400 µs of the
/// render thread and each conversion 300 µs of the GPU; `works: false` has no device.
fn fake_export_rig(clock: &TestClock, works: bool) -> rbs_app::studio_host::ExportRigMaker {
    use rbs_record::calibration::{ExportClass, ExportCounters, ExportRig, RigError};
    struct Rig {
        clock: TestClock,
        counters: ExportCounters,
        in_flight: u64,
    }
    impl ExportRig for Rig {
        fn begin(&mut self, _: &ExportClass) -> Result<(), RigError> {
            Ok(())
        }
        fn tick(&mut self) -> Result<(), RigError> {
            self.counters.frames += self.in_flight;
            self.in_flight = 1;
            self.clock.advance(Duration::from_micros(400));
            Ok(())
        }
        fn wait(&mut self, _: Duration) -> Result<bool, RigError> {
            self.clock.advance(Duration::from_micros(300));
            Ok(true)
        }
        fn counters(&self) -> ExportCounters {
            self.counters
        }
        fn end(&mut self) {}
    }
    let clock = clock.clone();
    Arc::new(move |_, _| {
        if works {
            Ok(Box::new(Rig {
                clock: clock.clone(),
                counters: ExportCounters::default(),
                in_flight: 0,
            }) as Box<dyn ExportRig>)
        } else {
            Err(RigError::Failed)
        }
    })
}

/// ADR-0087 step D2 (control API 1.57): with a screen marked, *Test this computer*
/// measures the export of its size class first, saves it, and the estimate's render
/// share row no longer asks for a test (the previous tick, step E, is still unknown).
/// Without a throwaway device the class is skipped and the row stops asking.
#[test]
fn test_this_computer_measures_the_export_of_separate_files() {
    for works in [true, false] {
        let Some(gpu) = gpu() else { return };
        let clock = TestClock::new();
        let cal = TestClock::new();
        let dir = temp_dir(if works { "export" } else { "export-none" });
        let mut config = HostConfig::desktop();
        config.config_dir = Some(dir.join("config"));
        let mut setup = calibration_setup(&cal, None);
        setup.export_rig = fake_export_rig(&cal, works);
        config.calibration_setup = Some(setup);
        let (host, rx) = spawn_host(gpu, &clock, config);
        mark_screens(&host, &rx, 1);
        profile_in(&host, &rx, dir.join("rec"));
        let mut api = ApiClient::attach(&host);
        api.call(
            "events.subscribe",
            serde_json::json!({"topics": ["calibration.state"]}),
        );
        let share = |api: &mut ApiClient| {
            let e = api.call("record.estimate", serde_json::json!({"profile": "default"}));
            e["result"]["resources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["resource"] == "iso_render_share")
                .cloned()
                .unwrap_or_else(|| panic!("{e}"))
        };
        let before = share(&mut api);
        assert_eq!(before["level"], "unknown", "{before}");
        assert_eq!(before["calibrate"], true, "{before}");

        let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
        assert!(run.get("error").is_none(), "{run}");
        let done = api.event("calibration.state", |d| d["state"] == "done");
        let after = share(&mut api);
        assert_eq!(after["level"], "unknown", "{after}");
        assert_eq!(after["calibrate"], false, "{after}");
        if works {
            let exports = done["exports"].as_array().unwrap();
            assert_eq!(exports.len(), 1, "{done}");
            assert_eq!(exports[0]["format"], "nv12", "{done}");
            assert_eq!(exports[0]["time_us"], 400, "{done}");
            assert!(
                done["exports_skipped"].as_array().unwrap().is_empty(),
                "{done}"
            );
            // The export part leaves the 1.55 list as it was.
            assert!(
                done["skipped"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|s| s.get("size").is_none()),
                "{done}"
            );
            let text =
                std::fs::read_to_string(dir.join("config").join("calibration.toml")).unwrap();
            assert!(
                text.contains("[[exports]]") && text.contains("time_us = 400"),
                "{text}"
            );
            assert!(after["need"].as_f64().unwrap() > 0.0, "{after}");
        } else {
            assert_eq!(
                done["exports_skipped"][0]["code"], "gpu_unavailable",
                "{done}"
            );
            assert!(done["exports"].as_array().unwrap().is_empty(), "{done}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-cal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn calibration_host(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    cal_clock: &TestClock,
    gate: Option<Receiver<()>>,
    name: &str,
) -> (StudioHost, Receiver<Update>, PathBuf) {
    let dir = temp_dir(name);
    let mut config = HostConfig::desktop();
    config.config_dir = Some(dir.join("config"));
    config.calibration_setup = Some(calibration_setup(cal_clock, gate));
    let (host, rx) = spawn_host(gpu, clock, config);
    profile_in(&host, &rx, dir.join("rec"));
    (host, rx, dir)
}

#[test]
fn test_this_computer_measures_saves_and_the_estimate_uses_it() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (host, _rx, dir) = calibration_host(gpu, &clock, &cal, None, "measure");
    let mut api = ApiClient::attach(&host);
    let sub = api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    assert!(sub.get("error").is_none(), "{sub}");
    let idle = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(idle["result"]["data"]["state"], "idle", "{idle}");

    // One canvas per run; an unknown profile is not found.
    let all = api.call(
        "calibration.run",
        serde_json::json!({"profile": "default", "canvas": "all"}),
    );
    assert_eq!(error_kind(&all), "invalid_params", "{all}");
    let other = api.call("calibration.run", serde_json::json!({"profile": "other"}));
    assert_eq!(error_kind(&other), "not_found", "{other}");

    let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
    assert!(run.get("error").is_none(), "{run}");
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["saved"], true, "{done}");
    assert_eq!(done["cancelled"], false, "{done}");
    // The working fake keeps up to the ceiling; the broken one is unsupported.
    let encoders = done["encoders"].as_array().unwrap();
    let working = encoders
        .iter()
        .find(|e| e["adapter"] == WORKING)
        .unwrap_or_else(|| panic!("{done}"));
    assert_eq!(working["stopped_by"], "ceiling", "{done}");
    assert_eq!(working["sessions"], 20, "{done}");
    assert!(
        done["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["code"] == "unsupported" && s["adapter"] == BROKEN),
        "{done}"
    );
    assert!(done["write_speed"].as_u64().unwrap() > 0, "{done}");
    // No path in the state.
    assert!(!done.to_string().contains("rbs-cal-"), "{done}");
    let text = std::fs::read_to_string(dir.join("config").join("calibration.toml")).unwrap();
    assert!(text.contains("stopped_by = \"ceiling\""), "{text}");

    // The next estimate reads the measurement.
    let e = api.call("record.estimate", serde_json::json!({"profile": "default"}));
    // The likely master encoder is the broken fake (unsupported, so unmeasured);
    // the folder speed is measured either way.
    let disk = e["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource"] == "disk_speed")
        .cloned()
        .unwrap_or_else(|| panic!("{e}"));
    assert_eq!(disk["measured"], true, "{e}");
    assert!(text.contains(WORKING), "{text}");
    let status = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"], done, "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn starts_are_refused_while_the_test_runs_and_a_cancel_ends_it() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (gate_tx, gate_rx) = mpsc::channel();
    let (host, rx, dir) = calibration_host(gpu, &clock, &cal, Some(gate_rx), "refuse");
    let mut api = ApiClient::attach(&host);
    api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
    assert!(run.get("error").is_none(), "{run}");
    // The write part waits on the gate: the run stays active.
    api.event("calibration.state", |d| {
        d["state"] == "running" && d["part"] == "write"
    });
    for (method, params) in [
        ("record.start", serde_json::json!({"profile": "default"})),
        ("stream.start", serde_json::json!({})),
        ("replay.arm", serde_json::json!({})),
        ("calibration.run", serde_json::json!({"profile": "default"})),
    ] {
        let a = api.call(method, params);
        assert_eq!(error_kind(&a), "conflict", "{method}: {a}");
    }
    // The desktop's Record says why.
    let path = dir.join("rec").join("x.mkv");
    host.send(Command::StartRecording(request(path, None)));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => {
            assert!(why.contains("Test this computer is running"), "{why}");
        }
        other => panic!("{other:?}"),
    }

    let cancel = api.call("calibration.cancel", serde_json::json!({}));
    assert!(cancel.get("error").is_none(), "{cancel}");
    let cancelling = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(
        cancelling["result"]["data"]["state"], "running",
        "{cancelling}"
    );
    assert_eq!(
        cancelling["result"]["data"]["cancelling"], true,
        "{cancelling}"
    );
    gate_tx.send(()).unwrap();
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["cancelled"], true, "{done}");
    assert_eq!(done["partial"], true, "{done}");
    // Starts are allowed again.
    let status = api.call("record.status", serde_json::json!({}));
    assert!(status.get("error").is_none(), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_that_does_not_answer_ends_at_the_host_deadline() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (_gate_tx, gate_rx) = mpsc::channel::<()>();
    let (host, _rx, dir) = calibration_host(gpu, &clock, &cal, Some(gate_rx), "hung");
    let mut api = ApiClient::attach(&host);
    api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    api.call("calibration.run", serde_json::json!({"profile": "default"}));
    api.event("calibration.state", |d| {
        d["state"] == "running" && d["part"] == "write"
    });
    // The write never returns; past the limit and the grace the host ends the run
    // with what it received.
    cal.advance(Duration::from_secs(70));
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["partial"], true, "{done}");
    assert!(!done["encoders"].as_array().unwrap().is_empty(), "{done}");
    let a = api.call("record.status", serde_json::json!({}));
    assert!(a.get("error").is_none(), "{a}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0089 decisions 3 and 4: scene changes mark a running recording (on by
/// default) with the new program scene's name, the lead moves a manual marker
/// earlier and keeps the press time, and with scene markers off a switch adds none.
#[test]
fn scene_markers_and_the_marker_lead_follow_the_request() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-auto-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx, _display, _mic) = marked(gpu, &clock, None);
    let mut api = ApiClient::attach(&host);
    let list = api.call("scene.list", serde_json::json!({}));
    let first = list["result"]["data"]["program"].clone();
    assert!(first.is_u64(), "{list}");
    let added = api.call("scene.add", serde_json::json!({"name": "Break"}));
    let second = added["result"]["scene"].clone();
    assert!(second.is_u64(), "{added}");

    let mut with_lead = request(dir.join("auto.mkv"), None);
    with_lead.markers.lead_ms = 2_000;
    host.send(Command::StartRecording(with_lead));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(5));
    let switched = api.call("scene.switch", serde_json::json!({"scene": second}));
    assert!(switched.get("error").is_none(), "{switched}");
    clock.advance(Duration::from_secs(1));
    api.reconnect();
    let late = api.call("marker.add", serde_json::json!({"label": "Late"}));
    assert_eq!(
        late["result"]["markers"][0]["t_ns"], 4_000_000_000u64,
        "{late}"
    );
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("auto.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    let marks: Vec<_> = manifest
        .markers
        .iter()
        .map(|m| (m.kind, m.t_ns, m.pressed_ns, m.label.as_str()))
        .collect();
    assert_eq!(
        marks,
        [
            (
                rbs_manifest::MarkerKind::Scene,
                5_000_000_000,
                None,
                "Break"
            ),
            (
                rbs_manifest::MarkerKind::Manual,
                4_000_000_000,
                Some(6_000_000_000),
                "Late"
            ),
        ],
        "{text}"
    );

    // Scene markers off: a switch marks nothing, so the plain take writes no manifest.
    let mut quiet = request(dir.join("quiet.mkv"), None);
    quiet.markers.scene_changes = false;
    host.send(Command::StartRecording(quiet));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(1));
    let back = api.call("scene.switch", serde_json::json!({"scene": first}));
    assert!(back.get("error").is_none(), "{back}");
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(matches!(
        stopped,
        RecordingUpdate::Stopped { failure: None, .. }
    ));
    assert!(!dir.join("quiet.rbs.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
