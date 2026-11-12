//! Video filters and the sync offset through the control API and the host (ADR-0062,
//! protocol 1.8): the catalogue, adding, patching, moving, turning off and removing
//! filters, the refusals, and the sync offset of a source with sound. In-memory
//! transport, reference adapters, an engine on a `TestClock`; waits end when the answer
//! arrives, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::studio_host::{Command, HostConfig, SourceRow, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_filters::{Value as FilterValue, Values};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_scene::model::SourceId;
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

fn filters_of(c: &mut Client, source: u64) -> Vec<Value> {
    // Absent when the chain is empty.
    let row = sources(c).into_iter().find(|s| s["id"] == source).unwrap();
    row["filters"].as_array().cloned().unwrap_or_default()
}

#[test]
fn filters_are_listed_added_patched_moved_and_removed() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let types = c.call("filter.types", json!({}));
    let types = ok(&types)["types"].as_array().unwrap().clone();
    let ids: Vec<_> = types.iter().map(|t| t["type"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "org.rbs.filter.color-correction",
            "org.rbs.filter.chroma-key",
            "org.rbs.filter.blur",
            "org.rbs.filter.sharpen"
        ]
    );
    let key = &types[1]["fields"][0];
    assert_eq!(key["kind"], "color");
    assert_eq!(key["default"], "#00ff00");

    let screen = add(&mut c, "screen", "Screen");
    ok(&c.call("events.subscribe", json!({"topics": ["source.changed"]})));
    let blur = c.call(
        "filter.add",
        json!({"source": screen, "type": "org.rbs.filter.blur", "settings": {"radius": 12}}),
    );
    let blur = ok(&blur)["filter"].as_u64().unwrap();
    let cc = c.call(
        "filter.add",
        json!({"source": screen, "type": "org.rbs.filter.color-correction"}),
    );
    let cc = ok(&cc)["filter"].as_u64().unwrap();
    // Subscribers see the chain change.
    c.event("source.changed", |d| {
        d[0]["filters"].as_array().is_some_and(|f| f.len() == 2)
    });
    let rows = filters_of(&mut c, screen);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], blur);
    assert_eq!(rows[0]["settings"]["radius"], 12.0);
    assert_eq!(rows[1]["settings"]["saturation"], 1.0);

    // A patch changes only what it sends.
    ok(&c.call(
        "filter.set_settings",
        json!({"filter": cc, "settings": {"brightness": 0.25}}),
    ));
    let rows = filters_of(&mut c, screen);
    assert_eq!(rows[1]["settings"]["brightness"], 0.25);
    assert_eq!(rows[1]["settings"]["saturation"], 1.0);
    assert_eq!(rows[1]["settings"]["gamma"], 1.0);

    ok(&c.call("filter.move", json!({"filter": cc, "index": 0})));
    ok(&c.call(
        "filter.set_enabled",
        json!({"filter": blur, "enabled": false}),
    ));
    let rows = filters_of(&mut c, screen);
    assert_eq!(rows[0]["id"], cc);
    assert_eq!(rows[1]["enabled"], false);

    for (method, params, want) in [
        (
            "filter.add",
            json!({"source": screen, "type": "org.rbs.filter.none"}),
            "invalid_params",
        ),
        (
            "filter.add",
            json!({"source": 999, "type": "org.rbs.filter.blur"}),
            "not_found",
        ),
        (
            "filter.set_settings",
            json!({"filter": blur, "settings": {"radius": 500}}),
            "invalid_params",
        ),
        (
            "filter.set_settings",
            json!({"filter": blur, "settings": {"strength": 1}}),
            "invalid_params",
        ),
        (
            "filter.set_settings",
            json!({"filter": cc, "settings": {"brightness": "#ffffff"}}),
            "invalid_params",
        ),
        (
            "filter.move",
            json!({"filter": cc, "index": 5}),
            "invalid_params",
        ),
        ("filter.remove", json!({"filter": 999}), "not_found"),
    ] {
        let bad = c.call(method, params.clone());
        assert_eq!(kind(&bad), want, "{method} {params}: {bad}");
    }
    // A refused patch changed nothing.
    assert_eq!(filters_of(&mut c, screen)[1]["settings"]["radius"], 12.0);

    ok(&c.call("filter.remove", json!({"filter": blur})));
    ok(&c.call("filter.remove", json!({"filter": cc})));
    assert!(filters_of(&mut c, screen).is_empty());
}

