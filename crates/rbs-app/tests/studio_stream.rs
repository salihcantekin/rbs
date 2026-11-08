//! Streaming through the studio host (Go live): the key read from the key store reaches
//! the transport only, a missing key refuses before anything opens, connection changes
//! reach the UI, a recording runs alongside the stream, and the stream ends cleanly or with the
//! server's reason; a control API client goes live, follows the connection on
//! `stream.state` and ends the stream (ADR-0044). Reference sources, fake encoders and a fake RTMP transport whose
//! events the test injects. The engine runs on a `TestClock` that never moves; the
//! output worker's doorbell timer polls the transport. Waits are for the host's updates,
//! bounded only as a hang guard (ADR-0032). Needs a GPU adapter (software is fine);
//! skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::record_profile::RecordProfile;
use rbs_app::stream_key::{ACCOUNT, KeyStore, MemoryKeyStore};
use rbs_app::stream_profile::StreamProfile;
use rbs_app::studio_host::{
    Command, HostConfig, RecordRequest, RecordingUpdate, SourceKind, Stats, StreamRequest,
    StreamUpdate, StudioHost, Update,
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
    BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamDescriptor, StreamParams,
    VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::{END_TAIL_LIMIT, RecordAdapters, VideoSettings};
use rbs_transport::{
    ReconnectPolicy, SendOutcome, Transport, TransportConfig, TransportError, TransportEvent,
    TransportFactory, TransportInput, TransportItem, TransportStats, caps as transport_caps,
};

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

/// What the fake RTMP server saw, and the events the test makes it report.
#[derive(Default)]
struct Server {
    /// URL and key of each session created.
    sessions: Mutex<Vec<(String, Option<String>)>>,
    /// Events the next `poll_events` reports.
    events: Mutex<Vec<TransportEvent>>,
    /// Sessions finished.
    finished: Mutex<u32>,
}

struct FakeRtmpFactory(Arc<Server>);

impl AdapterFactory for FakeRtmpFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.rtmp")
    }
    fn display_name(&self) -> &str {
        "test RTMP"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(transport_caps::SCHEMES, set(&["rtmp"]))
            .with(transport_caps::INPUT, set(&[transport_caps::INPUT_PACKETS]))
            .with(transport_caps::RECONNECT, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for FakeRtmpFactory {
    fn create(
        &self,
        config: &TransportConfig,
    ) -> Result<Box<dyn Transport>, rbs_transport::CreateError> {
        self.0.sessions.lock().unwrap().push((
            config.url.clone(),
            config.secret.as_ref().map(|s| s.expose().to_owned()),
        ));
        Ok(Box::new(FakeRtmp(Arc::clone(&self.0))))
    }
}

struct FakeRtmp(Arc<Server>);

impl Transport for FakeRtmp {
    fn start(&mut self, _: &[StreamDescriptor]) -> Result<(), TransportError> {
        // Connects at once, as a server on this computer would.
        self.0
            .events
            .lock()
            .unwrap()
            .push(TransportEvent::Connected);
        Ok(())
    }
    fn send(&mut self, _: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        Ok(SendOutcome::Queued)
    }
    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        out.append(&mut self.0.events.lock().unwrap());
    }
    fn stats(&self) -> TransportStats {
        TransportStats::default()
    }
    fn finish(&mut self, _: Duration) -> Result<(), TransportError> {
        *self.0.finished.lock().unwrap() += 1;
        Ok(())
    }
}

const WORKING: &str = "test.video-working";
const URL: &str = "rtmp://live.example.com/app";

fn host(
    gpu: Arc<GpuContext>,
    keys: Arc<dyn KeyStore>,
    server: Arc<Server>,
    clock: &TestClock,
) -> (StudioHost, Receiver<Update>) {
    let clock = clock.clone();
    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.keys = keys;
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
            a.video_encoders
                .register(Arc::new(FakeVideoFactory {
                    id: WORKING,
                    broken: false,
                }))
                .unwrap();
            a.audio_encoders
                .register(Arc::new(FakeAacFactory::new()))
                .unwrap();
            a.transports
                .register(Arc::new(FakeRtmpFactory(server)))
                .unwrap();
            // A recording alongside the stream.
            a.muxers.register(Arc::new(FakeMuxerFactory)).unwrap();
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
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
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

/// Like [`next`], calling `step` between waits.
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

fn stream(rx: &Receiver<Update>) -> StreamUpdate {
    next(rx, |u| match u {
        Update::Stream(s) => Some(s),
        _ => None,
    })
}

/// The next connection change that `ok` accepts.
fn link_where(
    rx: &Receiver<Update>,
    mut ok: impl FnMut(&rbs_record::Link) -> bool,
) -> rbs_record::Link {
    next(rx, |u| match u {
        Update::Stream(StreamUpdate::Link(l)) if ok(&l) => Some(l),
        Update::Stream(StreamUpdate::Ended { failure, .. }) => panic!("ended: {failure:?}"),
        _ => None,
    })
}

fn key_saved(rx: &Receiver<Update>) -> Result<bool, String> {
    next(rx, |u| match u {
        Update::StreamKey(r) => Some(r),
        _ => None,
    })
}

fn request() -> StreamRequest {
    let mut video = VideoSettings::default();
    video.rate_control = RateControl::Cbr {
        bitrate_bps: 6_000_000,
    };
    StreamRequest {
        url: URL.into(),
        input: TransportInput::Packets,
        needs_key: true,
        reconnect: ReconnectPolicy::Off,
        transport_options: Vec::new(),
        encoder: None,
        video,
        audio_bitrate_bps: 160_000,
    }
}

fn add_display(host: &StudioHost, rx: &Receiver<Update>) {
    host.send(Command::ListTargets(SourceKind::Screen));
    let display = next(rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: display,
    });
}

#[test]
fn streams_take_the_key_from_the_store_and_report_their_connection() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &clock);
    add_display(&host, &rx);

    // RTMP without a saved key: nothing opens.
    host.send(Command::StartStream(request()));
    assert_eq!(stream(&rx), StreamUpdate::NoKey);
    assert!(server.sessions.lock().unwrap().is_empty());

    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "live_abc",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert_eq!(keys.read(ACCOUNT).unwrap().unwrap().expose(), "live_abc");

    host.send(Command::StartStream(request()));
    match stream(&rx) {
        StreamUpdate::Started {
            url,
            video_encoder,
            audio_encoder,
        } => {
            assert_eq!(url, URL);
            assert_eq!(video_encoder, WORKING);
            assert!(audio_encoder.is_some());
        }
        other => panic!("{other:?}"),
    }
    // The key went to the transport, not into the URL.
    assert_eq!(
        *server.sessions.lock().unwrap(),
        [(URL.to_owned(), Some("live_abc".to_owned()))]
    );
    let l = link_where(&rx, |l| l.connected);
    assert_eq!((l.connects, l.reconnect_attempts), (1, 0));
    // The Stats panel gets the stream's counters once a second (on the master clock).
    let stats = next_while(
        &rx,
        || clock.advance(Duration::from_millis(250)),
        |u| match u {
            Update::Stats(s) if s.stream.is_some_and(|o| o.connected) => Some(s),
            _ => None,
        },
    );
    assert_eq!(stats.recording, None, "{stats:?}");

    // A recording runs alongside the stream (ADR-0047) and stops on its own; one
    // stream at a time.
    let file = std::env::temp_dir().join(format!("rbs-app-stream-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&file);
    host.send(Command::StartRecording(RecordRequest {
        path: file.clone(),
        container: ContainerId::Matroska,
        encoder: None,
        video: VideoSettings::default(),
        audio_bitrate_bps: 128_000,
    }));
    let recording = |rx: &_| {
        next(rx, |u| match u {
            Update::Recording(r) => Some(r),
            _ => None,
        })
    };
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert_eq!(path, file),
        other => panic!("{other:?}"),
    }
    // Stop waits for the video tail (ADR-0045); on the still clock that is
    // `END_TAIL_LIMIT`, so the test moves the clock until the answer comes.
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
        RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_file(&file);
    host.send(Command::StartStream(request()));
    match stream(&rx) {
        StreamUpdate::NotStarted(why) => assert!(why.contains("already"), "{why}"),
        other => panic!("{other:?}"),
    }

    // The connection drops and comes back.
    server
        .events
        .lock()
        .unwrap()
        .push(TransportEvent::Reconnecting { attempt: 1 });
    let l = link_where(&rx, |l| !l.connected);
    assert_eq!(l.reconnect_attempts, 1);
    server
        .events
        .lock()
        .unwrap()
        .push(TransportEvent::Connected);
    let l = link_where(&rx, |l| l.connected);
    assert_eq!(l.connects, 2);

    host.send(Command::EndStream);
    match stream(&rx) {
        StreamUpdate::Ended {
            failure, connects, ..
        } => {
            assert_eq!(failure, None);
            assert_eq!(connects, 2);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *server.finished.lock().unwrap(),
        1,
        "the session was finished"
    );
    // With no output left, the panel empties once.
    next(&rx, |u| match u {
        Update::Stats(s) if s == Stats::default() => Some(()),
        _ => None,
    });

    host.send(Command::SaveStreamKey(None));
    assert_eq!(key_saved(&rx), Ok(false));
    assert!(keys.read(ACCOUNT).unwrap().is_none());
    drop(host);
}

#[test]
fn a_stream_the_server_refuses_ends_with_its_reason() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("wrong"))
        .unwrap();
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &TestClock::new());
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    link_where(&rx, |l| l.connected);
    server
        .events
        .lock()
        .unwrap()
        .push(TransportEvent::Disconnected {
            error: TransportError::Rejected("unknown stream key".into()),
        });
    // The lost connection's `Link` may come before the end (the output reports it
    // before the host ends the stream); only the end counts here.
    let ended = next(&rx, |u| match u {
        Update::Stream(StreamUpdate::Link(_)) => None,
        Update::Stream(s) => Some(s),
        _ => None,
    });
    match ended {
        StreamUpdate::Ended { failure, .. } => {
            let why = failure.expect("a reason");
            assert!(why.contains("unknown stream key"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    // The host is ready for the next one.
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    host.send(Command::EndStream);
    // The new connection's `Link` may come before or after End stream is taken; only
    // the end counts here.
    next(&rx, |u| match u {
        Update::Stream(StreamUpdate::Ended { .. }) => Some(()),
        Update::Stream(StreamUpdate::Link(_)) => None,
        Update::Stream(other) => panic!("{other:?}"),
        _ => None,
    });
    drop(host);
}

/// The next recording update.
fn recording(rx: &Receiver<Update>) -> RecordingUpdate {
    next(rx, |u| match u {
        Update::Recording(r) => Some(r),
        _ => None,
    })
}

/// Recording settings that record into a new folder of their own.
fn record_profile(name: &str) -> RecordProfile {
    let folder = std::env::temp_dir().join(format!("rbs-auto-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    let mut p = RecordProfile::new();
    p.folder = folder;
    p
}

/// Moves the clock past the recording tail, then waits for two answers in a row: a
/// recording asked to stop would have stopped in the round after the first one, so no
/// recording update may come before the second.
fn no_recording_stops(host: &StudioHost, rx: &Receiver<Update>, clock: &TestClock) {
    clock.advance(END_TAIL_LIMIT * 2);
    for _ in 0..2 {
        host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
            "live_abc",
        ))));
        next(rx, |u| match u {
            Update::StreamKey(_) => Some(()),
            Update::Recording(r) => panic!("the recording changed: {r:?}"),
            _ => None,
        });
    }
}

/// The end of the stream, skipping connection changes.
fn ended(rx: &Receiver<Update>) -> Option<String> {
    next(rx, |u| match u {
        Update::Stream(StreamUpdate::Link(_)) => None,
        Update::Stream(StreamUpdate::Ended { failure, .. }) => Some(failure),
        Update::Stream(other) => panic!("{other:?}"),
        _ => None,
    })
}

#[test]
fn record_when_the_stream_starts_records_with_the_stream_and_stops_with_it() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    add_display(&host, &rx);
    let mut stream_profile = StreamProfile::new();
    stream_profile.server = URL.into();
    stream_profile.key_saved = true;

    // Off (the default): going live records nothing.
    host.send(Command::SetStreamProfile(stream_profile.clone()));
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    no_recording_stops(&host, &rx, &clock);
    host.send(Command::EndStream);
    assert_eq!(ended(&rx), None);

    // On: the recording starts right after the stream, with the recording settings.
    let first = record_profile("first");
    host.send(Command::SetProfile(first.clone()));
    stream_profile.auto_record = true;
    host.send(Command::SetStreamProfile(stream_profile.clone()));
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert!(path.starts_with(&first.folder)),
        other => panic!("{other:?}"),
    }
    // Ending the stream stops it, after its tail (ADR-0045).
    host.send(Command::EndStream);
    assert_eq!(ended(&rx), None);
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

    // A recording the user started keeps running when the stream ends, and no second
    // one starts.
    let manual = std::env::temp_dir().join(format!("rbs-auto-manual-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&manual);
    host.send(Command::StartRecording(RecordRequest {
        path: manual.clone(),
        container: ContainerId::Matroska,
        encoder: None,
        video: VideoSettings::default(),
        audio_bitrate_bps: 128_000,
    }));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    host.send(Command::EndStream);
    assert_eq!(ended(&rx), None);
    no_recording_stops(&host, &rx, &clock);
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
        RecordingUpdate::Stopped { path, .. } => assert_eq!(path, manual),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_file(&manual);

    // A stream that fails leaves its recording running: it is the copy of what the
    // viewers missed.
    let second = record_profile("second");
    host.send(Command::SetProfile(second.clone()));
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    link_where(&rx, |l| l.connected);
    server
        .events
        .lock()
        .unwrap()
        .push(TransportEvent::Disconnected {
            error: TransportError::Rejected("unknown stream key".into()),
        });
    assert!(ended(&rx).is_some());
    no_recording_stops(&host, &rx, &clock);
    // Going live again does not start another one, and ending that stream leaves the
    // recording to the user.
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    host.send(Command::EndStream);
    assert_eq!(ended(&rx), None);
    no_recording_stops(&host, &rx, &clock);
    host.send(Command::StopRecording);
    next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(RecordingUpdate::Stopped { .. }) => Some(()),
            Update::Recording(other) => panic!("{other:?}"),
            _ => None,
        },
    );
    drop(host);
    for p in [first, second] {
        let _ = std::fs::remove_dir_all(&p.folder);
    }
}

