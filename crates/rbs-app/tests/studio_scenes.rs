//! Scenes, item lock and visibility, source volume, mute and rename through the
//! control API and the host (ADR-0059): adding, switching, renaming and removing
//! scenes, the program never left empty and never changed by undo or redo, items and
//! sources edited by id. In-memory transport, reference adapters, an engine on a
//! `TestClock`; waits end when the answer arrives, with a bound only as a hang guard
//! (ADR-0032). Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::studio_host::{Command, HostConfig, SourceKind, StudioHost, Update};
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

fn start() -> Option<(StudioHost, Receiver<Update>, ApiServer, Client)> {
    let gpu = gpu()?;
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
        json!({"protocols": ["1.6"], "client": {"name": "test", "version": "1"}}),
    );
    assert_eq!(hello["result"]["protocol"], "1.6", "{hello}");
    Some((host, updates, api, c))
}

fn ok(answer: &Value) -> &Value {
    assert!(answer.get("error").is_none(), "{answer}");
    &answer["result"]
}

fn kind(answer: &Value) -> &Value {
    &answer["error"]["data"]["kind"]
}

/// Adds a source of `kind` with its first target and returns its id.
fn add(c: &mut Client, kind: &str, name: &str) -> u64 {
    let targets = c.call("source.targets", json!({"kind": kind}));
    let t = &ok(&targets)["targets"][0];
    let added = c.call(
        "source.add",
        json!({"kind": kind, "adapter": t["adapter"], "target": t["target"], "name": name}),
    );
    ok(&added)["source"].as_u64().unwrap()
}

fn scenes(c: &mut Client) -> Value {
    ok(&c.call("scene.list", json!({})))["data"].clone()
}

fn names(list: &Value) -> Vec<String> {
    list["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

fn sources(c: &mut Client) -> Vec<Value> {
    ok(&c.call("source.list", json!({})))["data"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn scenes_are_added_switched_renamed_and_removed_and_the_program_never_goes_empty() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call("events.subscribe", json!({"topics": ["scene.changed"]})));
    let first = scenes(&mut c);
    assert_eq!(names(&first), ["Scene"]);
    let one = first["program"].as_u64().unwrap();
    assert_eq!(first["scenes"][0]["id"], one);
    let screen = add(&mut c, "screen", "Main screen");

    // A new scene does not go on air; it is named for the user.
    let two = ok(&c.call("scene.add", json!({})))["scene"]
        .as_u64()
        .unwrap();
    let event = c.event("scene.changed", |d| {
        d["scenes"].as_array().unwrap().len() == 2
    });
    assert_eq!(event["params"]["data"]["program"], one);
    assert_eq!(names(&event["params"]["data"]), ["Scene", "Scene 2"]);
    // The UI gets the list too.
    next(&updates, |u| match u {
        Update::Scenes(l) if l.scenes.len() == 2 => Some(()),
        _ => None,
    });

    // Names: empty is invalid, taken is a conflict.
    assert_eq!(
        kind(&c.call("scene.rename", json!({"scene": two, "name": " "}))),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call("scene.rename", json!({"scene": two, "name": "Scene"}))),
        "conflict"
    );
    assert_eq!(
        kind(&c.call("scene.rename", json!({"scene": 99, "name": "X"}))),
        "not_found"
    );
    ok(&c.call("scene.rename", json!({"scene": two, "name": "Game"})));

    // Switching: the Sources list follows the scene on air.
    ok(&c.call("scene.switch", json!({"scene": two})));
    assert_eq!(scenes(&mut c)["program"], two);
    assert!(sources(&mut c).is_empty());
    let mic = add(&mut c, "audio_input", "Mic");
    assert_eq!(sources(&mut c).len(), 1);
    // The screen of the first scene is placed here too, as one source.
    let item = ok(&c.call("item.add", json!({"source": screen})))["item"]
        .as_u64()
        .unwrap();
    let rows = sources(&mut c);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["item"] == item && r["id"] == screen));
    assert_eq!(
        kind(&c.call("item.add", json!({"source": 99}))),
        "not_found"
    );

    // Removing the scene on air first puts the one before it on air; the undo brings
    // the scene back but not on air (undo never changes the program).
    ok(&c.call("scene.remove", json!({"scene": two})));
    let after = scenes(&mut c);
    assert_eq!(names(&after), ["Scene"]);
    assert_eq!(after["program"], one);
    assert_eq!(
        kind(&c.call("scene.remove", json!({"scene": one}))),
        "conflict"
    );
    ok(&c.call("edit.undo", json!({})));
    let back = scenes(&mut c);
    assert_eq!(names(&back), ["Scene", "Game"]);
    assert_eq!(back["program"], one);

    // Undoing the add of the scene on air puts another scene on air.
    let three = ok(&c.call("scene.add", json!({"name": "Break"})))["scene"]
        .as_u64()
        .unwrap();
    ok(&c.call("scene.switch", json!({"scene": three})));
    ok(&c.call("edit.undo", json!({})));
    let undone = scenes(&mut c);
    assert_eq!(names(&undone), ["Scene", "Game"]);
    assert_eq!(
        undone["program"], one,
        "the first scene the canvas may show"
    );
    // Redo brings the scene back without putting it on air.
    ok(&c.call("edit.redo", json!({})));
    let redone = scenes(&mut c);
    assert_eq!(names(&redone), ["Scene", "Game", "Break"]);
    assert_eq!(redone["program"], one);
    // Every source of the collection is listed for item.add.
    let listed: Vec<u64> = redone["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_u64().unwrap())
        .collect();
    assert!(listed.contains(&screen) && listed.contains(&mic));
    drop(host);
}