#[test]
fn the_sync_offset_is_set_on_sources_with_sound_only() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add(&mut c, "audio_input", "Mic");
    let screen = add(&mut c, "screen", "Screen");
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": mic, "offset_ms": -120}),
    ));
    let row = sources(&mut c)
        .into_iter()
        .find(|s| s["id"] == mic)
        .unwrap();
    assert_eq!(row["sync_offset_ms"], -120);

    for (params, want) in [
        (json!({"source": mic, "offset_ms": 201}), "invalid_params"),
        (json!({"source": screen, "offset_ms": 10}), "invalid_params"),
        (json!({"source": 999, "offset_ms": 10}), "not_found"),
    ] {
        let bad = c.call("source.set_sync_offset", params.clone());
        assert_eq!(kind(&bad), want, "{params}: {bad}");
    }
    // A microphone has no picture to filter.
    let bad = c.call(
        "filter.add",
        json!({"source": mic, "type": "org.rbs.filter.blur"}),
    );
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
}

#[test]
fn audio_filters_are_listed_by_domain_and_edited_on_sound_sources_only() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    // The default domain is video: what a 1.15 client always saw.
    let video = c.call("filter.types", json!({}));
    assert!(
        ok(&video)["types"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["domain"] == "video")
    );
    let audio = c.call("filter.types", json!({"domain": "audio"}));
    let types = ok(&audio)["types"].as_array().unwrap().clone();
    let ids: Vec<_> = types.iter().map(|t| t["type"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "org.rbs.audio-filter.gain",
            "org.rbs.audio-filter.noise-gate",
            "org.rbs.audio-filter.ducker"
        ]
    );
    let attack = types[1]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "attack")
        .unwrap();
    assert_eq!(attack["curve"], "log");
    assert_eq!(attack["unit"], "ms");
    assert!(attack["help"].as_str().is_some_and(|h| !h.is_empty()));
    let all = c.call("filter.types", json!({"domain": "all"}));
    assert_eq!(ok(&all)["types"].as_array().unwrap().len(), 7);

    let mic = add(&mut c, "audio_input", "Mic");
    let screen = add(&mut c, "screen", "Screen");
    let gain = c.call(
        "filter.add",
        json!({"source": mic, "type": "org.rbs.audio-filter.gain", "settings": {"gain": 6}}),
    );
    let gain = ok(&gain)["filter"].as_u64().unwrap();
    let rows = filters_of(&mut c, mic);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["type"], "org.rbs.audio-filter.gain");
    assert_eq!(rows[0]["settings"]["gain"], 6.0);
    ok(&c.call(
        "filter.set_settings",
        json!({"filter": gain, "settings": {"gain": -3.5}}),
    ));
    assert_eq!(filters_of(&mut c, mic)[0]["settings"]["gain"], -3.5);

    // The wrong domain is refused in both directions; ranges are checked.
    for (params, want) in [
        (
            json!({"source": screen, "type": "org.rbs.audio-filter.gain"}),
            "invalid_params",
        ),
        (
            json!({"source": mic, "type": "org.rbs.filter.blur"}),
            "invalid_params",
        ),
        (
            json!({"source": mic, "type": "org.rbs.audio-filter.gain", "settings": {"gain": 31}}),
            "invalid_params",
        ),
    ] {
        let bad = c.call("filter.add", params.clone());
        assert_eq!(kind(&bad), want, "{params}: {bad}");
    }
    ok(&c.call("filter.remove", json!({"filter": gain})));
    assert!(filters_of(&mut c, mic).is_empty());
}