#[test]
fn a_control_api_client_turns_on_record_when_the_stream_starts_and_ends_both() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    add_display(&host, &rx);
    let folder = record_profile("api");
    host.send(Command::SetProfile(folder.clone()));
    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.key_saved = true;
    host.send(Command::SetStreamProfile(profile));
    let (api, mut c) = api_client(&host);
    let none = serde_json::json!({});

    let got = c.call("settings.get", none.clone());
    let stream = got["result"]["data"]["stream"].clone();
    assert_eq!(stream["auto_record"], false, "{got}");
    let mut on = serde_json::json!({"stream": stream.clone()});
    on["stream"]["auto_record"] = true.into();
    let set = c.call("settings.set", on);
    assert_eq!(
        set["result"]["data"]["stream"]["auto_record"], true,
        "{set}"
    );
    // A patch without it (a 1.2 client) leaves it on.
    let mut old = serde_json::json!({"stream": stream});
    old["stream"].as_object_mut().unwrap().remove("auto_record");
    let set = c.call("settings.set", old);
    assert_eq!(
        set["result"]["data"]["stream"]["auto_record"], true,
        "{set}"
    );
    // The UI is told, to show and save it.
    next(&rx, |u| match u {
        Update::StreamProfile(p) if p.auto_record => Some(()),
        _ => None,
    });

    let started = c.call("stream.start", none.clone());
    assert!(started.get("result").is_some(), "{started}");
    match recording(&rx) {
        RecordingUpdate::Started { path, .. } => assert!(path.starts_with(&folder.folder)),
        other => panic!("{other:?}"),
    }
    let stopped = c.call("stream.stop", none);
    assert!(stopped.get("result").is_some(), "{stopped}");
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
    drop(c);
    drop(api);
    drop(host);
    let _ = std::fs::remove_dir_all(&folder.folder);
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
    events: Vec<serde_json::Value>,
}

