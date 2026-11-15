//! The loudness meter through the host and control API 1.47 and 1.59 (ADR-0092):
//! `loudness.set_enabled` starts and stops it and is saved, `loudness.status` reads it
//! with the target and limit, `loudness.reset` restarts the measurement, the values
//! without data are left out, and `settings.set` `loudness` changes and saves the
//! target and limit. Reference adapters, an engine on a `TestClock`; a bound only as a hang guard
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
use rbs_app::loudness_settings::{self, LoudnessSettings};
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
    start_with(clock, None, LoudnessSettings::default()).map(|(h, u, _)| (h, u))
}

type Started = (
    StudioHost,
    Receiver<Update>,
    Option<rbs_app::studio_host::LoudnessView>,
);

fn start_with(
    clock: &TestClock,
    config_dir: Option<std::path::PathBuf>,
    loudness: LoudnessSettings,
) -> Option<Started> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let audio = Arc::new(TestAudioBackend::new());
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = Some(collection(SourceMonitor::Off));
    config.config_dir = config_dir;
    config.loudness = loudness;
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
    // Waits for the start and for the first loudness view, which the host sends
    // before or after `Started` (it publishes on its own loop).
    let deadline = Instant::now() + LIMIT;
    let mut started = false;
    let mut view = None;
    while !started || view.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match updates
            .recv_timeout(left)
            .expect("the host started and sent its loudness view")
        {
            Update::Started { .. } => started = true,
            Update::Failed(e) => panic!("{e}"),
            Update::Loudness(v) => view = Some(v),
            _ => {}
        }
    }
    Some((host, updates, view))
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

    // Off by default: no values, and nothing to reset; the target and limit are the
    // defaults (protocol 1.59).
    let off = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(
        off,
        json!({"enabled": false, "target": -14.0, "true_peak_limit": -1.0})
    );
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
    assert_eq!(ok(&c.call("loudness.status", json!({})))["enabled"], false);
    assert_eq!(
        kind(&c.call("loudness.set_enabled", json!({"on": 1}))),
        "invalid_params"
    );
    drop(host);
}

/// The last [`Update::Loudness`] among the updates so far.
fn last_view(updates: &Receiver<Update>) -> Option<rbs_app::studio_host::LoudnessView> {
    updates
        .try_iter()
        .filter_map(|u| match u {
            Update::Loudness(v) => Some(v),
            _ => None,
        })
        .last()
}

#[test]
fn show_loudness_the_target_and_the_limit_are_saved_and_start_again() {
    let dir = std::env::temp_dir().join(format!("rbs-loudness-host-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let Some((host, updates, early)) =
        start_with(&clock, Some(dir.clone()), LoudnessSettings::default())
    else {
        return;
    };
    // The UI hears once at the start: off, the defaults.
    let first = last_view(&updates)
        .or(early)
        .expect("the start's loudness view");
    assert!(!first.shown && first.readout.is_none());
    assert_eq!((first.target, first.true_peak_limit), (-14.0, -1.0));
    let (_api, mut c) = connect(&host);

    ok(&c.call("loudness.set_enabled", json!({"on": true})));
    let shown = last_view(&updates).expect("a loudness view after Show loudness");
    assert!(shown.shown && shown.readout.is_some());
    assert!(loudness_settings::load(&dir).0.shown);

    // `settings.set` `loudness`: checked first, then saved and shown.
    let bad = c.call(
        "settings.set",
        json!({"loudness": {"target": -60.0, "true_peak_limit": -1.0}}),
    );
    assert_eq!(kind(&bad), "invalid_params");
    let set = ok(&c.call(
        "settings.set",
        json!({"loudness": {"target": -23.0, "true_peak_limit": -2.0}}),
    ))
    .clone();
    assert_eq!(
        set["data"]["loudness"],
        json!({"target": -23.0, "true_peak_limit": -2.0}),
        "{set}"
    );
    let status = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(status["target"], -23.0, "{status}");
    assert_eq!(status["true_peak_limit"], -2.0, "{status}");
    let view = last_view(&updates).expect("a loudness view after the settings");
    assert_eq!((view.target, view.true_peak_limit), (-23.0, -2.0));
    let saved = loudness_settings::load(&dir);
    assert_eq!(saved.1, None);
    assert_eq!(
        (saved.0.shown, saved.0.target, saved.0.true_peak_limit),
        (true, -23.0, -2.0)
    );

    // A save that fails changes nothing: a folder in the way of the file's temporary
    // copy makes the write fail.
    let blocker = dir.join("loudness.toml.tmp");
    std::fs::create_dir_all(&blocker).expect("a folder in the way");
    let failed = c.call(
        "settings.set",
        json!({"loudness": {"target": -16.0, "true_peak_limit": -1.0}}),
    );
    assert_eq!(kind(&failed), "failed", "{failed}");
    let failed = c.call("loudness.set_enabled", json!({"on": false}));
    assert_eq!(kind(&failed), "failed", "{failed}");
    let status = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(status["enabled"], true, "{status}");
    assert_eq!(status["target"], -23.0, "{status}");
    let view = last_view(&updates);
    assert!(view.is_none(), "the UI hears no change: {view:?}");
    std::fs::remove_dir(&blocker).expect("the folder removed");
    drop(c);
    drop(host);

    // A new start with the saved settings measures again at once.
    let Some((host, updates, early)) = start_with(&clock, Some(dir.clone()), saved.0) else {
        return;
    };
    let again = last_view(&updates)
        .or(early)
        .expect("the start's loudness view");
    assert!(again.shown && again.readout.is_some());
    let (_api, mut c) = connect(&host);
    let status = ok(&c.call("loudness.status", json!({}))).clone();
    assert_eq!(status["enabled"], true, "{status}");
    assert_eq!(status["target"], -23.0, "{status}");
    drop(c);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}
