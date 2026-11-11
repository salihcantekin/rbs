//! The monitor output through the host and control API 1.30 (ADR-0097 F1): a
//! collection with a monitored source opens the monitor device on the host's control
//! thread; `source.set_monitor`, `monitor.*` and `monitor.changed` drive and report it. Reference adapters, an engine on a `TestClock`; waits end
//! when a count is reached, with a bound only as a hang guard (ADR-0032). Needs a GPU
//! adapter (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

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

fn start(monitor: SourceMonitor) -> Option<(StudioHost, Receiver<Update>, Arc<TestAudioBackend>)> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let audio = Arc::new(TestAudioBackend::new());
    let backend = Arc::clone(&audio);
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = Some(collection(monitor));
    let host = StudioHost::spawn(
        config,
        move || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources.register_audio(backend).unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
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
    Some((host, updates, audio))
}

/// Waits (hang guard only) until `audio` has `streams` active streams.
fn streams_reach(audio: &TestAudioBackend, streams: usize) -> bool {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if audio.active_streams() == streams {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

#[test]
fn a_monitored_source_opens_the_monitor_device() {
    let Some((host, _updates, audio)) = start(SourceMonitor::Only) else {
        return;
    };
    // The microphone's input and the monitor's output.
    assert!(
        streams_reach(&audio, 2),
        "{} streams",
        audio.active_streams()
    );
    drop(host);
    assert!(streams_reach(&audio, 0));
}

#[test]
fn without_a_monitored_source_no_device_opens() {
    let Some((host, _updates, audio)) = start(SourceMonitor::Off) else {
        return;
    };
    // The microphone runs; nothing else opens.
    assert!(streams_reach(&audio, 1));
    drop(host);
    assert!(streams_reach(&audio, 0));
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

    /// The next event of `topic` with data accepted by `ok`.
    fn event(&mut self, topic: &str, ok: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self
            .events
            .iter()
            .position(|e| e["params"]["topic"] == topic && ok(&e["params"]["data"]))
        {
            return self.events.remove(i);
        }
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an event");
            if v["params"]["topic"] == topic && ok(&v["params"]["data"]) {
                return v;
            }
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
    assert_eq!(hello["result"]["protocol"], "1.33", "{hello}");
    (api, c)
}

#[test]
fn the_control_api_sets_and_reports_the_monitor() {
    let Some((host, _updates, audio)) = start(SourceMonitor::Off) else {
        return;
    };
    let (_api, mut c) = connect(&host);
    let status = c.call("monitor.status", json!({}));
    let data = &ok(&status)["data"];
    assert_eq!(data["available"], true);
    assert_eq!(data["wanted"], false);
    assert_eq!(data["playing"], false);
    assert_eq!(data["device"], Value::Null);
    assert_eq!(data["volume"], 1.0);

    let devices = c.call("monitor.devices", json!({}));
    let devices = ok(&devices)["devices"].as_array().unwrap().clone();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device"], rbs_audio_io_test::OUTPUT);
    assert_eq!(devices[0]["is_default"], true);

    let rows = c.call("source.list", json!({}));
    let mic = ok(&rows)["data"][0]["id"].as_u64().unwrap();
    assert!(ok(&rows)["data"][0].get("monitor").is_none());
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["monitor.changed", "source.changed"]}),
    ));

    // Monitoring the microphone opens the device.
    ok(&c.call(
        "source.set_monitor",
        json!({"source": mic, "monitor": "only"}),
    ));
    c.event("source.changed", |d| d[0]["monitor"] == "only");
    c.event("monitor.changed", |d| d["playing"] == true);
    assert!(streams_reach(&audio, 2));
    assert_eq!(
        kind(&c.call(
            "source.set_monitor",
            json!({"source": 999, "monitor": "also"})
        )),
        "not_found"
    );

    // Volume and device are checked; a known device is taken.
    ok(&c.call("monitor.set_volume", json!({"volume": 0.5})));
    c.event("monitor.changed", |d| d["volume"] == 0.5);
    assert_eq!(
        kind(&c.call("monitor.set_volume", json!({"volume": 2.0}))),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call("monitor.set_device", json!({"device": "gone"}))),
        "not_found"
    );
    ok(&c.call(
        "monitor.set_device",
        json!({"device": rbs_audio_io_test::OUTPUT}),
    ));
    c.event("monitor.changed", |d| {
        d["device"] == rbs_audio_io_test::OUTPUT && d["playing"] == true
    });

    // Solo alone keeps the monitor wanted; ending it and monitoring closes the device.
    ok(&c.call(
        "source.set_monitor",
        json!({"source": mic, "monitor": "off"}),
    ));
    ok(&c.call("monitor.set_solo", json!({"sources": [mic]})));
    c.event("monitor.changed", |d| {
        d["solo"] == json!([mic]) && d["wanted"] == true
    });
    assert_eq!(
        kind(&c.call("monitor.set_solo", json!({"sources": [999]}))),
        "not_found"
    );
    ok(&c.call("monitor.set_solo", json!({"sources": []})));
    c.event("monitor.changed", |d| {
        d["wanted"] == false && d["playing"] == false && d.get("solo").is_none()
    });
    assert!(streams_reach(&audio, 1));

    // A deleted source leaves solo.
    ok(&c.call("monitor.set_solo", json!({"sources": [mic]})));
    c.event("monitor.changed", |d| d["solo"] == json!([mic]));
    ok(&c.call("source.delete", json!({"source": mic})));
    c.event("monitor.changed", |d| d.get("solo").is_none());
}

