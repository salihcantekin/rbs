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
