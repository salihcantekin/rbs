//! Add-ons through the control API (ADR-0145, add-ons spec "Control API"): list,
//! install with progress on `addon.changed`, a failed install, remove, and the
//! refusals. A fake download and unpack into a temporary folder, no network; waits end
//! when the answer or event arrives, with a bound only as a hang guard (ADR-0032).
//! Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::io::{Cursor, Read};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_add_ons::{
    ArchiveEntry, EntryKind, Fetch, FetchError, Format, Response, Unpack, UnpackError,
};
use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::add_ons::AddOnsSetup;
use rbs_app::studio_host::{HostConfig, StudioHost, Update};
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use serde_json::{Value, json};

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);
const TARGET: &str = "x86_64-test-none";
/// The fake archive; its SHA-256 is in [`entry`].
const BODY: &[u8] = b"rbs add-on test archive";
const BODY_SHA256: &str = "e10f3014583d3a9068cb636bb6b530996d3f4ad80ef7fc8ad1a708f8177d1977";

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

fn entry(id: &str, file: &str, platform: bool) -> rbs_add_ons::Entry {
    let mut text = format!(
        r#"
id = "{id}"
kind = "runtime"
name = "Test add-on"
summary = "Used by tests."
version = "1.2.3"
hosts = ["dl.example"]
notices = "https://dl.example/notices"
"#
    );
    if platform {
        text.push_str(&format!(
            r#"
[platform.{TARGET}]
url = "https://dl.example/{file}"
size = {size}
sha256 = "{BODY_SHA256}"
unpacked_size = 1000
format = "tar.gz"
strip = 0
library_dir = "lib"
resources_dir = "lib"
required = ["a.bin"]
"#,
            size = BODY.len()
        ));
    }
    rbs_add_ons::parse_entry(&text, TARGET).unwrap().unwrap()
}

/// Serves [`BODY`] for `ok.tar.gz` and fails everything else.
struct FakeFetch;

impl Fetch for FakeFetch {
    fn get(&self, url: &str) -> Result<Response, FetchError> {
        if url.ends_with("/ok.tar.gz") {
            Ok(Response::Body(Box::new(Cursor::new(BODY))))
        } else {
            Err(FetchError::Network("connection refused".into()))
        }
    }
}

/// Reads the archive and hands over `lib/` and `lib/a.bin`.
struct FakeUnpack;

impl Unpack for FakeUnpack {
    fn unpack(
        &self,
        _format: Format,
        input: &mut dyn Read,
        visit: &mut dyn FnMut(ArchiveEntry<'_>) -> ControlFlow<()>,
    ) -> Result<(), UnpackError> {
        let mut all = Vec::new();
        input
            .read_to_end(&mut all)
            .map_err(|e| UnpackError(e.to_string()))?;
        let _ = visit(ArchiveEntry {
            name: b"lib/".to_vec(),
            kind: EntryKind::Dir,
            mode: 0o755,
            link: None,
            data: &mut std::io::empty(),
        });
        let _ = visit(ArchiveEntry {
            name: b"lib/a.bin".to_vec(),
            kind: EntryKind::File,
            mode: 0o644,
            link: None,
            data: &mut Cursor::new(b"content".to_vec()),
        });
        Ok(())
    }
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

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-add-ons-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_control_api_client_installs_and_removes_an_add_on() {
    let Some(gpu) = gpu() else { return };
    let root = temp_root("api");
    let (tx, updates) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.add_ons = Some(AddOnsSetup {
        catalog: vec![
            entry("org.example.good", "ok.tar.gz", true),
            entry("org.example.broken", "missing.tar.gz", true),
            entry("org.example.elsewhere", "", false),
        ],
        root: root.clone(),
        fs: Arc::new(rbs_add_ons_sys::SysFs),
        fetch: Arc::new(FakeFetch),
        unpack: Arc::new(FakeUnpack),
        clock: Arc::new(TestClock::new()),
    });
    let host = StudioHost::spawn(
        config,
        || (RecordAdapters::new(), Vec::new()),
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
    c.call(
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}),
    );

    // The catalog in order, nothing installed, no path in the answer.
    let list = c.call("addon.list", json!({}));
    let rows = list["result"]["add_ons"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{list}");
    assert_eq!(rows[0]["id"], "org.example.good");
    assert_eq!(rows[0]["status"], json!({"state": "not_installed"}));
    assert_eq!(rows[0]["size"], BODY.len());
    assert_eq!(rows[2]["available"], false);
    assert!(!list.to_string().contains(root.to_str().unwrap()));

    let sub = c.call("events.subscribe", json!({"topics": ["addon.changed"]}));
    assert!(sub["result"].is_object(), "{sub}");

    // Install answers at once; the end arrives as an event, and the UI hears it too.
    let r = c.call("addon.install", json!({"id": "org.example.good"}));
    assert!(r["result"].is_object(), "{r}");
    let done = c.event("addon.changed", |d| d["status"]["state"] == "installed");
    assert_eq!(done["params"]["data"]["id"], "org.example.good");
    assert_eq!(done["params"]["object"], 0);
    assert_eq!(done["params"]["data"]["status"]["version"], "1.2.3");
    next(&updates, |u| match u {
        Update::AddOnChanged(c) if c.id == "org.example.good" => {
            matches!(c.status, rbs_api::types::AddOnStatus::Installed { .. }).then_some(())
        }
        _ => None,
    });
    assert!(root.join("org.example.good/1.2.3/lib/a.bin").is_file());
    let list = c.call("addon.list", json!({}));
    assert_eq!(list["result"]["add_ons"][0]["status"]["state"], "installed");

    // A failed download shows its reason on the row until the next try.
    c.call("addon.install", json!({"id": "org.example.broken"}));
    let failed = c.event("addon.changed", |d| d["status"]["state"] == "failed");
    assert_eq!(failed["params"]["data"]["status"]["reason"], "network");
    let list = c.call("addon.list", json!({}));
    assert_eq!(
        list["result"]["add_ons"][1]["status"],
        json!({"state": "failed", "reason": "network"})
    );

    // Refusals: no download for this computer, an unknown id.
    let r = c.call("addon.install", json!({"id": "org.example.elsewhere"}));
    assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{r}");
    assert_eq!(r["error"]["data"]["reason"], "not_available", "{r}");
    let r = c.call("addon.remove", json!({"id": "org.example.nothing"}));
    assert_eq!(r["error"]["data"]["kind"], "not_found", "{r}");
    // Cancel with nothing running changes nothing.
    let r = c.call("addon.cancel", json!({"id": "org.example.good"}));
    assert!(r["result"].is_object(), "{r}");

    // Remove takes every version away.
    let r = c.call("addon.remove", json!({"id": "org.example.good"}));
    assert!(r["result"].is_object(), "{r}");
    c.event("addon.changed", |d| {
        d["id"] == "org.example.good" && d["status"]["state"] == "not_installed"
    });
    assert!(!root.join("org.example.good/1.2.3").exists());
    drop(host);
    let _ = std::fs::remove_dir_all(&root);
}

/// A collection whose program scene shows one browser source.
fn browser_collection() -> rbs_scene::SceneCollection {
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
    use rbs_sources::types::{self, keys};
    let mut col = rbs_scene::SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1280, 720), FrameRate::FPS_30)
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let mut settings = Settings::new();
    settings.insert(
        keys::URL.into(),
        SettingValue::Text("https://example.com/".into()),
    );
    settings.insert(keys::WIDTH.into(), SettingValue::Int(64));
    settings.insert(keys::HEIGHT.into(), SettingValue::Int(64));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(page),
        Layout::new([0.0, 0.0], [64.0, 64.0]),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();
    col
}