impl Client {
    /// Calls a method and returns its answer; events that arrive meanwhile are kept.
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
            self.events.push(v);
        }
    }

    /// The next `stream.state` event whose data `ok` accepts; earlier ones are dropped.
    fn stream_state(&mut self, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        self.event("stream.state", ok)
    }

    /// The next event of `name` whose data `ok` accepts; earlier ones are dropped.
    fn event(&mut self, name: &str, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let topic = |e: &serde_json::Value| e["params"]["topic"] == name;
        while !self.events.is_empty() {
            let e = self.events.remove(0);
            if topic(&e) && ok(&e["params"]["data"]) {
                return e;
            }
        }
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an event");
            if topic(&v) && ok(&v["params"]["data"]) {
                return v;
            }
        }
    }
}

/// Serves the control API from `host` and connects a same-user IPC client to it, after
/// `session.hello`.
fn api_client(host: &StudioHost) -> (ApiServer, Client) {
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
    let mut c = Client {
        handle,
        rx: out_rx,
        next: 1,
        events: Vec::new(),
    };
    c.call(
        "session.hello",
        serde_json::json!({"protocols": ["1.2"], "client": {"name": "test", "version": "1"}}),
    );
    (api, c)
}

#[test]
fn a_control_api_client_goes_live_and_ends_the_stream() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &TestClock::new());
    let (api, mut c) = api_client(&host);
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.state"]}),
    );
    assert_eq!(sub["result"]["seq"]["stream.state"], 0, "{sub}");
    let none = serde_json::json!({});

    // The UI has not sent its stream settings yet: refused, nothing guessed.
    let early = c.call("stream.start", none.clone());
    assert!(
        early["error"]["message"]
            .as_str()
            .unwrap()
            .contains("starting"),
        "{early}"
    );
    // No server saved yet: refused, saying what is missing. `add_display` waits for an
    // answer from the control thread, so the settings are in place before the call.
    host.send(Command::SetStreamProfile(StreamProfile::new()));
    add_display(&host, &rx);
    let refused = c.call("stream.start", none.clone());
    assert_eq!(refused["error"]["data"]["kind"], "failed", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("server"),
        "{refused}"
    );

    // A server that is not valid is refused without repeating it.
    let mut profile = StreamProfile::new();
    profile.server = "rtmp://".into();
    host.send(Command::SetStreamProfile(profile));
    host.send(Command::ListTargets(SourceKind::Screen));
    next(&rx, |u| matches!(u, Update::Targets(..)).then_some(()));
    let bad = c.call("stream.start", none.clone());
    assert_eq!(bad["error"]["data"]["kind"], "failed", "{bad}");
    assert!(!bad.to_string().contains("rtmp://"), "{bad}");

    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.reconnect = false;
    host.send(Command::SetStreamProfile(profile));
    host.send(Command::ListTargets(SourceKind::Screen));
    next(&rx, |u| matches!(u, Update::Targets(..)).then_some(()));

    // RTMP without a saved key: nothing opens.
    let no_key = c.call("stream.start", none.clone());
    assert_eq!(no_key["error"]["data"]["kind"], "failed", "{no_key}");
    assert!(
        no_key["error"]["message"].as_str().unwrap().contains("key"),
        "{no_key}"
    );
    assert!(server.sessions.lock().unwrap().is_empty());

    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "live_api",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    let started = c.call("stream.start", none.clone());
    assert_eq!(started["result"]["video_encoder"], WORKING, "{started}");
    assert!(started["result"]["audio_encoder"].is_string(), "{started}");
    // The UI shows it as if Go live was pressed.
    match stream(&rx) {
        StreamUpdate::Started { url, .. } => assert_eq!(url, URL),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *server.sessions.lock().unwrap(),
        [(URL.to_owned(), Some("live_api".to_owned()))]
    );
    let live = c.stream_state(|d| d["state"] == "live" && d["connected"] == true);
    let data = &live["params"]["data"];
    assert_eq!(data["protocol"], "rtmp", "{live}");
    assert_eq!(data["connects"], 1, "{live}");
    assert_eq!(data["video_encoder"], WORKING, "{live}");
    // The key is never shown.
    assert!(!live.to_string().contains("live_api"), "{live}");
    // The fake server sends no further connection change, so this is the latest state.
    let status = c.call("stream.status", none.clone());
    assert_eq!(status["result"]["data"]["state"], "live", "{status}");
    assert_eq!(
        status["result"]["seq"]["stream.state"], live["params"]["seq"],
        "{status}"
    );
    let again = c.call("stream.start", none.clone());
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");

    let stop = c.call("stream.stop", none.clone());
    assert!(stop["result"].is_object(), "{stop}");
    let idle = c.stream_state(|d| d["state"] == "idle");
    let last = &idle["params"]["data"]["last"];
    assert_eq!(last["failure"], serde_json::Value::Null, "{idle}");
    assert_eq!(last["connects"], 1, "{idle}");
    next(&rx, |u| match u {
        Update::Stream(StreamUpdate::Ended { failure, .. }) => {
            assert_eq!(failure, None);
            Some(())
        }
        _ => None,
    });
    let stop = c.call("stream.stop", none.clone());
    assert_eq!(stop["error"]["data"]["kind"], "conflict", "{stop}");

    // A stream the UI starts reaches API clients too, and so does its failure.
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    c.stream_state(|d| d["state"] == "live" && d["connected"] == true);
    server
        .events
        .lock()
        .unwrap()
        .push(TransportEvent::Disconnected {
            error: TransportError::Rejected("unknown stream key".into()),
        });
    let failed = c.stream_state(|d| d["state"] == "idle");
    let why = failed["params"]["data"]["last"]["failure"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(why.contains("unknown stream key"), "{failed}");

    drop(host);
    drop(api);
}