#[test]
fn items_lock_and_hide_and_sources_mute_rename_and_change_volume() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Main screen");
    let mic = add(&mut c, "audio_input", "Mic");
    let rows = sources(&mut c);
    let row = |rows: &[Value], id: u64| rows.iter().find(|r| r["id"] == id).unwrap().clone();
    let screen_row = row(&rows, screen);
    assert_eq!(
        screen_row.get("visible"),
        None,
        "shown is the default, left out"
    );
    assert_eq!(screen_row.get("locked"), None);
    assert_eq!(row(&rows, mic)["volume"], 1.0);
    assert_eq!(row(&rows, mic)["muted"], false);
    let item = screen_row["item"].as_u64().unwrap();

    // Lock and hide: one undo step each.
    ok(&c.call("item.set_locked", json!({"item": item, "locked": true})));
    ok(&c.call("item.set_visible", json!({"item": item, "visible": false})));
    let rows = sources(&mut c);
    assert_eq!(row(&rows, screen)["locked"], true);
    assert_eq!(row(&rows, screen)["visible"], false);
    // The UI rows say it too.
    next(&updates, |u| match u {
        Update::Sources(r) if r.iter().any(|r| r.locked && !r.visible) => Some(()),
        _ => None,
    });
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(row(&sources(&mut c), screen).get("visible"), None);
    assert_eq!(row(&sources(&mut c), screen)["locked"], true);
    assert_eq!(
        kind(&c.call("item.set_locked", json!({"item": 999, "locked": true}))),
        "not_found"
    );

    // Volume and mute are live: not on the undo stack.
    ok(&c.call("source.set_volume", json!({"source": mic, "volume": 0.5})));
    ok(&c.call("source.set_muted", json!({"source": mic, "muted": true})));
    let r = row(&sources(&mut c), mic);
    assert_eq!(r["volume"], 0.5);
    assert_eq!(r["muted"], true);
    for bad in [json!(1.5), json!(-0.1)] {
        let answer = c.call("source.set_volume", json!({"source": mic, "volume": bad}));
        assert_eq!(kind(&answer), "invalid_params", "{answer}");
    }
    assert_eq!(
        kind(&c.call("source.set_muted", json!({"source": 999, "muted": true}))),
        "not_found"
    );

    // Rename: one undo step; the undo leaves the sound as it is now.
    ok(&c.call("source.rename", json!({"source": mic, "name": "Voice"})));
    assert_eq!(
        kind(&c.call(
            "source.rename",
            json!({"source": mic, "name": "Main screen"})
        )),
        "conflict"
    );
    assert_eq!(row(&sources(&mut c), mic)["name"], "Voice");
    ok(&c.call("source.set_muted", json!({"source": mic, "muted": false})));
    ok(&c.call("edit.undo", json!({})));
    let r = row(&sources(&mut c), mic);
    assert_eq!(r["name"], "Mic");
    assert_eq!(r["muted"], false);
    assert_eq!(r["volume"], 0.5);
    // The undo before that was the lock, not a volume change.
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(row(&sources(&mut c), screen).get("locked"), None);
    drop(host);
}
