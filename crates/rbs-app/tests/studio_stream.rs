//! Streaming through the studio host (Go live): the key read from the key store reaches
//! the transport only, a missing key refuses before anything opens, connection changes
//! reach the UI, one output runs at a time, and the stream ends cleanly or with the
//! server's reason. Reference sources, fake encoders and a fake RTMP transport whose
//! events the test injects. The engine runs on a `TestClock` that never moves; the
//! output worker's doorbell timer polls the transport. Waits are for the host's updates,
//! bounded only as a hang guard (ADR-0032). Needs a GPU adapter (software is fine);
//! skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rbs_app::stream_key::{KeyStore, MemoryKeyStore};
use rbs_app::studio_host::{
    Command, HostConfig, RecordRequest, RecordingUpdate, SourceKind, StreamRequest, StreamUpdate,
    StudioHost, Update,
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
use rbs_record::{RecordAdapters, VideoSettings};
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
) -> (StudioHost, Receiver<Update>) {
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
            (a, Vec::new())
        },
        move || Ok(gpu),
        || Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default()),
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
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server));
    add_display(&host, &rx);

    // RTMP without a saved key: nothing opens.
    host.send(Command::StartStream(request()));
    assert_eq!(stream(&rx), StreamUpdate::NoKey);
    assert!(server.sessions.lock().unwrap().is_empty());

    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "live_abc",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert_eq!(keys.read().unwrap().unwrap().expose(), "live_abc");

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

    // One output at a time, one stream at a time.
    host.send(Command::StartRecording(RecordRequest {
        path: std::env::temp_dir().join(format!("rbs-app-stream-{}.mkv", std::process::id())),
        container: ContainerId::Matroska,
        encoder: None,
        video: VideoSettings::default(),
        audio_bitrate_bps: 128_000,
    }));
    match next(&rx, |u| match u {
        Update::Recording(r) => Some(r),
        _ => None,
    }) {
        RecordingUpdate::NotStarted(why) => assert!(why.contains("End the stream"), "{why}"),
        other => panic!("{other:?}"),
    }
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

    host.send(Command::SaveStreamKey(None));
    assert_eq!(key_saved(&rx), Ok(false));
    assert!(keys.read().unwrap().is_none());
    drop(host);
}

#[test]
fn a_stream_the_server_refuses_ends_with_its_reason() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(&rbs_transport::Secret::new("wrong")).unwrap();
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server));
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
    match stream(&rx) {
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
    assert!(matches!(stream(&rx), StreamUpdate::Ended { .. }));
    drop(host);
}
