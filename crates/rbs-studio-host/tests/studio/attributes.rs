//! Duplicate source, Duplicate scene with new sources, Reset source and Copy / Paste
//! attributes through the control API (ADR-0133, protocol 1.68): one undo step each,
//! live sound outside it, the clipboard and its event. In-memory transport, reference
//! adapters, an engine on a `TestClock`; waits end when the answer arrives, with a bound
//! only as a hang guard (ADR-0032). Needs a GPU adapter (software is fine); skipped
//! without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_studio_host::studio_host::{HostConfig, StudioHost, Update};
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
        json!({"protocols": ["1.8"], "client": {"name": "test", "version": "1"}}),
    );
    assert_eq!(
        hello["result"]["protocol"],
        rbs_api::session::PROTOCOL_VERSION,
        "{hello}"
    );
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

fn sources(c: &mut Client) -> Vec<Value> {
    ok(&c.call("source.list", json!({})))["data"]
        .as_array()
        .unwrap()
        .clone()
}

fn row(c: &mut Client, source: u64) -> Value {
    sources(c).into_iter().find(|s| s["id"] == source).unwrap()
}

fn names(c: &mut Client) -> Vec<String> {
    sources(c)
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_duplicate_is_independent_placed_above_and_one_undo_step() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    ok(&c.call(
        "filter.add",
        json!({"source": screen, "type": "org.rbs.filter.blur", "settings": {"radius": 12}}),
    ));
    let made = c.call("source.duplicate", json!({"source": screen}));
    let made = ok(&made).clone();
    let copy = made["source"].as_u64().unwrap();
    assert_ne!(copy, screen);
    assert_eq!(made["active"], true);
    assert_eq!(made["companions"], json!([]));
    let r = row(&mut c, copy);
    assert_eq!(r["name"], "Screen 2");
    assert_eq!(r["item"], made["item"]);
    assert_eq!(r["filters"][0]["settings"]["radius"], 12.0);
    assert_ne!(
        r["filters"][0]["id"],
        row(&mut c, screen)["filters"][0]["id"]
    );
    // The copy is directly above the original (the list is top first).
    let order: Vec<u64> = sources(&mut c)
        .iter()
        .map(|s| s["id"].as_u64().unwrap())
        .collect();
    let at = order.iter().position(|s| *s == screen).unwrap();
    assert_eq!(order[at - 1], copy, "{order:?}");
    // Changing the copy leaves the original alone.
    ok(&c.call(
        "filter.set_settings",
        json!({"filter": r["filters"][0]["id"], "settings": {"radius": 3}}),
    ));
    assert_eq!(
        row(&mut c, screen)["filters"][0]["settings"]["radius"],
        12.0
    );
    // Two undos: the filter change, then the duplicate.
    ok(&c.call("edit.undo", json!({})));
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(names(&mut c), vec!["Screen".to_owned()]);
    assert_eq!(
        kind(&c.call("source.duplicate", json!({"source": 999}))),
        "not_found"
    );
}

#[test]
fn a_camera_copy_starts_in_standby() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let cam = add(&mut c, "camera", "Camera");
    let made = c.call("source.duplicate", json!({"source": cam}));
    assert_eq!(ok(&made)["active"], false);
}

#[test]
fn a_scene_with_new_sources_copies_them_and_a_shared_copy_does_not() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    let scenes = c.call("scene.list", json!({}));
    let scene = ok(&scenes)["data"]["scenes"][0]["id"].clone();
    let shared = c.call("scene.duplicate", json!({"scene": scene}));
    assert!(ok(&shared).get("sources").is_none(), "{shared}");
    let copied = c.call(
        "scene.duplicate",
        json!({"scene": scene, "sources": "copied"}),
    );
    let pairs = ok(&copied)["sources"].as_array().unwrap().clone();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0]["from"], screen);
    assert_ne!(pairs[0]["to"], screen);
    assert!(names(&mut c).contains(&"Screen".to_owned()));
    ok(&c.call("edit.undo", json!({})));
    let after = c.call("scene.list", json!({}));
    assert_eq!(ok(&after)["data"]["scenes"].as_array().unwrap().len(), 2);
    assert_eq!(
        kind(&c.call(
            "scene.duplicate",
            json!({"scene": scene, "sources": "deep"})
        )),
        "invalid_params"
    );
}

