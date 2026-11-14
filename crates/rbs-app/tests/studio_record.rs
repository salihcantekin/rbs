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

use rbs_app::edit_history::EditKind;
use rbs_app::studio_host::{
    Command, CropRegion, Fault, HostConfig, RecordRequest, RecordingUpdate, SourceKind, SourceRow,
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
    assert!(rows.iter().all(|r| !r.record_separately));

    // Nothing marked: one file, as before.
    let plain = dir.join("plain.mkv");
    host.send(Command::StartRecording(request(plain.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    stop_recording(&host, &rx, &clock);
    assert!(plain.is_file());
    assert!(!dir.join("plain").exists());

    host.send(Command::SetRecordSeparately {
        id: mic.id,
        enabled: true,
    });
    rows_where(&rx, |r| {
        r.iter().any(|r| r.id == mic.id && r.record_separately)
    });

    // Marked, `iso` off (as `record.start_to_file` and `iso: false` ask): still one file.
    let single = dir.join("single.mkv");
    host.send(Command::StartRecording(request(single.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    stop_recording(&host, &rx, &clock);
    assert!(single.is_file());

    // Marked and `iso` on: a folder with the master, the microphone and the manifest.
    let take = dir.join("take.mkv");
    let folder = dir.join("take");
    let mut with_tracks = request(take.clone(), None);
    with_tracks.iso = true;
    host.send(Command::StartRecording(with_tracks.clone()));
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert_eq!(path, folder.join("master.mkv")),
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
        r.iter().any(|r| r.id == mic.id && !r.record_separately)
    });

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
        enabled: true,
    });
    rows_where(&rx, |r| {
        r.iter().any(|r| r.id == mic.id && r.record_separately)
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
/// tracks; a picture source cannot be marked in 1.46; deleting the marked source
/// during the take ends cleanly.
#[test]
fn separate_files_clean_up_after_a_refused_or_panicked_start() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-iso-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, display, mic) = marked(gpu, &clock, Some(Fault::AfterOutputStart));

    // A picture source is refused and stays unmarked.
    host.send(Command::SetRecordSeparately {
        id: display.id,
        enabled: true,
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

/// A control API connection to `host` (same-user IPC: every scope).
struct ApiClient {
    _api: rbs_api::ApiServer,
    handle: rbs_api::transport::ConnectionHandle,
    rx: Receiver<serde_json::Value>,
    next: i64,
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
            serde_json::json!({"protocols": ["1.48"], "client": {"name": "test", "version": "1"}}),
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
        }
    }
}

fn error_kind(answer: &serde_json::Value) -> &serde_json::Value {
    &answer["error"]["data"]["kind"]
}

/// ADR-0089 first slice: `marker.add` and `marker.remove` on the running recording;
/// the markers go into `<stem>.rbs.toml` beside a plain recording, and into the
/// recording folder's `recording.rbs.toml`, when it stops. No marker, no sidecar.
#[test]
fn markers_go_into_the_manifest_when_the_recording_stops() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, _display, mic) = marked(gpu, &clock, None);
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
    assert!(!manifest.is_complete(), "the stop rewrite is a later step");

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

    drop(api);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}
