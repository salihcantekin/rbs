//! The control API served by the studio's control thread (ADR-0044): a same-user IPC
//! client lists and adds sources, reads and changes settings, subscribes to events and
//! is refused a recording on a system without encoders. In-memory transport, reference
//! adapters, an engine on a `TestClock`; waits end when the answer arrives, with a
//! bound only as a hang guard (ADR-0032). Needs a GPU adapter (software is fine);
//! skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::studio_host::{Command, HostConfig, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
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
    /// Calls a method and returns its answer; events that arrive meanwhile are kept.
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

fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if let Some(t) = pick(rx.recv_timeout(left).expect("the host answered")) {
            return t;
        }
    }
}

#[test]
fn a_control_api_client_drives_the_studio() {
    let Some(gpu) = gpu() else { return };
    let (tx, updates) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
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
            (a, Vec::new())
        },
        move || Ok(gpu),
        || Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default()),
        move |u| {
            let _ = tx.send(u);
        },
    )
    .unwrap();
    next(&updates, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });

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
        json!({"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}),
    );
    assert!(
        hello["result"]["scopes"].as_array().unwrap().len() == 5,
        "{hello}"
    );

    let studio = c.call("studio.get", json!({}));
    assert_eq!(studio["result"]["data"]["canvas"]["width"], 1920);
    assert_eq!(studio["result"]["data"]["canvas"]["fps_num"], 30);
    // The reference adapters have no video encoder.
    assert!(studio["result"]["data"]["cannot_record"].is_string());

    let sub = c.call(
        "events.subscribe",
        json!({"topics": ["source.changed", "settings.changed", "record.state", "canvas.changed"]}),
    );
    assert_eq!(sub["result"]["seq"]["source.changed"], 0);

    let targets = c.call("source.targets", json!({"kind": "screen"}));
    let t = &targets["result"]["targets"][0];
    let added = c.call(
        "source.add",
        json!({"kind": "screen", "adapter": t["adapter"], "target": t["target"], "name": "Main screen"}),
    );
    let id = added["result"]["source"].as_u64().unwrap();
    let event = c.event("source.changed", |d| {
        d.as_array().is_some_and(|a| a.len() == 1)
    });
    assert_eq!(event["params"]["seq"], 1);
    assert_eq!(event["params"]["data"][0]["name"], "Main screen");
    assert_eq!(event["params"]["data"][0]["kind"], "screen");
    // The UI sees the same source.
    next(&updates, |u| match u {
        Update::Sources(rows) if rows.len() == 1 => Some(()),
        _ => None,
    });
    let list = c.call("source.list", json!({}));
    assert_eq!(list["result"]["seq"]["source.changed"], 1);
    assert_eq!(list["result"]["data"][0]["id"], id);

    // Shape and border (protocol 1.3, ADR-0053): the item and layout are listed, a
    // style is set on the item, published, and undone like any edit.
    let row = &list["result"]["data"][0];
    let item = row["item"].as_u64().unwrap();
    assert_eq!(
        row["layout"]["style"],
        json!({"shape": "rectangle", "radius": 0.0})
    );
    let set = c.call(
        "item.set_style",
        json!({"item": item, "style": {"shape": "circle", "border": {"width": 6.0, "color": "#FF0080"}}}),
    );
    assert!(set["result"].is_object(), "{set}");
    let styled = c.event("source.changed", |d| {
        d[0]["layout"]["style"]["shape"] == "circle"
    });
    assert_eq!(
        styled["params"]["data"][0]["layout"]["style"]["border"],
        json!({"width": 6.0, "color": "#ff0080ff"})
    );
    for (params, kind) in [
        (
            json!({"item": item, "style": {"shape": "circle", "radius": 4.0}}),
            "invalid_params",
        ),
        (
            json!({"item": item, "style": {"radius": -1.0}}),
            "invalid_params",
        ),
        (
            json!({"item": item, "style": {"border": {"width": 2.0, "color": "red"}}}),
            "invalid_params",
        ),
        (json!({"item": 999, "style": {}}), "not_found"),
    ] {
        let bad = c.call("item.set_style", params.clone());
        assert_eq!(bad["error"]["data"]["kind"], kind, "{params}: {bad}");
    }
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| {
        d[0]["layout"]["style"]["shape"] == "rectangle"
    });

    let unknown = c.call(
        "source.add",
        json!({"kind": "screen", "adapter": "nope", "target": "x"}),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not_found");

    // Settings: the UI is told and saves them.
    let settings = c.call("settings.get", json!({}));
    let mut profile = settings["result"]["data"]["profiles"][0].clone();
    assert_eq!(profile["id"], "default");
    profile["audio_kbps"] = json!(128);
    let set = c.call("settings.set", json!({"profiles": [profile]}));
    assert_eq!(set["result"]["data"]["profiles"][0]["audio_kbps"], 128);
    let p = next(&updates, |u| match u {
        Update::Profile(p) => Some(p),
        _ => None,
    });
    assert_eq!(p.audio_kbps, 128);
    c.event("settings.changed", |d| {
        d["profiles"][0]["audio_kbps"] == 128
    });
    profile["audio_kbps"] = json!(100);
    let bad = c.call("settings.set", json!({"profiles": [profile]}));
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params");

    // No encoder: the recording is refused with the reason, and nothing records.
    let rec = c.call("record.start", json!({"profile": "default"}));
    assert_eq!(rec["error"]["data"]["kind"], "failed", "{rec}");
    let status = c.call("record.status", json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle");
    let other = c.call("record.start", json!({"profile": "other"}));
    assert_eq!(other["error"]["data"]["kind"], "not_found");
    let stop = c.call("record.stop", json!({}));
    assert_eq!(stop["error"]["data"]["kind"], "conflict", "{stop}");
    let relative = c.call(
        "record.start_to_file",
        json!({"profile": "default", "path": "relative.mkv"}),
    );
    assert_eq!(
        relative["error"]["data"]["kind"], "invalid_params",
        "{relative}"
    );
    let existing = std::env::current_exe().unwrap();
    let exists = c.call(
        "record.start_to_file",
        json!({"profile": "default", "path": existing}),
    );
    assert_eq!(exists["error"]["data"]["kind"], "file_exists", "{exists}");

    // A UI edit reaches API clients.
    let mut ui_profile = p.clone();
    ui_profile.audio_kbps = 192;
    host.send(Command::SetProfile(ui_profile));
    c.event("settings.changed", |d| {
        d["profiles"][0]["audio_kbps"] == 192
    });

    // The canvas: the studio restarts, the UI and API clients are told.
    let canvas = c.call(
        "settings.set",
        json!({"canvas": {"width": 1280, "height": 720, "fps_num": 30, "fps_den": 1}}),
    );
    assert_eq!(
        canvas["result"]["data"]["canvas"]["width"], 1280,
        "{canvas}"
    );
    c.event("canvas.changed", |d| d["width"] == 1280);
    next(&updates, |u| match u {
        Update::Canvas { size, .. } if size.width == 1280 => Some(()),
        _ => None,
    });

    let del = c.call("source.delete", json!({"source": id}));
    assert!(del["result"].is_object(), "{del}");
    c.event("source.changed", |d| {
        d.as_array().is_some_and(Vec::is_empty)
    });
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| {
        d.as_array().is_some_and(|a| a.len() == 1)
    });

    // Redo deletes it again; the same undo stack as the app's own panels.
    let redo = c.call("edit.redo", json!({}));
    assert!(redo["result"].is_object(), "{redo}");
    c.event("source.changed", |d| {
        d.as_array().is_some_and(Vec::is_empty)
    });
    let nothing = c.call("edit.redo", json!({}));
    assert_eq!(nothing["error"]["data"]["kind"], "conflict", "{nothing}");
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| {
        d.as_array().is_some_and(|a| a.len() == 1)
    });

    // The canvas change before cleared the add: nothing older to undo.
    let none = c.call("edit.undo", json!({}));
    assert_eq!(none["error"]["data"]["kind"], "conflict", "{none}");

    // The host goes first while the server still holds its waker: the control thread
    // must end anyway. The drop runs on a helper thread so a regression fails the test
    // instead of hanging it.
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(host);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(LIMIT)
        .expect("the host stopped while the server held its waker");
    drop(api);
}
