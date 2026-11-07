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
use rbs_app::studio_host::{HostConfig, StudioHost, Update};
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

#[test]
fn studio_mode_previews_a_scene_and_the_transition_puts_it_on_air() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call("events.subscribe", json!({"topics": ["scene.changed"]})));
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let screen = add(&mut c, "screen", "Main screen");
    let two = ok(&c.call("scene.add", json!({"name": "Game"})))["scene"]
        .as_u64()
        .unwrap();
    // Outside studio mode there is no preview scene and nothing to transition.
    assert_eq!(scenes(&mut c).get("preview"), None, "left out when off");
    assert_eq!(kind(&c.call("canvas.transition", json!({}))), "conflict");
    assert_eq!(
        kind(&c.call("canvas.set_preview", json!({"scene": 99}))),
        "not_found"
    );

    // Entering studio mode with a scene previews it; the program stays on air.
    ok(&c.call("canvas.set_preview", json!({"scene": two})));
    let event = c.event("scene.changed", |d| d["preview"] == two);
    assert_eq!(event["params"]["data"]["program"], one);
    // The UI is told too.
    next(&updates, |u| match u {
        Update::Scenes(l) if l.preview.map(|p| p.get()) == Some(two as u32) => Some(()),
        _ => None,
    });

    // Sources go to the scene named in the call; source.list stays on the program.
    let targets = c.call("source.targets", json!({"kind": "audio_input"}));
    let t = ok(&targets)["targets"][0].clone();
    let mic = ok(&c.call(
        "source.add",
        json!({"kind": "audio_input", "adapter": t["adapter"], "target": t["target"],
               "name": "Mic", "scene": two}),
    ))["source"]
        .as_u64()
        .unwrap();
    let program_rows = sources(&mut c);
    assert_eq!(program_rows.len(), 1);
    assert_eq!(program_rows[0]["id"], screen);
    let in_two = ok(&c.call("scene.sources", json!({"scene": two})))["sources"].clone();
    assert_eq!(in_two.as_array().unwrap().len(), 1);
    assert_eq!(in_two[0]["id"], mic);
    ok(&c.call("item.add", json!({"source": screen, "scene": two})));
    let in_two = ok(&c.call("scene.sources", json!({"scene": two})))["sources"].clone();
    assert_eq!(in_two.as_array().unwrap().len(), 2);
    assert_eq!(
        kind(&c.call("scene.sources", json!({"scene": 99}))),
        "not_found"
    );
    assert_eq!(
        kind(&c.call("item.add", json!({"source": screen, "scene": 99}))),
        "not_found"
    );

    // The transition swaps program and preview; it is not on the undo stack.
    ok(&c.call("canvas.transition", json!({})));
    let after = scenes(&mut c);
    assert_eq!(after["program"], two);
    assert_eq!(after["preview"], one);
    assert_eq!(sources(&mut c).len(), 2, "source.list follows the program");
    // Undo removes the item added to Game, but leaves program and preview.
    ok(&c.call("edit.undo", json!({})));
    let undone = scenes(&mut c);
    assert_eq!(undone["program"], two);
    assert_eq!(undone["preview"], one);
    assert_eq!(sources(&mut c).len(), 1);

    // Removing the preview scene keeps studio mode, previewing the program.
    ok(&c.call("scene.remove", json!({"scene": one})));
    let removed = scenes(&mut c);
    assert_eq!(removed["program"], two);
    assert_eq!(removed["preview"], two);
    // A transition between one scene and itself changes nothing.
    ok(&c.call("canvas.transition", json!({})));

    // Leaving studio mode.
    ok(&c.call("canvas.set_preview", json!({"scene": null})));
    assert_eq!(scenes(&mut c).get("preview"), None);
    assert_eq!(kind(&c.call("canvas.transition", json!({}))), "conflict");
    drop(host);
}

#[test]
fn in_studio_mode_the_ui_edits_the_preview_and_the_mixer_hears_the_program() {
    use rbs_app::studio_host::Command;
    use rbs_scene::model::SceneId;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let mic = add(&mut c, "audio_input", "Mic");
    let two = ok(&c.call("scene.add", json!({"name": "Game"})))["scene"]
        .as_u64()
        .unwrap();
    let id = |raw: u64| SceneId::from_raw(u32::try_from(raw).unwrap());

    // The toggle puts the program scene in preview.
    host.send(Command::SetStudioMode(true));
    next(&updates, |u| match u {
        Update::Scenes(l) if l.preview == Some(id(one)) => Some(()),
        _ => None,
    });
    // A click on a scene previews it; the program stays on air.
    host.send(Command::SwitchScene(id(two)));
    next(&updates, |u| match u {
        Update::Scenes(l) if l.preview == Some(id(two)) => {
            assert_eq!(l.program, id(one));
            Some(())
        }
        _ => None,
    });
    // The Sources list shows the edited (preview) scene.
    next(&updates, |u| match u {
        Update::Sources(r) if r.is_empty() => Some(()),
        _ => None,
    });
    // The transition button: now the Sources list shows the first scene (in preview)
    // and the Mixer the empty program scene.
    host.send(Command::Transition);
    next(&updates, |u| match u {
        Update::Scenes(l) if l.program == id(two) => {
            assert_eq!(l.preview, Some(id(one)));
            Some(())
        }
        _ => None,
    });
    let (mut sources_seen, mut heard_seen) = (false, false);
    next(&updates, |u| {
        match u {
            Update::Sources(r) if r.iter().any(|r| u64::from(r.id.get()) == mic) => {
                sources_seen = true;
            }
            Update::Heard(r) if r.is_empty() => heard_seen = true,
            _ => {}
        }
        (sources_seen && heard_seen).then_some(())
    });
    // Off again: a click goes on air.
    host.send(Command::SetStudioMode(false));
    next(&updates, |u| match u {
        Update::Scenes(l) if l.preview.is_none() => Some(()),
        _ => None,
    });
    host.send(Command::SwitchScene(id(one)));
    next(&updates, |u| match u {
        Update::Scenes(l) if l.program == id(one) => Some(()),
        _ => None,
    });
    drop(host);
}