#[test]
fn paste_writes_chosen_groups_onto_several_sources_in_one_step() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let a = add(&mut c, "screen", "A");
    let b = add(&mut c, "window", "B");
    let d = add(&mut c, "window", "D");
    ok(&c.call(
        "filter.add",
        json!({"source": a, "type": "org.rbs.filter.blur", "settings": {"radius": 7}}),
    ));
    let item_a = row(&mut c, a)["item"].clone();
    ok(&c.call(
        "item.set_style",
        json!({"item": item_a, "style": {"shape": "circle"}}),
    ));
    // Nothing copied yet.
    let (ib, id) = (
        row(&mut c, b)["item"].clone(),
        row(&mut c, d)["item"].clone(),
    );
    assert_eq!(
        kind(&c.call(
            "source.paste_attributes",
            json!({"items": [ib], "groups": ["video_filters"]})
        )),
        "conflict"
    );
    assert_eq!(
        ok(&c.call("source.attributes_clipboard", json!({}))),
        &Value::Null
    );
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["source.attributes_clipboard.changed"]}),
    ));
    let copied = c.call("source.copy_attributes", json!({"source": a}));
    let groups = ok(&copied)["groups"].as_array().unwrap().clone();
    assert!(groups.contains(&json!("video_filters")), "{copied}");
    c.event("source.attributes_clipboard.changed", |d| {
        d["clipboard"]["source"] == a
    });
    assert_eq!(
        kind(&c.call(
            "source.paste_attributes",
            json!({"items": [ib], "groups": ["sparkle"]})
        )),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "source.paste_attributes",
            json!({"items": [ib], "groups": []})
        )),
        "invalid_params"
    );
    let pasted = c.call(
        "source.paste_attributes",
        json!({"items": [ib, id], "groups": ["video_filters", "options", "style"]}),
    );
    let targets = ok(&pasted)["targets"].as_array().unwrap().clone();
    assert_eq!(targets.len(), 2);
    for t in &targets {
        let changed = t["changed"].as_array().unwrap();
        assert!(changed.contains(&json!("video_filters")), "{t}");
        assert!(changed.contains(&json!("style")), "{t}");
        // A window is not a display: its options are not pasted from one.
        assert!(
            t["skipped"]
                .as_array()
                .unwrap()
                .contains(&json!({"group": "options", "reason": "not_applicable"})),
            "{t}"
        );
    }
    assert_eq!(row(&mut c, b)["filters"][0]["settings"]["radius"], 7.0);
    assert_eq!(row(&mut c, d)["filters"][0]["settings"]["radius"], 7.0);
    // Same chain again: nothing changes, so no new step.
    let again = c.call(
        "source.paste_attributes",
        json!({"items": [ib], "groups": ["video_filters"]}),
    );
    assert_eq!(
        ok(&again)["targets"][0]["skipped"],
        json!([{"group": "video_filters", "reason": "unchanged"}])
    );
    // One undo takes both targets back.
    ok(&c.call("edit.undo", json!({})));
    assert!(row(&mut c, b)["filters"].is_null());
    assert!(row(&mut c, d)["filters"].is_null());
}

#[test]
fn sound_is_live_and_never_chosen_by_default() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add(&mut c, "audio_input", "Mic");
    ok(&c.call("source.set_muted", json!({"source": mic, "muted": true})));
    ok(&c.call(
        "filter.add",
        json!({"source": mic, "type": "org.rbs.audio-filter.gain"}),
    ));
    // A reset without groups leaves a muted microphone muted.
    let reset = c.call("source.reset", json!({"source": mic}));
    let t = &ok(&reset)["targets"][0];
    assert!(
        t["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("audio_filters")),
        "{t}"
    );
    assert_eq!(t["live"], json!([]));
    assert_eq!(row(&mut c, mic)["muted"], true);
    // Named, it unmutes, outside the undo step.
    let reset = c.call("source.reset", json!({"source": mic, "groups": ["sound"]}));
    assert_eq!(ok(&reset)["targets"][0]["live"], json!(["sound"]));
    assert_eq!(row(&mut c, mic)["muted"], false);
    // Undo restores the filters, not the mute; a fader move does not block it.
    ok(&c.call("source.set_volume", json!({"source": mic, "volume": 0.5})));
    ok(&c.call("edit.undo", json!({})));
    let r = row(&mut c, mic);
    assert_eq!(r["filters"].as_array().map(Vec::len), Some(1));
    assert_eq!(r["muted"], false);
    assert_eq!(r["volume"], 0.5);
}