/// Waits (hang guard only) for a monitor status that `want` accepts.
fn monitor_until(
    updates: &Receiver<Update>,
    want: impl Fn(&rbs_api::types::MonitorStatus) -> bool,
) -> rbs_api::types::MonitorStatus {
    let deadline = Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Update::Monitor(s) = updates.recv_timeout(left).expect("a monitor update")
            && want(&s)
        {
            return s;
        }
    }
}

#[test]
fn the_ui_commands_drive_the_monitor_and_report_it() {
    use rbs_app::studio_host::Command;
    use rbs_scene::model::SourceId;
    let Some((host, updates, audio)) = start(SourceMonitor::Off) else {
        return;
    };
    // The monitor's state is sent once at the start (nothing monitored), the Mixer's
    // rows give the microphone's id, and the devices answer the list request.
    host.send(Command::ListMonitorDevices);
    let deadline = Instant::now() + LIMIT;
    let (mut status, mut mic, mut devices) = (None, None, None);
    while status.is_none() || mic.is_none() || devices.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match updates
            .recv_timeout(left)
            .expect("the state, the rows and the devices")
        {
            Update::Monitor(s) => status = Some(s),
            Update::Heard(rows) if !rows.is_empty() => mic = Some(rows[0].id),
            Update::MonitorDevices(d) => devices = Some(d),
            _ => {}
        }
    }
    let s = status.unwrap();
    assert!(s.available && !s.wanted && !s.playing);
    let mic: SourceId = mic.unwrap();
    assert!(!devices.unwrap().is_empty());
    host.send(Command::SetSourceMonitor {
        id: mic,
        monitor: SourceMonitor::Also,
    });
    monitor_until(&updates, |s| s.playing);
    assert!(streams_reach(&audio, 2));
    host.send(Command::SetMonitorVolume(0.5));
    monitor_until(&updates, |s| s.volume == 0.5);
    // Solo keeps the monitor wanted after the source's own monitor goes off.
    host.send(Command::SetMonitorSolo(vec![mic]));
    monitor_until(&updates, |s| s.solo == [mic.get()]);
    host.send(Command::SetSourceMonitor {
        id: mic,
        monitor: SourceMonitor::Off,
    });
    host.send(Command::SetMonitorSolo(Vec::new()));
    monitor_until(&updates, |s| !s.wanted && !s.playing && s.solo.is_empty());
    assert!(streams_reach(&audio, 1));
    drop(host);
}