fn globals(c: &mut Client) -> Vec<Value> {
    ok(&c.call("global.list", json!({})))["sources"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn global_sources_are_in_every_scene_and_removal_from_a_scene_is_safe() {
    // ADR-0065, protocol 1.10.
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let cam = add(&mut c, "camera", "Cam");
    let two = ok(&c.call("scene.add", json!({"name": "Two"})))["scene"]
        .as_u64()
        .unwrap();
    ok(&c.call("scene.switch", json!({"scene": two})));
    let cam_two = ok(&c.call("item.add", json!({"source": cam})))["item"]
        .as_u64()
        .unwrap();

    // Remove from scene: the camera stays in the first scene.
    let removed = ok(&c.call("item.remove", json!({"item": cam_two}))).clone();
    assert_eq!(removed["source_deleted"], false);
    assert!(sources(&mut c).is_empty());
    let all = scenes(&mut c);
    assert!(
        all["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == cam)
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(sources(&mut c).len(), 1);

    // A microphone added as a global source: heard in every scene, listed nowhere as a
    // scene, and absent from the 1.9 rows.
    let targets = c.call("source.targets", json!({"kind": "audio_input"}));
    let t = ok(&targets)["targets"][0].clone();
    let mic = ok(&c.call(
        "source.add",
        json!({"kind": "audio_input", "adapter": t["adapter"], "target": t["target"], "name": "Mic", "global": true}),
    ))["source"]
        .as_u64()
        .unwrap();
    let g = globals(&mut c);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0]["id"], mic);
    assert!(sources(&mut c).iter().all(|r| r["id"] != mic));
    let list = scenes(&mut c);
    assert_eq!(
        names(&list),
        ["Scene", "Two"],
        "the global scene is never listed"
    );
    let entry = list["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == mic)
        .unwrap()
        .clone();
    assert_eq!(entry["global"], true);
    assert_eq!(
        kind(&c.call("item.add", json!({"source": mic}))),
        "conflict"
    );
    // The Mixer hears it with the program, in either scene.
    next(&updates, |u| match u {
        Update::Heard(rows) if rows.iter().any(|r| r.id.get() as u64 == mic && r.global) => {
            Some(())
        }
        _ => None,
    });
    // One undo step takes the whole add back.
    ok(&c.call("edit.undo", json!({})));
    assert!(globals(&mut c).is_empty());
    ok(&c.call("edit.redo", json!({})));
    assert_eq!(globals(&mut c).len(), 1);

    // Making the camera global moves it out of the scenes; keep moves it back into
    // the program scene.
    let item = ok(&c.call("global.add", json!({"source": cam})))["item"]
        .as_u64()
        .unwrap();
    assert_eq!(globals(&mut c).len(), 2);
    assert!(sources(&mut c).is_empty());
    ok(&c.call("scene.switch", json!({"scene": one})));
    assert!(sources(&mut c).is_empty(), "it left the first scene too");
    assert_eq!(
        kind(&c.call("global.add", json!({"source": cam}))),
        "conflict"
    );
    let back = ok(&c.call("global.remove", json!({"source": cam, "keep": true})))["item"]
        .as_u64()
        .unwrap();
    assert_ne!(back, item);
    assert_eq!(sources(&mut c).len(), 1);
    assert_eq!(
        kind(&c.call("global.remove", json!({"source": cam}))),
        "invalid_params"
    );

    // Removing the last placement deletes the source.
    let gone = ok(&c.call("global.remove", json!({"source": mic}))).clone();
    assert_eq!(gone["source_deleted"], true);
    assert!(
        scenes(&mut c)["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != mic)
    );
    drop(host);
}

#[test]
fn with_the_program_scene_in_preview_the_program_is_held_until_the_transition() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let screen = add(&mut c, "screen", "Main screen");
    // Studio mode with the program scene in preview: the program is held (ADR-0063).
    ok(&c.call("canvas.set_preview", json!({"scene": one})));
    // An edit of that scene: the scene changes, the held copy is never listed.
    add(&mut c, "audio_input", "Mic");
    let list = scenes(&mut c);
    assert_eq!(names(&list), ["Scene"], "no copy is listed");
    assert_eq!(list["program"], one);
    assert_eq!(list["preview"], one);
    assert_eq!(sources(&mut c).len(), 2);
    // A source the held copy shows is deleted: the copy lets it go and stays on air.
    ok(&c.call("source.delete", json!({"source": screen})));
    assert_eq!(sources(&mut c).len(), 1);
    // The transition puts the edits on air; the studio runs on without an error.
    ok(&c.call("canvas.transition", json!({})));
    ok(&c.call("canvas.transition", json!({})));
    let list = scenes(&mut c);
    assert_eq!(names(&list), ["Scene"]);
    assert_eq!(list["program"], one);
    ok(&c.call("canvas.set_preview", json!({"scene": null})));
    while let Ok(u) = updates.try_recv() {
        if let Update::Error(e) = u {
            panic!("{e}");
        }
    }
    drop(host);
}
