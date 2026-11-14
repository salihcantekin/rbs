//! The loudness meter through the host and control API 1.51 (ADR-0092 stage 1):
//! `loudness.set_enabled` starts and stops it, `loudness.status` reads it and
//! `loudness.reset` restarts the measurement, and the values without data are left
//! out. Reference adapters, an engine on a `TestClock`; a bound only as a hang guard
//! (ADR-0032). Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::studio_host::{HostConfig, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::node::NodeTypeId;
use rbs_record::RecordAdapters;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings, SourceMonitor};
use rbs_sources::types;
use serde_json::{Value, json};

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
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

/// One canvas whose program scene has a microphone with `monitor`.
fn collection(monitor: SourceMonitor) -> SceneCollection {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (mic, _) = col
        .add_source(
            "Microphone",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.set_source_monitor(mic, monitor).unwrap();
    col.add_item(
        scene,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();
    col
}

fn start(clock: &TestClock) -> Option<(StudioHost, Receiver<Update>)> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let audio = Arc::new(TestAudioBackend::new());
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = Some(collection(SourceMonitor::Off));
    let clock = clock.clone();
    let host = StudioHost::spawn(
        config,
        move || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources.register_audio(audio).unwrap();
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
    let deadline = Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match updates.recv_timeout(left).expect("the host started") {
            Update::Started { .. } => break,
            Update::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    Some((host, updates))
}

struct ChannelSender(Sender<Value>);

impl ConnectionSender for ChannelSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        self.0
            .send(serde_json::from_str(text).unwrap())
            .map_err(|_| SendError::Closed)
    }

    fn close(&mut self, _reason: CloseReason) {}
}

struct Client {
    handle: ConnectionHandle,
    rx: Receiver<Value>,
    next: i64,
    events: Vec<Value>,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let text = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an answer");
            if v["id"] == id {
                return v;
            }
            self.events.push(v);
        }
    }
}

fn ok(answer: &Value) -> &Value {
    assert!(answer.get("error").is_none(), "{answer}");
    &answer["result"]
}

fn kind(answer: &Value) -> &Value {
    &answer["error"]["data"]["kind"]
}

fn connect(host: &StudioHost) -> (ApiServer, Client) {
    let server = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
    let (api, endpoint) =
        ApiServer::start(server, TokenStore::new(), ApiOptions::new(host.api_waker())).unwrap();
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
    let hello = c.call(
        "session.hello",
        json!({"protocols": ["1.33"], "client": {"name": "test", "version": "1"}}),
    );
    assert_eq!(
        hello["result"]["protocol"],
        rbs_api::session::PROTOCOL_VERSION,
        "{hello}"
    );
    (api, c)
}

#[test]
fn the_control_api_starts_reads_resets_and_stops_the_loudness_meter() {
    let clock = TestClock::new();
    let Some((host, _updates)) = start(&clock) else {
        return;
    };
    let (_api, mut c) = connect(&host);

    // Off by default: no values, and nothing to reset.
    let off = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(off, json!({"enabled": false}));
    assert_eq!(kind(&c.call("loudness.reset", json!({}))), "conflict");

    ok(&c.call("loudness.set_enabled", json!({"on": true})));
    // Turning it on again keeps the measurement.
    ok(&c.call("loudness.set_enabled", json!({"on": true})));
    // On: the meter runs. Its values appear once audio has been measured; the
    // measurement itself is tested in `rbs-audio` (EBU conformance) and `rbs-record`
    // (the studio's tap on the mix), where the test drives the audio partition.
    let on = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(on["enabled"], true, "{on}");
    for v in [
        "momentary",
        "short_term",
        "integrated",
        "range",
        "true_peak_max",
    ] {
        assert!(on.get(v).is_none_or(Value::is_number), "{on}");
    }
    ok(&c.call("loudness.reset", json!({})));

    ok(&c.call("loudness.set_enabled", json!({"on": false})));
    assert_eq!(
        ok(&c.call("loudness.status", json!({}))).clone(),
        json!({"enabled": false})
    );
    assert_eq!(
        kind(&c.call("loudness.set_enabled", json!({"on": 1}))),
        "invalid_params"
    );
    drop(host);
}
