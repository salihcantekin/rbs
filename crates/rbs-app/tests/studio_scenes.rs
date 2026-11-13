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
    start_with(None)
}

/// [`start`] with a saved collection, as RBS opens one.
fn start_with(
    collection: Option<rbs_scene::SceneCollection>,
) -> Option<(StudioHost, Receiver<Update>, ApiServer, Client)> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = collection;
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

    // Hidden from the Mixer (ADR-0080): live, keeps the sound, absent means shown.
    assert_eq!(row(&sources(&mut c), mic).get("mixer_hidden"), None);
    ok(&c.call(
        "source.set_mixer_hidden",
        json!({"source": mic, "hidden": true}),
    ));
    let r = row(&sources(&mut c), mic);
    assert_eq!(r["mixer_hidden"], true);
    assert_eq!(
        (r["volume"].clone(), r["muted"].clone()),
        (json!(0.5), json!(true))
    );
    assert_eq!(
        kind(&c.call(
            "source.set_mixer_hidden",
            json!({"source": 999, "hidden": true})
        )),
        "not_found"
    );
    ok(&c.call(
        "source.set_mixer_hidden",
        json!({"source": mic, "hidden": false}),
    ));
    assert_eq!(row(&sources(&mut c), mic).get("mixer_hidden"), None);

    // The canvas master (ADR-0081): live, left out of the list while it is the default.
    assert_eq!(scenes(&mut c).get("master"), None);
    ok(&c.call("canvas.set_master_volume", json!({"volume": 0.25})));
    ok(&c.call("canvas.set_master_muted", json!({"muted": true})));
    assert_eq!(
        scenes(&mut c)["master"],
        json!({"volume": 0.25, "muted": true})
    );
    // The UI hears it too.
    next(&updates, |u| match u {
        Update::Scenes(l) if l.master.muted && (l.master.volume - 0.25).abs() < f32::EPSILON => {
            Some(())
        }
        _ => None,
    });
    for bad in [json!(1.5), json!(-0.1)] {
        let answer = c.call("canvas.set_master_volume", json!({"volume": bad}));
        assert_eq!(kind(&answer), "invalid_params", "{answer}");
    }
    ok(&c.call("canvas.set_master_volume", json!({"volume": 1.0})));
    ok(&c.call("canvas.set_master_muted", json!({"muted": false})));
    assert_eq!(scenes(&mut c).get("master"), None);

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

    // With the swap setting on (ADR-0098), the transition swaps program and preview;
    // it is not on the undo stack.
    host.send(rbs_app::studio_host::Command::SetSwapAfterTransition(true));
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
fn by_default_the_transition_keeps_the_preview_scene() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call("events.subscribe", json!({"topics": ["scene.changed"]})));
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let two = ok(&c.call("scene.add", json!({"name": "Game"})))["scene"]
        .as_u64()
        .unwrap();
    ok(&c.call("canvas.set_preview", json!({"scene": two})));
    // The preview scene goes on air and stays in preview (ADR-0098).
    ok(&c.call("canvas.transition", json!({})));
    let event = c.event("scene.changed", |d| d["program"] == two);
    assert_eq!(event["params"]["data"]["preview"], two, "{event}");
    let after = scenes(&mut c);
    assert_eq!(after["program"], two);
    assert_eq!(after["preview"], two);
    // An edit of the scene on air, then another scene previewed: the program stays
    // held (the copy is never listed), and the next transition puts that scene on air.
    add(&mut c, "audio_input", "Mic");
    ok(&c.call("canvas.set_preview", json!({"scene": one})));
    let list = scenes(&mut c);
    assert_eq!(names(&list), ["Scene", "Game"], "no copy is listed");
    assert_eq!(list["program"], two);
    assert_eq!(list["preview"], one);
    // Undo and redo keep program and preview.
    ok(&c.call("edit.undo", json!({})));
    ok(&c.call("edit.redo", json!({})));
    let list = scenes(&mut c);
    assert_eq!(
        (list["program"].clone(), list["preview"].clone()),
        (json!(two), json!(one))
    );
    ok(&c.call("canvas.transition", json!({})));
    let list = scenes(&mut c);
    assert_eq!(list["program"], one);
    assert_eq!(list["preview"], one);
    // A transition of the scene with itself, swap on: one scene, nothing swaps.
    host.send(rbs_app::studio_host::Command::SetSwapAfterTransition(true));
    ok(&c.call("canvas.transition", json!({})));
    let list = scenes(&mut c);
    assert_eq!(list["program"], one);
    assert_eq!(list["preview"], one);
    while let Ok(u) = updates.try_recv() {
        if let Update::Error(e) = u {
            panic!("{e}");
        }
    }
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
    // The transition button, with the swap setting on (ADR-0098): now the Sources list
    // shows the first scene (in preview) and the Mixer the empty program scene.
    host.send(Command::SetSwapAfterTransition(true));
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