/// The rows of one scene (`scene.sources`), top first.
fn scene_rows(c: &mut Client, scene: &Value) -> Vec<Value> {
    ok(&c.call("scene.sources", json!({"scene": scene})))["sources"]
        .as_array()
        .unwrap()
        .clone()
}

fn ids(rows: &[Value]) -> Vec<u64> {
    rows.iter().map(|r| r["id"].as_u64().unwrap()).collect()
}

#[test]
fn removing_sources_from_a_duplicated_scene_never_touches_the_original() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    ok(&c.call(
        "source.set_companion_audio",
        json!({"source": screen, "enabled": true}),
    ));
    add(&mut c, "camera", "Cam");
    add(&mut c, "audio_input", "Mic");
    let scene = ok(&c.call("scene.list", json!({})))["data"]["scenes"][0]["id"].clone();
    let original = scene_rows(&mut c, &scene);
    assert_eq!(original.len(), 4, "{original:?}");

    // Duplicate scene (references): removing every item of the copy deletes nothing.
    let shared = ok(&c.call("scene.duplicate", json!({"scene": scene})))["scene"].clone();
    let rows = scene_rows(&mut c, &shared);
    assert_eq!(ids(&rows), ids(&original));
    for r in &rows {
        let gone = c.call("item.remove", json!({"item": r["item"]}));
        assert_eq!(ok(&gone)["source_deleted"], false, "{r}");
    }
    assert!(scene_rows(&mut c, &shared).is_empty());
    assert_eq!(ids(&scene_rows(&mut c, &scene)), ids(&original));

    // Duplicate scene with new sources: removing the copies deletes them and leaves
    // the original scene's sources placed and running.
    let copied = ok(&c.call(
        "scene.duplicate",
        json!({"scene": scene, "sources": "copied"}),
    ))
    .clone();
    let copy_scene = copied["scene"].clone();
    let rows = scene_rows(&mut c, &copy_scene);
    assert_eq!(rows.len(), original.len());
    for r in &rows {
        assert!(!ids(&original).contains(&r["id"].as_u64().unwrap()), "{r}");
    }
    for r in &rows {
        let gone = c.call("item.remove", json!({"item": r["item"]}));
        // The screen's companion copy went with the screen copy.
        if gone.get("error").is_some() {
            assert_eq!(kind(&gone), "not_found", "{gone}");
            continue;
        }
        assert_eq!(ok(&gone)["source_deleted"], true, "{r}");
    }
    assert!(scene_rows(&mut c, &copy_scene).is_empty());
    assert_eq!(ids(&scene_rows(&mut c, &scene)), ids(&original));
    // Undo brings every copy back in its scene.
    for _ in 0..rows.len() {
        let _ = c.call("edit.undo", json!({}));
    }

    // The same through the UI: the copied scene is edited and each source is removed
    // from it by its row.
    use rbs_scene::model::{SceneId, SourceId};
    use rbs_studio_host::studio_host::Command;
    let raw = |v: &Value| u32::try_from(v.as_u64().unwrap()).unwrap();
    let copied = ok(&c.call(
        "scene.duplicate",
        json!({"scene": scene, "sources": "copied"}),
    ))
    .clone();
    let copy_scene = copied["scene"].clone();
    host.send(Command::SwitchScene(SceneId::from_raw(raw(&copy_scene))));
    next(&updates, |u| match u {
        Update::Scenes(l) if l.program == SceneId::from_raw(raw(&copy_scene)) => Some(()),
        _ => None,
    });
    let rows = scene_rows(&mut c, &copy_scene);
    for r in &rows {
        host.send(Command::RemoveFromScene(SourceId::from_raw(raw(&r["id"]))));
    }
    // Every row answered: a Deleted per source still there when asked, or an error
    // only for a companion that already went with its picture.
    let mut left = rows.len();
    next(&updates, |u| match u {
        Update::Deleted { .. } => {
            left -= 1;
            (left == 0).then_some(())
        }
        Update::Error(e) if e.contains("not in the scene any more") => {
            left -= 1;
            (left == 0).then_some(())
        }
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert!(scene_rows(&mut c, &copy_scene).is_empty());
    assert_eq!(ids(&scene_rows(&mut c, &scene)), ids(&original));

    // Undo brings the copies back; Delete source on one copy leaves its original.
    while scene_rows(&mut c, &copy_scene).len() < rows.len() {
        ok(&c.call("edit.undo", json!({})));
    }
    let back = scene_rows(&mut c, &copy_scene);
    assert_eq!(ids(&back), ids(&rows));
    for r in &back {
        let _ = c.call("source.delete", json!({"source": r["id"]}));
    }
    assert!(scene_rows(&mut c, &copy_scene).is_empty());
    assert_eq!(ids(&scene_rows(&mut c, &scene)), ids(&original));
    drop(host);
}

/// The kind of the next undo step the host announces that removes or deletes `name`.
fn next_removal(updates: &Receiver<Update>, name: &str) -> rbs_studio_host::edit_history::EditKind {
    use rbs_studio_host::edit_history::EditKind;
    next(updates, |u| match u {
        Update::History { undo: Some(s), .. }
            if s.name == name && matches!(s.kind, EditKind::Remove | EditKind::Delete) =>
        {
            Some(s.kind)
        }
        _ => None,
    })
}

#[test]
fn the_companion_sound_leaves_the_scene_with_its_picture() {
    // ADR-0142, protocol 1.88.
    use rbs_studio_host::edit_history::EditKind;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    ok(&c.call(
        "source.set_companion_audio",
        json!({"source": screen, "enabled": true}),
    ));
    let scene = ok(&c.call("scene.list", json!({})))["data"]["scenes"][0]["id"].clone();
    let rows = scene_rows(&mut c, &scene);
    let sound = rows
        .iter()
        .find(|r| r["companion_of"] == screen)
        .unwrap()
        .clone();
    let picture = rows.iter().find(|r| r["id"] == screen).unwrap().clone();
    let copy = ok(&c.call("scene.duplicate", json!({"scene": scene})))["scene"].clone();
    let copy_rows = scene_rows(&mut c, &copy);

    // Removing the picture from the first scene takes its sound there; the copy keeps both.
    let gone = c.call("item.remove", json!({"item": picture["item"]}));
    let gone = ok(&gone);
    assert_eq!(gone["source_deleted"], false);
    assert_eq!(
        gone["removed_items"],
        json!([picture["item"], sound["item"]])
    );
    assert!(gone.get("deleted_sources").is_none(), "{gone}");
    assert!(scene_rows(&mut c, &scene).is_empty());
    assert_eq!(ids(&scene_rows(&mut c, &copy)), ids(&copy_rows));
    assert_eq!(next_removal(&updates, "Screen"), EditKind::Remove);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(ids(&scene_rows(&mut c, &scene)), ids(&rows));

    // With the sound placed only beside the picture, it is deleted with that item and
    // the step is still Remove, named after the picture.
    let copy_sound = copy_rows.iter().find(|r| r["id"] == sound["id"]).unwrap();
    ok(&c.call("item.remove", json!({"item": copy_sound["item"]})));
    let gone = c.call("item.remove", json!({"item": picture["item"]}));
    assert_eq!(ok(&gone)["deleted_sources"], json!([sound["id"]]));
    assert_eq!(next_removal(&updates, "Screen"), EditKind::Remove);
    assert!(sources(&mut c).iter().all(|s| s["id"] != sound["id"]));
    ok(&c.call("edit.undo", json!({})));
    assert!(sources(&mut c).iter().any(|s| s["id"] == sound["id"]));
    drop(host);
}