#[test]
fn a_ducker_takes_a_sound_source_as_its_key() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let audio = c.call("filter.types", json!({"domain": "audio"}));
    let types = ok(&audio)["types"].as_array().unwrap().clone();
    let ducker = types
        .iter()
        .find(|t| t["type"] == "org.rbs.audio-filter.ducker")
        .unwrap();
    let key = ducker["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "key_source")
        .unwrap();
    assert_eq!(key["kind"], "source");
    assert!(key["default"].is_null());

    let voice = add(&mut c, "audio_input", "Voice");
    let music = add(&mut c, "audio_input", "Music");
    let screen = add(&mut c, "screen", "Screen");
    let added = c.call(
        "filter.add",
        json!({"source": music, "type": "org.rbs.audio-filter.ducker",
               "settings": {"key_source": voice, "amount": 18}}),
    );
    let duck = ok(&added)["filter"].as_u64().unwrap();
    let rows = filters_of(&mut c, music);
    assert_eq!(rows[0]["settings"]["key_source"], voice);
    assert_eq!(rows[0]["settings"]["amount"], 18.0);

    // Unknown, own and picture sources cannot be the key.
    for (id, want) in [
        (json!(9999), "not_found"),
        (json!(music), "invalid_params"),
        (json!(screen), "invalid_params"),
    ] {
        let bad = c.call(
            "filter.set_settings",
            json!({"filter": duck, "settings": {"key_source": id}}),
        );
        assert_eq!(kind(&bad), want, "{id}: {bad}");
    }
    // null clears the key.
    ok(&c.call(
        "filter.set_settings",
        json!({"filter": duck, "settings": {"key_source": null}}),
    ));
    let rows = filters_of(&mut c, music);
    assert!(
        rows[0]["settings"]
            .get("key_source")
            .is_none_or(|v| v.is_null())
    );
}

#[test]
fn the_properties_tabs_commands_edit_filters_and_the_sync_offset() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen") as u32;
    let mic = add(&mut c, "audio_input", "Mic") as u32;
    let screen = SourceId::from_raw(screen);
    let rows = |pick: &dyn Fn(&[SourceRow]) -> bool| {
        next(&updates, |u| match u {
            Update::Sources(rows) if pick(&rows) => Some(rows),
            _ => None,
        })
    };
    let chain = |rows: &[SourceRow]| {
        rows.iter()
            .find(|r| r.id == screen)
            .map(|r| r.filters.clone())
            .unwrap_or_default()
    };

    host.send(Command::AddFilter {
        source: screen,
        type_id: "org.rbs.filter.sharpen".into(),
    });
    let got = rows(&|r| chain(r).len() == 1);
    let filter = chain(&got)[0].id;
    let ty = rbs_filters::find("org.rbs.filter.sharpen").unwrap();

    // A drag shows values without storing them; the release stores them.
    let dragged = Values::from_pairs(ty, [("amount", FilterValue::Number(1.5))]).unwrap();
    host.send(Command::PreviewFilter {
        filter,
        values: dragged,
    });
    host.send(Command::RestoreFilter(filter));
    host.send(Command::SetFilterValues {
        filter,
        values: dragged,
    });
    let got = rows(&|r| chain(r).first().and_then(|f| f.values) == Some(dragged));
    assert_eq!(chain(&got)[0].name, "Sharpen");

    host.send(Command::SetFilterEnabled {
        filter,
        enabled: false,
    });
    rows(&|r| chain(r).first().is_some_and(|f| !f.enabled));
    host.send(Command::RemoveFilter(filter));
    rows(&|r| chain(r).is_empty());

    host.send(Command::SetSourceSyncOffset {
        id: SourceId::from_raw(mic),
        offset_ms: 80,
    });
    rows(&|r| {
        r.iter()
            .any(|s| s.id.get() == mic && s.audio.sync_offset_ms == 80)
    });
}