#[test]
fn stream_settings_are_read_and_changed_through_settings_but_never_the_server() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &TestClock::new());
    let (api, mut c) = api_client(&host);
    let none = serde_json::json!({});
    c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["settings.changed"]}),
    );

    // Before the UI sent its stream settings there are none to show or change.
    let early = c.call("settings.get", none.clone());
    assert!(early["result"]["data"].get("stream").is_none(), "{early}");
    let patch = serde_json::json!({"stream": {
        "reconnect": false,
        "srt_latency_ms": null,
        "encoding": {
            "encoder": null, "rate_control": "vbr", "video_kbps": 8000,
            "max_video_kbps": 12000, "quality": 23, "keyframe_seconds": 2,
            "profile": null, "preset": "quality", "b_frames": 2,
            "cbr_padding": false, "audio_kbps": 192
        }
    }});
    let refused = c.call("settings.set", patch.clone());
    assert_eq!(refused["error"]["data"]["kind"], "failed", "{refused}");

    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.key_saved = true;
    host.send(Command::SetStreamProfile(profile.clone()));
    add_display(&host, &rx);
    let got = c.call("settings.get", none.clone());
    let stream = &got["result"]["data"]["stream"];
    assert_eq!(stream["protocol"], "rtmp", "{got}");
    assert_eq!(stream["key_saved"], true, "{got}");
    assert_eq!(stream["reconnect"], true, "{got}");
    assert_eq!(stream["encoding"]["video_kbps"], 6000, "{got}");
    // The server is the local user's: no answer or event names it.
    assert!(!got.to_string().contains("live.example.com"), "{got}");

    // The settings `settings.get` returned can be sent back changed; the server and
    // the key stay, whatever the caller adds.
    let mut back = serde_json::json!({"stream": stream.clone()});
    back["stream"]["encoding"] = patch["stream"]["encoding"].clone();
    back["stream"]["reconnect"] = false.into();
    back["stream"]["server"] = "rtmp://elsewhere.example/app".into();
    back["stream"]["key_saved"] = false.into();
    let set = c.call("settings.set", back);
    let data = &set["result"]["data"]["stream"];
    assert_eq!(data["encoding"]["video_kbps"], 8000, "{set}");
    assert_eq!(data["encoding"]["rate_control"], "vbr", "{set}");
    assert_eq!(data["reconnect"], false, "{set}");
    assert_eq!(data["key_saved"], true, "{set}");
    // The UI is told, to show and save them.
    let saved = next(&rx, |u| match u {
        Update::StreamProfile(p) => Some(p),
        _ => None,
    });
    assert_eq!(saved.server, URL);
    assert!(saved.key_saved);
    assert!(!saved.reconnect);
    assert_eq!(saved.encoding.video_kbps, 8000);
    assert_eq!(saved.encoding.audio_kbps, 192);
    let event = c.event("settings.changed", |d| d["stream"]["reconnect"] == false);
    assert_eq!(
        event["params"]["data"]["stream"]["encoding"]["video_kbps"], 8000,
        "{event}"
    );
    assert!(!event.to_string().contains("live.example.com"), "{event}");

    // Settings that cannot be used are refused, and nothing changes.
    let mut bad = patch.clone();
    bad["stream"]["encoding"]["audio_kbps"] = 100.into();
    let refused = c.call("settings.set", bad);
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid_params",
        "{refused}"
    );
    let mut bad = patch.clone();
    bad["stream"]["srt_latency_ms"] = 0.into();
    let refused = c.call("settings.set", bad);
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid_params",
        "{refused}"
    );
    let got = c.call("settings.get", none.clone());
    assert_eq!(
        got["result"]["data"]["stream"]["encoding"]["audio_kbps"], 192,
        "{got}"
    );

    // While a stream runs its settings stay; the same settings are accepted.
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "k",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    let started = c.call("stream.start", none.clone());
    assert!(started["result"].is_object(), "{started}");
    let mut changed = patch.clone();
    changed["stream"]["encoding"]["video_kbps"] = 4000.into();
    let busy = c.call("settings.set", changed);
    assert_eq!(busy["error"]["data"]["kind"], "conflict", "{busy}");
    let same = c.call("settings.set", patch);
    assert!(same["result"].is_object(), "{same}");
    c.call("stream.stop", none);

    drop(host);
    drop(api);
}