#[test]
fn a_vertical_canvas_is_added_follows_the_main_program_and_is_removed() {
    use rbs_app::studio_host::{Command, VerticalCanvas};
    use rbs_core::media::Size;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let _ = scenes(&mut c);
    host.send(Command::AddVerticalCanvas(VerticalCanvas {
        size: Size::new(1080, 1920),
        follow: true,
        layout: None,
    }));
    let state = next(&updates, |u| match u {
        Update::Vertical(Some(v)) => Some(v),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(state.size, Size::new(1080, 1920));
    assert!(state.follow);
    host.send(Command::RemoveVerticalCanvas);
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    drop(host);
}

#[test]
fn a_box_on_the_vertical_canvas_is_that_canvas_s_own_layout() {
    use rbs_app::studio_host::{Command, OutputCanvas, VerticalCanvas};
    use rbs_core::media::Size;
    use rbs_scene::model::SourceId;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let id = add(&mut c, "screen", "Screen");
    host.send(Command::AddVerticalCanvas(VerticalCanvas {
        size: Size::new(1080, 1920),
        follow: true,
        layout: None,
    }));
    next(&updates, |u| match u {
        Update::Vertical(Some(_)) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    // The rows say where the picture is on the vertical canvas too.
    let rows = next(&updates, |u| match u {
        Update::Sources(r) if r.iter().any(|r| r.vertical_layout.is_some()) => Some(r),
        _ => None,
    });
    let main_before = rows[0].layout.unwrap();
    let box_on = |seq: u64, position: [f32; 2], size: [f32; 2], canvas: OutputCanvas| {
        host.send(Command::SetSourceBox {
            seq,
            gesture: 7,
            id: SourceId::from_raw(id as u32),
            position,
            size,
            canvas,
        });
    };
    box_on(1, [100.0, 200.0], [540.0, 960.0], OutputCanvas::Vertical);
    let rows = next(&updates, |u| match u {
        Update::Sources(r)
            if r[0]
                .vertical_layout
                .is_some_and(|l| l.position == [100.0, 200.0] && l.size == [540.0, 960.0]) =>
        {
            Some(r)
        }
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    // The main canvas keeps its box.
    assert_eq!(rows[0].layout, Some(main_before));
    assert!(next(&updates, |u| match u {
        Update::BoxPlaced { seq: 1, ok } => Some(ok),
        _ => None,
    }));
    // The same box again changes nothing and is still answered; a zero size is refused.
    box_on(2, [100.0, 200.0], [540.0, 960.0], OutputCanvas::Vertical);
    assert!(next(&updates, |u| match u {
        Update::BoxPlaced { seq: 2, ok } => Some(ok),
        _ => None,
    }));
    box_on(3, [0.0, 0.0], [0.0, 960.0], OutputCanvas::Vertical);
    assert!(!next(&updates, |u| match u {
        Update::BoxPlaced { seq: 3, ok } => Some(ok),
        _ => None,
    }));
    // After the main program switches, the following canvas shows the new scene (not
    // the scene it showed when it was added), so that scene's sources are placed on it.
    let first = scenes(&mut c)["program"].clone();
    let two = ok(&c.call("scene.add", json!({"name": "Two"})))["scene"].clone();
    ok(&c.call("scene.switch", json!({"scene": two})));
    let other = add(&mut c, "screen", "Screen 2");
    next(&updates, |u| match u {
        Update::Sources(r)
            if r.iter()
                .any(|r| u64::from(r.id.get()) == other && r.vertical_layout.is_some()) =>
        {
            Some(())
        }
        _ => None,
    });
    host.send(Command::SetSourceBox {
        seq: 6,
        gesture: 8,
        id: SourceId::from_raw(other as u32),
        position: [0.0, 0.0],
        size: [1080.0, 960.0],
        canvas: OutputCanvas::Vertical,
    });
    assert!(next(&updates, |u| match u {
        Update::BoxPlaced { seq: 6, ok } => Some(ok),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    }));
    ok(&c.call("scene.switch", json!({"scene": first})));
    // In studio mode the vertical canvas shows the scene on air, not the one edited:
    // its boxes are refused, and the rows give none.
    let program = scenes(&mut c)["program"].clone();
    ok(&c.call("canvas.set_preview", json!({"scene": program})));
    next(&updates, |u| match u {
        Update::Sources(r) if r.iter().all(|r| r.vertical_layout.is_none()) => Some(()),
        _ => None,
    });
    box_on(5, [0.0, 0.0], [10.0, 10.0], OutputCanvas::Vertical);
    assert!(!next(&updates, |u| match u {
        Update::BoxPlaced { seq: 5, ok } => Some(ok),
        _ => None,
    }));
    ok(&c.call("canvas.set_preview", json!({"scene": null})));
    // Without a vertical canvas a vertical box is refused.
    host.send(Command::RemoveVerticalCanvas);
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        _ => None,
    });
    box_on(4, [0.0, 0.0], [10.0, 10.0], OutputCanvas::Vertical);
    assert!(!next(&updates, |u| match u {
        Update::BoxPlaced { seq: 4, ok } => Some(ok),
        _ => None,
    }));
    drop(host);
}

#[test]
fn a_saved_vertical_canvas_runs_again_when_the_collection_opens() {
    use rbs_app::studio_host::Command;
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_scene::model::SceneScope;
    // What `canvas.add_vertical` leaves in the document: a second canvas that follows
    // the main one and shows a shared scene.
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let (vertical, _) = doc
        .add_canvas("Vertical", Size::new(1080, 1920), fps)
        .unwrap();
    doc.set_program(vertical, Some(scene)).unwrap();
    doc.set_canvas_follow(vertical, Some(main), Default::default())
        .unwrap();
    let Some((host, updates, _api, _c)) = start_with(Some(doc)) else {
        return;
    };
    // It runs without a new Add: its outputs, panel and Remove are offered again.
    let state = next(&updates, |u| match u {
        Update::Vertical(v) => Some(v),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    })
    .expect("the saved vertical canvas runs");
    assert_eq!(state.canvas, vertical);
    assert_eq!(state.size, Size::new(1080, 1920));
    assert!(state.follow);
    host.send(Command::RemoveVerticalCanvas);
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    drop(host);
}

#[test]
fn the_control_api_adds_and_removes_the_vertical_canvas() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    // Nothing to remove yet.
    assert_eq!(
        kind(&c.call("canvas.remove_vertical", json!({}))),
        "not_found"
    );
    // Sizes outside 16..=8192 are refused.
    let bad = c.call("canvas.add_vertical", json!({"width": 32, "height": 1920}));
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
    let odd = c.call("canvas.add_vertical", json!({"width": 721, "height": 1280}));
    assert_eq!(kind(&odd), "invalid_params", "{odd}");
    ok(&c.call(
        "canvas.add_vertical",
        json!({"width": 720, "height": 1280, "layout": "stack"}),
    ));
    next(&updates, |u| match u {
        Update::Vertical(Some(v)) if v.follow => Some(()),
        _ => None,
    });
    // At most one.
    let again = c.call("canvas.add_vertical", json!({}));
    assert_eq!(kind(&again), "conflict", "{again}");
    ok(&c.call("canvas.remove_vertical", json!({})));
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        _ => None,
    });
    drop(host);
}