#[test]
fn looks_and_sample_pictures_from_the_library_are_one_undo_step_each() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = SourceId::from_raw(add(&mut c, "screen", "Screen") as u32);
    let rows = |pick: &dyn Fn(&[SourceRow]) -> bool| {
        next(&updates, |u| match u {
            Update::Sources(rows) if pick(&rows) => Some(rows),
            _ => None,
        })
    };
    let chain = |rows: &[SourceRow], id: SourceId| {
        rows.iter()
            .find(|r| r.id == id)
            .map(|r| r.filters.clone())
            .unwrap_or_default()
    };

    // A sample background is an image source below everything (rows list the top first).
    host.send(Command::AddSample {
        sample: "aurora".into(),
    });
    let got = rows(&|r| r.len() == 2);
    assert_eq!(got.last().unwrap().name, "Aurora");
    assert_eq!(
        got.last().unwrap().kind,
        Some(rbs_app::studio_host::SourceKind::Image)
    );
    host.send(Command::AddSample {
        sample: "frame-thin".into(),
    });
    let got = rows(&|r| r.len() == 3);
    assert_eq!(
        got.first().unwrap().name,
        "Thin frame",
        "a frame goes on top"
    );
    host.send(Command::AddSample {
        sample: "no-such-sample".into(),
    });
    host.send(Command::ApplyLook {
        source: screen,
        look: "builtin:no-such-look".into(),
    });

    // A colour look adds a colour correction; the same kind of look replaces it.
    host.send(Command::ApplyLook {
        source: screen,
        look: "builtin:warm".into(),
    });
    let got = rows(&|r| chain(r, screen).len() == 1);
    let warm = chain(&got, screen)[0].clone();
    assert_eq!(warm.type_id, "org.rbs.filter.color-correction");
    host.send(Command::ApplyLook {
        source: screen,
        look: "builtin:vivid".into(),
    });
    let got = rows(&|r| {
        chain(r, screen)
            .first()
            .is_some_and(|f| f.stored != warm.stored)
    });
    let vivid = chain(&got, screen);
    assert_eq!(vivid.len(), 1, "replaced, not stacked");
    assert_eq!(vivid[0].id, warm.id);

    // A shape look sets the item's style.
    host.send(Command::ApplyLook {
        source: screen,
        look: "builtin:circle".into(),
    });
    rows(&|r| {
        r.iter().any(|s| {
            s.id == screen
                && s.layout
                    .is_some_and(|l| l.style.shape == rbs_scene::model::Shape::Circle)
        })
    });

    // Undo takes the shape look back, then the colour look's change.
    host.send(Command::Undo);
    rows(&|r| {
        r.iter().any(|s| {
            s.id == screen
                && s.layout
                    .is_some_and(|l| l.style.shape != rbs_scene::model::Shape::Circle)
        })
    });
    host.send(Command::Undo);
    let got = rows(&|r| {
        chain(r, screen)
            .first()
            .is_some_and(|f| f.stored == warm.stored)
    });
    assert_eq!(chain(&got, screen).len(), 1);
}

