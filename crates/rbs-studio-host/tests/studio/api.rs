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
use rbs_api::types::{Library, LibraryEntry};
use rbs_api::{ApiServer, TokenStore};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_studio_host::studio_host::{Command, HostConfig, SourceKind, StudioHost, Update};
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
        rbs_crash::errln!("SKIPPED: no GPU adapter");
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

/// Sends a UI command and waits until the control thread took it. UI commands and API
/// requests reach the control thread on separate queues, so a request sent after a
/// command may still be served first; a `ListTargets` sent behind the command is
/// answered only after it.
/// A target just picked is listed as the newest recent one (protocol 1.101); a field
/// it did not have before is left out.
fn listed_as_recent(c: &mut Client, t: &Value) {
    assert!(
        t.get("recent").is_none() && t.get("picker").is_none(),
        "{t}"
    );
    let again = c.call("source.targets", json!({"kind": "screen"}));
    let row = again["result"]["targets"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["target"] == t["target"]))
        .cloned()
        .unwrap();
    assert_eq!(row["recent"], 0, "{row}");
}

fn send_and_wait(host: &StudioHost, updates: &Receiver<Update>, command: Command) {
    host.send(command);
    host.send(Command::ListTargets(SourceKind::Screen));
    next(updates, |u| match u {
        Update::Targets(SourceKind::Screen, _) => Some(()),
        _ => None,
    });
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

    let t = &c.call("source.targets", json!({"kind": "screen"}))["result"]["targets"][0];
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
    listed_as_recent(&mut c, t);
    // The UI sees the same source.
    next(&updates, |u| match u {
        Update::Sources(rows) if rows.len() == 1 => Some(()),
        _ => None,
    });
    let list = c.call("source.list", json!({}));
    assert_eq!(list["result"]["seq"]["source.changed"], 1);
    assert_eq!(list["result"]["data"][0]["id"], id);

    // Source settings (protocol 1.17, ADR-0077): read, patch, refuse what the type does
    // not take, and a null removes a key.
    let got = c.call("source.get_settings", json!({"source": id}));
    assert_eq!(got["result"]["settings"]["target"], t["target"], "{got}");
    let set = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"fps": 60, "cursor": false}}),
    );
    assert!(set["result"].is_object(), "{set}");
    let got = c.call("source.get_settings", json!({"source": id}));
    assert_eq!(got["result"]["settings"]["fps"], 60);
    assert_eq!(got["result"]["settings"]["cursor"], false);
    for patch in [
        json!({"fps": "fast"}),
        json!({"width": 1280}),
        json!({"bogus": 1}),
        json!({"fps": 1000}),
    ] {
        let bad = c.call(
            "source.set_settings",
            json!({"source": id, "settings": patch}),
        );
        assert_eq!(
            bad["error"]["data"]["kind"], "invalid_params",
            "{patch}: {bad}"
        );
    }
    let removed = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"cursor": null}}),
    );
    assert!(removed["result"].is_object(), "{removed}");
    let got = c.call("source.get_settings", json!({"source": id}));
    assert!(got["result"]["settings"].get("cursor").is_none(), "{got}");
    let missing = c.call("source.get_settings", json!({"source": 999}));
    assert_eq!(missing["error"]["data"]["kind"], "not_found", "{missing}");

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

    // Scrolling (protocol 1.94, ADR-0150): a scroll is set on the item, published in
    // its layout with the defaults left out, refused out of range, and undone like any
    // edit. The live calls refuse an item without a scroll and a speed out of range.
    let set = c.call(
        "item.set_scroll",
        json!({"item": item, "scroll": {"speed": 120.0, "repeat": "once"}}),
    );
    assert!(set["result"].is_object(), "{set}");
    let scrolled = c.event("source.changed", |d| !d[0]["layout"]["scroll"].is_null());
    assert_eq!(
        scrolled["params"]["data"][0]["layout"]["scroll"],
        json!({"speed": 120.0, "repeat": "once"})
    );
    for (method, params) in [
        ("item.scroll_pause", json!({"item": item})),
        ("item.scroll_speed", json!({"item": item, "speed": 60.0})),
        ("item.scroll_resume", json!({"item": item})),
        (
            "item.scroll_restart",
            json!({"item": item, "canvas": "main"}),
        ),
    ] {
        let done = c.call(method, params);
        assert!(done["result"].is_object(), "{method}: {done}");
    }
    for (method, params, kind) in [
        (
            "item.set_scroll",
            json!({"item": item, "scroll": {"speed": 0.0}}),
            "invalid_params",
        ),
        (
            "item.set_scroll",
            json!({"item": item, "scroll": {"speed": 10.0, "gap": -1.0}}),
            "invalid_params",
        ),
        (
            "item.set_scroll",
            json!({"item": 999, "scroll": null}),
            "not_found",
        ),
        (
            "item.scroll_speed",
            json!({"item": item, "speed": 5000.0}),
            "invalid_params",
        ),
        ("item.scroll_restart", json!({"item": 999}), "not_found"),
    ] {
        let bad = c.call(method, params.clone());
        assert_eq!(
            bad["error"]["data"]["kind"], kind,
            "{method} {params}: {bad}"
        );
    }
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| d[0]["layout"]["scroll"].is_null());
    let none = c.call("item.scroll_resume", json!({"item": item}));
    assert_eq!(none["error"]["data"]["kind"], "invalid_params", "{none}");

    // Crop (protocol 1.5, ADR-0058). Nothing shows or records the display here, so
    // its picture size was never seen: keeping the scale is refused, a plain crop is
    // taken and undone like any edit.
    assert!(row.get("picture_size").is_none(), "{row}");
    let kept = c.call(
        "item.set_crop",
        json!({"item": item, "crop": [10, 0, 0, 0]}),
    );
    assert_eq!(kept["error"]["data"]["kind"], "failed", "{kept}");
    let set = c.call(
        "item.set_crop",
        json!({"item": item, "crop": [10, 20, 30, 40], "keep_scale": false}),
    );
    assert!(set["result"].is_object(), "{set}");
    c.event("source.changed", |d| {
        d[0]["layout"]["crop"] == json!([10, 20, 30, 40])
    });
    for (params, kind) in [
        (json!({"item": 999, "crop": [0, 0, 0, 0]}), "not_found"),
        (
            json!({"item": item, "crop": [0, 0, 0, 0], "scale": 1}),
            "invalid_params",
        ),
    ] {
        let bad = c.call("item.set_crop", params.clone());
        assert_eq!(bad["error"]["data"]["kind"], kind, "{params}: {bad}");
    }
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| {
        d[0]["layout"]["crop"] == json!([0, 0, 0, 0])
    });

    // Transform commands (protocol 1.18, ADR-0084): flip, rotate, centre, one undo
    // step each; fit needs a picture size this test never sees.
    let flip = c.call("item.flip", json!({"item": item, "axis": "horizontal"}));
    assert!(flip["result"].is_object(), "{flip}");
    c.event("source.changed", |d| {
        d[0]["layout"]["size"][0].as_f64().is_some_and(|w| w < 0.0)
    });
    let undo = c.call("edit.undo", json!({}));
    assert!(undo["result"].is_object(), "{undo}");
    c.event("source.changed", |d| {
        d[0]["layout"]["size"][0].as_f64().is_some_and(|w| w > 0.0)
    });
    let turn = c.call("item.rotate", json!({"item": item, "degrees": 90}));
    assert!(turn["result"].is_object(), "{turn}");
    c.event("source.changed", |d| d[0]["layout"]["rotation"] == 90.0);
    let centre = c.call("item.align", json!({"item": item, "axis": "vertical"}));
    assert!(centre["result"].is_object(), "{centre}");
    let fit = c.call("item.fit", json!({"item": item, "mode": "fit"}));
    assert_eq!(fit["error"]["data"]["kind"], "invalid_params", "{fit}");
    for (method, params, kind) in [
        (
            "item.rotate",
            json!({"item": item, "degrees": 45}),
            "invalid_params",
        ),
        (
            "item.flip",
            json!({"item": 999, "axis": "vertical"}),
            "not_found",
        ),
        (
            "item.flip",
            json!({"item": item, "axis": "diagonal"}),
            "invalid_params",
        ),
    ] {
        let bad = c.call(method, params.clone());
        assert_eq!(bad["error"]["data"]["kind"], kind, "{params}: {bad}");
    }

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

    // Output size (protocol 1.39): omitted is the canvas. A fitted size is stored,
    // omitting it again clears it, and a size that does not fit is refused.
    let settings = c.call("settings.get", json!({}));
    let mut profile = settings["result"]["data"]["profiles"][0].clone();
    assert!(profile.get("output_size").is_none(), "{profile}");
    profile["output_size"] = json!({"width": 1280, "height": 720});
    let set = c.call("settings.set", json!({"profiles": [profile.clone()]}));
    assert_eq!(
        set["result"]["data"]["profiles"][0]["output_size"]["width"], 1280,
        "{set}"
    );
    assert_eq!(
        set["result"]["data"]["profiles"][0]["output_size"]["height"], 720,
        "{set}"
    );
    let p = next(&updates, |u| match u {
        Update::Profile(p) => Some(p),
        _ => None,
    });
    assert_eq!(
        p.output_size.map(|s| (s.width, s.height)),
        Some((1280, 720))
    );
    profile.as_object_mut().unwrap().remove("output_size");
    let cleared = c.call("settings.set", json!({"profiles": [profile.clone()]}));
    assert!(
        cleared["result"]["data"]["profiles"][0]
            .get("output_size")
            .is_none(),
        "{cleared}"
    );
    profile["output_size"] = json!(null);
    let nulled = c.call("settings.set", json!({"profiles": [profile.clone()]}));
    assert!(nulled["result"].is_object(), "{nulled}");
    assert!(
        nulled["result"]["data"]["profiles"][0]
            .get("output_size")
            .is_none(),
        "{nulled}"
    );
    profile["output_size"] = json!({"width": 1280, "height": 800});
    let bad = c.call("settings.set", json!({"profiles": [profile]}));
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params", "{bad}");
    let still = c.call("settings.get", json!({}));
    assert!(
        still["result"]["data"]["profiles"][0]
            .get("output_size")
            .is_none(),
        "{still}"
    );

    // Output profiles and scene collections (protocol 1.4): `failed` until the UI
    // sent its libraries, then listed; a switch is checked here and handed to the UI,
    // which answers with the libraries again.
    let early = c.call("profile.list", json!({}));
    assert_eq!(early["error"]["data"]["kind"], "failed", "{early}");
    let library = |ids: &[(&str, &str)], current: &str| Library {
        entries: ids
            .iter()
            .map(|(id, name)| LibraryEntry {
                id: (*id).into(),
                name: (*name).into(),
            })
            .collect(),
        current: current.into(),
        pending: false,
    };
    let profiles = library(&[("default", "Default"), ("live", "Live")], "default");
    let collections = library(
        &[("default", "Default"), ("talk-show", "Talk show")],
        "default",
    );
    send_and_wait(
        &host,
        &updates,
        Command::SetLibrary {
            profiles: profiles.clone(),
            collections: collections.clone(),
            answered: None,
        },
    );
    let listed = c.call("collection.list", json!({}));
    assert_eq!(
        listed["result"],
        json!({"entries": [{"id": "default", "name": "Default"}, {"id": "talk-show", "name": "Talk show"}], "current": "default", "pending": false})
    );
    let listed = c.call("profile.list", json!({}));
    assert_eq!(listed["result"]["entries"][1]["id"], "live", "{listed}");
    let same = c.call("collection.switch", json!({"id": "default"}));
    assert!(same["result"].is_object(), "{same}");
    let missing = c.call("profile.switch", json!({"id": "nope"}));
    assert_eq!(missing["error"]["data"]["kind"], "not_found", "{missing}");
    let switch = c.call("collection.switch", json!({"id": "talk-show"}));
    assert!(switch["result"].is_object(), "{switch}");
    let seq = next(&updates, |u| match u {
        Update::SwitchRequested {
            seq,
            collection,
            id,
        } => {
            assert!(collection);
            assert_eq!(id, "talk-show");
            Some(seq)
        }
        _ => None,
    });
    // Until the UI answered this request: another switch and outputs wait, and the
    // list says a switch is being made. A list sent for something else does not end
    // it.
    let busy = c.call("profile.switch", json!({"id": "live"}));
    assert_eq!(busy["error"]["data"]["kind"], "conflict", "{busy}");
    let stream = c.call("stream.start", json!({}));
    assert_eq!(stream["error"]["data"]["kind"], "conflict", "{stream}");
    send_and_wait(
        &host,
        &updates,
        Command::SetLibrary {
            profiles: profiles.clone(),
            collections: collections.clone(),
            answered: None,
        },
    );
    let listed = c.call("collection.list", json!({}));
    assert_eq!(listed["result"]["pending"], true, "{listed}");
    // Refused by the app: answered, and the one in use stays.
    send_and_wait(
        &host,
        &updates,
        Command::SetLibrary {
            profiles,
            collections,
            answered: Some(seq),
        },
    );
    let listed = c.call("collection.list", json!({}));
    assert_eq!(listed["result"]["pending"], false, "{listed}");
    assert_eq!(listed["result"]["current"], "default", "{listed}");
    let switch = c.call("profile.switch", json!({"id": "live"}));
    assert!(switch["result"].is_object(), "{switch}");
    let seq = next(&updates, |u| match u {
        Update::SwitchRequested {
            seq,
            collection,
            id,
        } => {
            assert!(!collection);
            assert_eq!(id, "live");
            Some(seq)
        }
        _ => None,
    });
    send_and_wait(
        &host,
        &updates,
        Command::SetLibrary {
            profiles: library(&[("default", "Default"), ("live", "Live")], "live"),
            collections: library(&[("default", "Default")], "default"),
            answered: Some(seq),
        },
    );
    let listed = c.call("profile.list", json!({}));
    assert_eq!(listed["result"]["current"], "live", "{listed}");
    assert_eq!(listed["result"]["pending"], false, "{listed}");

    // No encoder: the recording is refused with the reason, and nothing records.
    let rec = c.call("record.start", json!({"profile": "default"}));
    assert_eq!(rec["error"]["data"]["kind"], "failed", "{rec}");
    let status = c.call("record.status", json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle");
    let other = c.call("record.start", json!({"profile": "other"}));
    assert_eq!(other["error"]["data"]["kind"], "not_found");
    let stop = c.call("record.stop", json!({}));
    assert_eq!(stop["error"]["data"]["kind"], "conflict", "{stop}");
    // Nothing records: nothing to pause or resume (protocol 1.61).
    for method in ["record.pause", "record.resume", "record.pause_toggle"] {
        let none = c.call(method, json!({}));
        assert_eq!(
            none["error"]["data"]["kind"], "conflict",
            "{method}: {none}"
        );
        let all = c.call(method, json!({"canvas": "all"}));
        assert_eq!(all["error"]["data"]["kind"], "conflict", "{method}: {all}");
    }
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

/// Text sources through the control API (protocol 1.75, ADR-0129 decision 8): added
/// with their text, edited with `source.set_text` and `source.set_settings`, their
/// `file` refused, the font list read from the worker.
/// The id `source.add` answered, once its text row shows a measured box: a new text
/// source starts with the default box and is measured off the control thread.
fn measured(updates: &Receiver<Update>, added: &Value) -> u64 {
    let id = added["result"]["source"].as_u64().expect("a source id");
    let default = rbs_scene::model::SettingValue::Int(i64::from(rbs_sources::text::DEFAULT_BOX[0]));
    next(updates, |u| match u {
        Update::Sources(rows) => rows
            .iter()
            .any(|r| {
                u64::from(r.id.get()) == id
                    && r.text
                        .as_ref()
                        .and_then(|t| t.get("width"))
                        .is_some_and(|w| *w != default)
            })
            .then_some(id),
        _ => None,
    })
}

#[test]
fn a_control_api_client_adds_and_edits_a_text_source() {
    let Some(gpu) = gpu() else { return };
    let (tx, updates) = mpsc::channel();
    let clock = TestClock::new();
    let engine_clock = clock.clone();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_text(Arc::new(rbs_text_test::FakeTextRenderer::new()));
            (a, Vec::new())
        },
        move || Ok(gpu),
        move || {
            Engine::with_clock(
                Arc::new(engine_clock.clone()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
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
    c.call(
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}),
    );

    // Added with the default box; the test waits for the measure that replaces it.
    let added = c.call("source.add", json!({"kind": "text", "text": "Hello"}));
    let id = measured(&updates, &added);
    let got = c.call("source.get_settings", json!({"source": id}));
    let settings = &got["result"]["settings"];
    assert_eq!(settings["text"], "Hello", "{got}");
    assert!(settings["width"].as_u64().is_some_and(|w| w > 0), "{got}");
    assert!(settings["height"].as_u64().is_some_and(|h| h > 0), "{got}");
    let list = c.call("source.list", json!({}));
    assert_eq!(list["result"]["data"][0]["kind"], "text", "{list}");
    assert_eq!(list["result"]["data"][0]["name"], "Text", "{list}");

    // Source activity (ADR-0152, protocol 1.95): a text source runs while placed and
    // has no return choice; a value it does not allow is refused, a linger is kept and
    // an empty call resets.
    let activity = c.call("source.get_activity", json!({"source": id}));
    assert_eq!(
        activity["result"],
        json!({"source": id, "effective": {"run": "placed", "linger_s": 0},
               "supported": true, "runs": ["placed"], "on_returns": []}),
        "{activity}"
    );
    let refused = c.call(
        "source.set_activity",
        json!({"source": id, "run": "on_air"}),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid_params",
        "{refused}"
    );
    let lingered = c.call("source.set_activity", json!({"source": id, "linger_s": 30}));
    assert!(lingered["result"].is_object(), "{lingered}");
    let activity = c.call("source.get_activity", json!({"source": id}));
    assert_eq!(activity["result"]["linger_s"], 30, "{activity}");
    assert_eq!(
        activity["result"]["effective"]["linger_s"], 30,
        "{activity}"
    );
    let reset = c.call("source.set_activity", json!({"source": id}));
    assert!(reset["result"].is_object(), "{reset}");
    let activity = c.call("source.get_activity", json!({"source": id}));
    assert!(activity["result"].get("linger_s").is_none(), "{activity}");
    let missing = c.call("source.get_activity", json!({"source": 999}));
    assert_eq!(missing["error"]["data"]["kind"], "not_found", "{missing}");

    // `text` is only for a text source.
    let bad = c.call(
        "source.add",
        json!({"kind": "screen", "adapter": "a", "target": "t", "text": "x"}),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params", "{bad}");

    // `source.set_text` replaces only the text.
    let set = c.call("source.set_text", json!({"source": id, "text": "Bye"}));
    assert!(set["result"].is_object(), "{set}");
    let got = c.call("source.get_settings", json!({"source": id}));
    assert_eq!(got["result"]["settings"]["text"], "Bye", "{got}");
    assert_eq!(
        got["result"]["settings"]["width"], settings["width"],
        "{got}"
    );
    let missing = c.call("source.set_text", json!({"source": 999, "text": "x"}));
    assert_eq!(missing["error"]["data"]["kind"], "not_found", "{missing}");

    // Settings are checked strictly, and the API cannot point a text at a file.
    let ok = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"size": 48}}),
    );
    assert!(ok["result"].is_object(), "{ok}");
    let out_of_range = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"size": 5000}}),
    );
    assert_eq!(
        out_of_range["error"]["data"]["kind"], "invalid_params",
        "{out_of_range}"
    );
    let file = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"file": "/etc/hosts"}}),
    );
    assert_eq!(file["error"]["data"]["kind"], "forbidden", "{file}");
    assert!(
        !file["error"]["message"].as_str().unwrap().contains("/etc"),
        "{file}"
    );
    window_passes_the_text_file_rule(&host, &updates, id as u32);
    // A box edit makes the text crisp (ADR-0129 decision 12): the box and the font
    // size double with the item, in one undo step.
    let before = c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
    let (w, h) = (
        before["width"].as_f64().unwrap(),
        before["height"].as_f64().unwrap(),
    );
    let placed = c.call(
        "item.set_box",
        json!({"source": id, "position": [0.0, 0.0], "size": [w * 2.0, h * 2.0]}),
    );
    assert_eq!(placed["result"]["ok"], true, "{placed}");
    let after = c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
    assert_eq!(after["width"].as_f64(), Some(w * 2.0), "{after}");
    assert_eq!(after["height"].as_f64(), Some(h * 2.0), "{after}");
    assert_eq!(after["size"].as_f64(), Some(96.0), "{after}");
    let undone = c.call("edit.undo", json!({}));
    assert!(undone["result"].is_object(), "{undone}");
    let back = c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
    assert_eq!(back, before, "one undo step restores the box and the size");

    // A drag stretches; the box changes once the drag rests.
    let dragged = c.call(
        "item.set_box",
        json!({"source": id, "position": [0.0, 0.0], "size": [w * 3.0, h * 3.0], "gesture": 7}),
    );
    assert_eq!(dragged["result"]["ok"], true, "{dragged}");
    let stretched =
        c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
    assert_eq!(stretched, before, "not while the drag goes on");
    clock.advance(Duration::from_millis(300));
    let guard = std::time::Instant::now();
    loop {
        let now =
            c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
        if now["width"].as_f64() == Some(w * 3.0) {
            assert_eq!(now["size"].as_f64(), Some(144.0), "{now}");
            break;
        }
        assert!(guard.elapsed() < LIMIT, "the rested drag never got its box");
        std::thread::yield_now();
    }
    c.call("edit.undo", json!({}));
    let back = c.call("source.get_settings", json!({"source": id}))["result"]["settings"].clone();
    assert_eq!(back, before, "the drag and its box are one undo step");

    // `text.timer` (protocol 1.84) runs a stopwatch, not a plain text.
    let plain = c.call("text.timer", json!({"source": id, "action": "start"}));
    assert_eq!(plain["error"]["data"]["kind"], "invalid_params", "{plain}");
    let kind = c.call(
        "source.set_settings",
        json!({"source": id, "settings": {"kind": "stopwatch"}}),
    );
    assert!(kind["result"].is_object(), "{kind}");
    let started = c.call("text.timer", json!({"source": id, "action": "start"}));
    assert_eq!(started["result"]["kind"], "stopwatch", "{started}");
    assert_eq!(started["result"]["running"], true, "{started}");
    let reset = c.call("text.timer", json!({"source": id, "action": "reset"}));
    assert_eq!(reset["result"]["running"], false, "{reset}");
    assert_eq!(reset["result"]["value_ms"], 0, "{reset}");
    let gone = c.call("text.timer", json!({"source": 999, "action": "pause"}));
    assert_eq!(gone["error"]["data"]["kind"], "not_found", "{gone}");

    // The font list: `not_ready` until the worker has read it, then the families.
    let guard = std::time::Instant::now();
    let fonts = loop {
        let answer = c.call("text.fonts", json!({}));
        if answer["error"]["data"]["kind"] == "not_ready" {
            assert!(guard.elapsed() < LIMIT, "the font list never came");
            std::thread::yield_now();
            continue;
        }
        break answer;
    };
    assert_eq!(
        fonts["result"]["families"],
        json!(rbs_text_test::FAMILIES),
        "{fonts}"
    );

    drop(host);
    drop(api);
}

/// The window is the person at this computer: the text-file rule is a caller check its
/// own `source.set_settings` does not meet (ADR-0164 decision 7).
fn window_passes_the_text_file_rule(host: &StudioHost, updates: &Receiver<Update>, id: u32) {
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceSetSettings(
            rbs_api::request::SetSourceSettings {
                source: id,
                settings: [("file".to_owned(), json!("/no/such/file.txt"))].into(),
                gesture: 0,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Ticket(1),
    });
    let window = next(updates, |u| match u {
        Update::ApiAnswer { ticket: 1, result } => Some(result),
        _ => None,
    });
    assert!(
        window
            .as_ref()
            .err()
            .is_none_or(|e| e.kind != rbs_api::ErrorKind::Forbidden),
        "{window:?}"
    );
}