/// The browser engine (add-ons spec "Browser engine consumer", conformance tests for
/// `rbs-app`): without it Add Source › Browser leads to Settings › Add-ons and adds
/// nothing; once it is installed Browser adds a page. The studio's side (the source's
/// reason, no slot, the start after an install) is `rbs-record`'s
/// `a_browser_source_waits_for_its_engine_and_starts_after_the_install`.
#[test]
fn add_source_browser_leads_to_the_engine_until_it_is_installed() {
    use rbs_app::browser_tree::Runner;
    use rbs_app::studio_host::{AddOnAction, Command};
    let Some(gpu) = gpu() else { return };
    let root = temp_root("engine");
    let engine = rbs_app::add_ons::BROWSER_ENGINE;
    let (tx, updates) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.collection = Some(browser_collection());
    config.add_ons = Some(AddOnsSetup {
        catalog: vec![entry(engine, "ok.tar.gz", true)],
        root: root.clone(),
        fs: Arc::new(rbs_add_ons_sys::SysFs),
        fetch: Arc::new(FakeFetch),
        unpack: Arc::new(FakeUnpack),
        clock: Arc::new(TestClock::new()),
    });
    // Never started: nothing in this test draws the program.
    config.browser_runner = Some(Runner {
        program: root.join("rbs-browser-cef"),
        args: Vec::new(),
        needs_engine: true,
    });
    let host = StudioHost::spawn(
        config,
        || (RecordAdapters::new(), Vec::new()),
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

    // No engine: Browser leads to the add-on and adds no source.
    host.send(Command::AddBrowser {
        url: "https://example.com/".into(),
    });
    let shown = next(&updates, |u| match u {
        Update::OpenAddOn(id) => Some(id),
        Update::Sources(rows) => {
            assert_eq!(rows.len(), 1, "no source was added");
            None
        }
        _ => None,
    });
    assert_eq!(shown, engine);

    // Installed: Browser adds a page.
    host.send(Command::AddOn {
        id: engine.into(),
        action: AddOnAction::Install,
    });
    next(&updates, |u| match u {
        Update::AddOnChanged(c) if c.id == engine => {
            matches!(c.status, rbs_api::types::AddOnStatus::Installed { .. }).then_some(())
        }
        _ => None,
    });
    host.send(Command::AddBrowser {
        url: "https://example.com/two".into(),
    });
    next(&updates, |u| match u {
        Update::OpenAddOn(_) => panic!("the engine is installed"),
        Update::Sources(rows) => (rows.len() == 2).then_some(()),
        _ => None,
    });
    drop(host);
    let _ = std::fs::remove_dir_all(&root);
}