/// A muxer that writes the packets' bytes between a header and a trailer.
struct FakeMuxerFactory;

impl AdapterFactory for FakeMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.muxer")
    }
    fn display_name(&self) -> &str {
        "test muxer"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(
            rbs_mux::caps::CONTAINER,
            CapabilityValue::Set(["matroska".to_owned()].into()),
        ))
    }
}

impl rbs_mux::MuxerFactory for FakeMuxerFactory {
    fn create(
        &self,
        _: &rbs_mux::MuxerConfig,
    ) -> Result<Box<dyn rbs_mux::Muxer>, rbs_mux::CreateError> {
        Ok(Box::new(FakeMuxer))
    }
}

struct FakeMuxer;

impl rbs_mux::Muxer for FakeMuxer {
    fn begin(&mut self, out: &mut Vec<rbs_core::media::MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(rbs_core::media::MuxChunk::new(
            rbs_core::media::ChunkKind::Header,
            Bytes::from_static(b"h"),
        ));
        Ok(())
    }
    fn write(
        &mut self,
        _: rbs_core::media::StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<rbs_core::media::MuxChunk>,
    ) -> Result<(), rbs_mux::MuxError> {
        out.push(rbs_core::media::MuxChunk::new(
            rbs_core::media::ChunkKind::Media {
                keyframe: packet.keyframe(),
                timestamp: packet.dts(),
            },
            packet.data().clone(),
        ));
        Ok(())
    }
    fn finish(
        &mut self,
        out: &mut Vec<rbs_core::media::MuxChunk>,
    ) -> Result<(), rbs_mux::MuxError> {
        out.push(rbs_core::media::MuxChunk::new(
            rbs_core::media::ChunkKind::Trailer,
            Bytes::from_static(b"t"),
        ));
        Ok(())
    }
}

#[test]
fn each_output_profile_has_its_own_stream_key() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &clock);
    add_display(&host, &rx);
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "default_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));

    // Another profile: the default profile's key is not its key.
    let b = rbs_app::stream_key::account("studio-b");
    host.send(Command::SetKeyAccount(b.clone()));
    host.send(Command::StartStream(request()));
    assert_eq!(stream(&rx), StreamUpdate::NoKey);
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "b_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert_eq!(keys.read(&b).unwrap().unwrap().expose(), "b_key");
    assert_eq!(keys.read(ACCOUNT).unwrap().unwrap().expose(), "default_key");

    // Deleting a profile removes its key only.
    host.send(Command::SetKeyAccount(ACCOUNT.to_owned()));
    host.send(Command::RemoveStreamKey(b.clone()));
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "default_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert!(keys.read(&b).unwrap().is_none());
    assert_eq!(keys.read(ACCOUNT).unwrap().unwrap().expose(), "default_key");
    assert!(server.sessions.lock().unwrap().is_empty());
    drop(host);
    while rx.recv_timeout(LIMIT).is_ok() {}
}
