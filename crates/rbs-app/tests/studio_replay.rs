//! The replay buffer through the studio host: arm, hold the past, save a clip, disarm
//! (ADR-0088). The engine runs on a `TestClock` the test moves; it waits for the host's
//! updates, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::replay_settings::ReplaySetup;
use rbs_app::studio_host::{Command, Fault, HostConfig, SourceKind, SourceRow, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory,
    caps as codec_caps,
};
use rbs_codec_mf::testing::FakeAacFactory;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, EncodedPacket, MuxChunk, StreamDescriptor,
    StreamIndex, StreamParams, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory, caps as mux_caps};
use rbs_record::RecordAdapters;

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

fn rows_where(rx: &Receiver<Update>, mut ok: impl FnMut(&[SourceRow]) -> bool) -> Vec<SourceRow> {
    next(rx, |u| match u {
        Update::Sources(rows) if ok(&rows) => Some(rows),
        _ => None,
    })
}

#[test]
fn the_buffer_arms_holds_the_past_saves_a_clip_and_disarms() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
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

    let setup = ReplaySetup {
        folder: Some(dir.clone()),
        ..ReplaySetup::default()
    };
    host.send(Command::SetReplay(setup));
    host.send(Command::ArmReplay);
    // The buffer holds a past once frames were encoded: the test moves the clock.
    next_while(
        &rx,
        || clock.advance(Duration::from_millis(33)),
        |u| match u {
            Update::Replay(s) if s.armed && s.held_ms > 0 => Some(()),
            Update::Failed(e) => panic!("the studio failed: {e}"),
            _ => None,
        },
    );

    host.send(Command::SaveReplay { seconds: None });
    let path = next_while(
        &rx,
        || clock.advance(Duration::from_millis(33)),
        |u| match u {
            Update::ReplaySaved(r) => Some(r.expect("the clip was written")),
            _ => None,
        },
    );
    assert!(path.starts_with(&dir), "{path:?}");
    let written = std::fs::read(&path).unwrap();
    assert!(written.starts_with(HEADER), "{written:?}");
    assert!(written.ends_with(TRAILER), "{written:?}");

    host.send(Command::DisarmReplay);
    next_while(
        &rx,
        || clock.advance(Duration::from_millis(33)),
        |u| match u {
            Update::Replay(s) if !s.armed => Some(()),
            _ => None,
        },
    );
    drop(host);
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
            serde_json::json!({"protocols": ["1.27"], "client": {"name": "test", "version": "1"}}),
        );
        assert_eq!(hello["result"]["protocol"], "1.27", "{hello}");
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

fn kind(answer: &serde_json::Value) -> &str {
    answer["error"]["data"]["kind"].as_str().unwrap_or("none")
}

#[test]
fn a_control_api_client_arms_saves_and_disarms_the_buffer() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-replay-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
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
    host.send(Command::SetReplay(ReplaySetup {
        folder: Some(dir.clone()),
        ..ReplaySetup::default()
    }));
    let (api, mut c) = Client::connect(&host);
    let none = serde_json::json!({});

    // Disarmed: status says so, and a save or disarm is refused.
    let status = c.call("replay.status", none.clone());
    assert_eq!(status["result"]["data"]["armed"], false, "{status}");
    assert_eq!(kind(&c.call("replay.save", none.clone())), "conflict");
    assert_eq!(kind(&c.call("replay.disarm", none.clone())), "conflict");

    let armed = c.call("replay.arm", none.clone());
    assert!(armed.get("error").is_none(), "{armed}");
    assert_eq!(kind(&c.call("replay.arm", none.clone())), "conflict");
    // Out of the buffer's length.
    let bad = c.call("replay.save", serde_json::json!({"seconds": 0}));
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
    let bad = c.call("replay.save", serde_json::json!({"seconds": 100_000}));
    assert_eq!(kind(&bad), "invalid_params", "{bad}");

    // Wait until the buffer holds a second of past: the test moves the clock, and
    // listens to the host rather than asking the API again and again.
    next_while(
        &rx,
        || clock.advance(Duration::from_millis(33)),
        |u| match u {
            Update::Replay(s) if s.held_ms >= 1000 => Some(()),
            Update::Failed(e) => panic!("the studio failed: {e}"),
            _ => None,
        },
    );
    let saved = c.call("replay.save", serde_json::json!({"seconds": 1}));
    assert!(saved.get("error").is_none(), "{saved}");
    let path = next_while(
        &rx,
        || clock.advance(Duration::from_millis(33)),
        |u| match u {
            Update::ReplaySaved(r) => Some(r.expect("the clip was written")),
            _ => None,
        },
    );
    assert!(path.starts_with(&dir) && path.exists(), "{path:?}");
    let status = c.call("replay.status", none.clone());
    assert_eq!(
        status["result"]["data"]["last_saved"]
            .as_str()
            .map(PathBuf::from),
        Some(path),
        "{status}"
    );

    let off = c.call("replay.disarm", none.clone());
    assert!(off.get("error").is_none(), "{off}");
    let status = c.call("replay.status", none);
    assert_eq!(status["result"]["data"]["armed"], false, "{status}");
    drop((api, host));
    let _ = std::fs::remove_dir_all(&dir);
}