#[test]
fn looks_samples_and_boxes_go_through_the_control_api() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let looks = ok(&c.call("look.list", json!({})))["looks"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        looks
            .iter()
            .any(|l| { l["id"] == "builtin:warm" && l["name"] == "Warm" && l["group"] == "color" })
    );
    assert!(
        looks
            .iter()
            .any(|l| l["id"] == "builtin:circle" && l["group"] == "shape")
    );

    let screen = add(&mut c, "screen", "Screen");
    let voice = add(&mut c, "audio_input", "Voice");

    assert_eq!(
        kind(&c.call("source.add_sample", json!({"sample": "no-such-sample"}))),
        "not_found"
    );
    let background = ok(&c.call("source.add_sample", json!({"sample": "aurora"})))["source"]
        .as_u64()
        .unwrap();
    let rows = sources(&mut c);
    assert_eq!(
        rows.last().unwrap()["id"],
        background,
        "a background is below"
    );
    assert_eq!(rows.last().unwrap()["kind"], "image");
    assert_eq!(rows.last().unwrap()["name"], "Aurora");
    let frame = ok(&c.call("source.add_sample", json!({"sample": "frame-thin"})))["source"]
        .as_u64()
        .unwrap();
    let rows = sources(&mut c);
    assert_eq!(rows.first().unwrap()["id"], frame, "a frame goes on top");
    assert_eq!(rows.first().unwrap()["name"], "Thin frame");

    assert_eq!(
        kind(&c.call(
            "look.apply",
            json!({"source": screen, "look": "builtin:no-such-look"})
        )),
        "not_found"
    );
    assert_eq!(
        kind(&c.call(
            "look.apply",
            json!({"source": 9_999_999, "look": "builtin:warm"})
        )),
        "not_found"
    );
    assert_eq!(
        kind(&c.call(
            "look.apply",
            json!({"source": voice, "look": "builtin:warm"})
        )),
        "invalid_params",
    );

    ok(&c.call(
        "look.apply",
        json!({"source": screen, "look": "builtin:warm"}),
    ));
    let warm = filters_of(&mut c, screen);
    assert_eq!(warm.len(), 1);
    assert_eq!(warm[0]["type"], "org.rbs.filter.color-correction");
    let warm_settings = warm[0]["settings"].clone();
    ok(&c.call(
        "look.apply",
        json!({"source": screen, "look": "builtin:vivid"}),
    ));
    let vivid = filters_of(&mut c, screen);
    assert_eq!(vivid.len(), 1, "replaced, not stacked");
    assert_eq!(vivid[0]["id"], warm[0]["id"]);
    assert_ne!(vivid[0]["settings"], warm_settings);
    ok(&c.call(
        "look.apply",
        json!({"source": screen, "look": "builtin:circle"}),
    ));
    let shape = |rows: &[Value]| {
        rows.iter().find(|s| s["id"] == screen).unwrap()["layout"]["style"]["shape"].clone()
    };
    assert_eq!(shape(&sources(&mut c)), "circle");
    ok(&c.call("edit.undo", json!({})));
    assert_ne!(shape(&sources(&mut c)), "circle");

    let before = sources(&mut c)
        .into_iter()
        .find(|s| s["id"] == screen)
        .unwrap()["layout"]["position"]
        .clone();
    let placed = c.call(
        "item.set_box",
        json!({
            "source": screen,
            "position": [12.0, 24.0],
            "size": [320.0, 180.0],
            "gesture": 4,
            "seq": 9
        }),
    );
    assert_eq!(ok(&placed)["seq"].as_u64(), Some(9));
    assert_eq!(ok(&placed)["ok"], true);
    ok(&c.call(
        "item.set_box",
        json!({
            "source": screen,
            "position": [40.0, 24.0],
            "size": [320.0, 180.0],
            "gesture": 4,
            "seq": 10
        }),
    ));
    ok(&c.call("edit.undo", json!({})));
    let row = sources(&mut c)
        .into_iter()
        .find(|s| s["id"] == screen)
        .unwrap();
    assert_eq!(
        row["layout"]["position"], before,
        "one gesture is one undo step"
    );
    assert_eq!(
        kind(&c.call(
            "item.set_box",
            json!({"source": screen, "position": [0.0, 0.0], "size": [0.0, 10.0]})
        )),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "item.set_box",
            json!({"source": 9_999_999, "position": [1.0, 1.0], "size": [10.0, 10.0]})
        )),
        "not_found"
    );
}

#[test]
fn a_display_can_capture_the_system_sound_too() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let targets = c.call("source.targets", json!({"kind": "screen"}));
    let t = ok(&targets)["targets"][0].clone();
    let added = c.call(
        "source.add",
        json!({"kind": "screen", "adapter": t["adapter"], "target": t["target"],
               "name": "Screen", "with_audio": true}),
    );
    let screen = ok(&added)["source"].as_u64().unwrap();
    let rows = sources(&mut c);
    let sound = rows
        .iter()
        .find(|s| s["companion_of"] == screen)
        .expect("a companion sound");
    assert_eq!(sound["name"], "System sound");
    let sound_id = sound["id"].as_u64().unwrap();

    // One undo step removes both; redo brings both back.
    assert!(ok(&c.call("edit.undo", json!({}))).is_object());
    assert!(
        sources(&mut c)
            .iter()
            .all(|s| s["id"] != screen && s["id"] != sound_id)
    );
    c.call("edit.redo", json!({}));
    assert!(sources(&mut c).iter().any(|s| s["id"] == sound_id));

    // Removing and adding it again by command.
    c.call(
        "source.set_companion_audio",
        json!({"source": screen, "enabled": false}),
    );
    assert!(sources(&mut c).iter().all(|s| s["companion_of"].is_null()));
    c.call(
        "source.set_companion_audio",
        json!({"source": screen, "enabled": true}),
    );
    assert!(sources(&mut c).iter().any(|s| s["companion_of"] == screen));

    // Deleting the picture deletes the sound; a sound source has no companion.
    let voice = add(&mut c, "audio_input", "Voice");
    let bad = c.call(
        "source.set_companion_audio",
        json!({"source": voice, "enabled": true}),
    );
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
    c.call("source.delete", json!({"source": screen}));
    assert!(sources(&mut c).iter().all(|s| s["companion_of"].is_null()));
}
