//! Browser source checks that do not need CEF (ADR-0109 decision 12).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rbs_browser_ipc::Configure;
use rbs_scene::model::{SettingValue, Settings, SourceId};
use rbs_sources::types::{self, keys};
use rbs_sources::{
    BrowserBudget, BrowserClaim, BrowserMessage, BrowserScheme, BrowserSession, BrowserStartError,
    BrowserTree, NOT_PERMITTED, ShowClass, SourceError, SourceInstance, SourceTypes,
    agent_may_set_browser_url, browser_scheme, browser_url_is_network, file_directory_scope,
};

struct FakeTree {
    starts: AtomicU32,
    exit_immediately: bool,
    /// The engine is not installed: `unavailable` says so.
    missing: AtomicBool,
    /// What an exit maps to.
    exit_reason: Option<&'static str>,
}

/// Control messages the fake pages got, in order.
static SENT: std::sync::Mutex<Vec<(u32, BrowserMessage)>> = std::sync::Mutex::new(Vec::new());
/// The kept profile directory each start was given, by source width.
static PROFILES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct FakeSession {
    alive: bool,
    stopped: Arc<AtomicU32>,
    exit_reason: Option<&'static str>,
    /// The page's width: tells tests' sessions apart.
    width: u32,
}

impl BrowserSession for FakeSession {
    fn poll(&mut self) -> Option<i32> {
        if self.alive { None } else { Some(2) }
    }
    fn stop(&mut self) {
        self.alive = false;
        self.stopped.fetch_add(1, Ordering::Relaxed);
    }
    fn exit_reason(&self) -> Option<&'static str> {
        self.exit_reason
    }
    fn send(&mut self, message: &BrowserMessage) -> bool {
        SENT.lock().unwrap().push((self.width, message.clone()));
        self.alive
    }
}

impl BrowserTree for FakeTree {
    fn start(
        &self,
        config: &Configure,
        _sink: Arc<rbs_sources::BrowserFrameSink>,
        _sound: Option<Arc<rbs_sources::BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        PROFILES
            .lock()
            .unwrap()
            .push(config.page.profile_dir.clone());
        Ok(Box::new(FakeSession {
            alive: !self.exit_immediately,
            stopped: Arc::new(AtomicU32::new(0)),
            exit_reason: self.exit_reason,
            width: config.width,
        }))
    }

    fn profile_dir(&self, profile_id: &str) -> Option<String> {
        Some(format!("/profiles/{profile_id}"))
    }

    fn unavailable(&self) -> Option<&'static str> {
        self.missing
            .load(Ordering::Relaxed)
            .then_some(rbs_sources::BROWSER_RUNTIME_MISSING)
    }
}

fn settings(url: &str) -> Settings {
    let mut settings = Settings::new();
    settings.insert(keys::URL.to_owned(), SettingValue::Text(url.to_owned()));
    settings
}

#[test]
fn scheme_allow_list() {
    assert_eq!(
        browser_scheme("https://example.com/a").unwrap(),
        BrowserScheme::Https
    );
    assert_eq!(
        browser_scheme("http://example.com/a").unwrap(),
        BrowserScheme::Http
    );
    assert_eq!(
        browser_scheme("file:///C:/page.html").unwrap(),
        BrowserScheme::File
    );
    assert!(browser_scheme("ftp://example.com/a").is_err());
    assert!(browser_scheme("javascript:alert(1)").is_err());
    assert!(browser_scheme("file:page.html").is_err());
    assert!(browser_url_is_network("file:///C:/page.html"));
    assert_eq!(
        file_directory_scope("file:///C:/pages/a.html").as_deref(),
        Some("C:/pages/")
    );
    let reg = SourceTypes::new();
    let id = rbs_graph::node::NodeTypeId::new(types::BROWSER_SOURCE);
    assert!(rbs_sources::validate_settings(&id, &settings("https://example.com/")).is_ok());
    assert!(matches!(
        rbs_sources::validate_settings(&id, &settings("ftp://example.com/")),
        Err(SourceError::InvalidSetting { key: "url", .. })
    ));
    let _ = reg;
}

#[test]
fn even_size_and_8192_cap() {
    let id = rbs_graph::node::NodeTypeId::new(types::BROWSER_SOURCE);
    let mut ok = settings("https://example.com/");
    ok.insert(keys::WIDTH.to_owned(), SettingValue::Int(8192));
    ok.insert(keys::HEIGHT.to_owned(), SettingValue::Int(1080));
    ok.insert(keys::FPS.to_owned(), SettingValue::Int(60));
    assert!(rbs_sources::validate_settings(&id, &ok).is_ok());
    let mut odd = settings("https://example.com/");
    odd.insert(keys::WIDTH.to_owned(), SettingValue::Int(1919));
    odd.insert(keys::HEIGHT.to_owned(), SettingValue::Int(1080));
    assert!(rbs_sources::validate_settings(&id, &odd).is_err());
    let mut huge = settings("https://example.com/");
    huge.insert(keys::WIDTH.to_owned(), SettingValue::Int(8194));
    huge.insert(keys::HEIGHT.to_owned(), SettingValue::Int(1080));
    assert!(rbs_sources::validate_settings(&id, &huge).is_err());
    let mut fast = settings("https://example.com/");
    fast.insert(keys::FPS.to_owned(), SettingValue::Int(61));
    assert!(rbs_sources::validate_settings(&id, &fast).is_err());
    let mut unknown = settings("https://example.com/");
    unknown.insert("user_agent".into(), SettingValue::Text("x".into()));
    assert!(rbs_sources::validate_settings(&id, &unknown).is_err());
}

fn with(key: &str, value: SettingValue) -> Settings {
    let mut s = settings("https://example.com/");
    s.insert(key.to_owned(), value);
    s
}

fn valid(key: &str, value: SettingValue) -> bool {
    let id = rbs_graph::node::NodeTypeId::new(types::BROWSER_SOURCE);
    rbs_sources::validate_settings(&id, &with(key, value)).is_ok()
}

#[test]
fn page_settings_have_their_bounds() {
    use SettingValue::{Bool, Int, Text};
    // ADR-0132 decision 4: CSS up to 64 KiB.
    assert!(valid(keys::CSS, Text("body{}".into())));
    assert!(valid(keys::CSS, Text("a".repeat(64 * 1024))));
    assert!(!valid(keys::CSS, Text("a".repeat(64 * 1024 + 1))));
    // ADR-0149 decision 1.
    assert!(valid(keys::RELOAD_EVERY_S, Int(0)));
    assert!(!valid(keys::RELOAD_EVERY_S, Int(9)));
    assert!(valid(keys::RELOAD_EVERY_S, Int(10)));
    assert!(valid(keys::RELOAD_EVERY_S, Int(86_400)));
    assert!(!valid(keys::RELOAD_EVERY_S, Int(86_401)));
    assert!(valid(keys::SCRIPT, Text("a".repeat(32 * 1024))));
    assert!(!valid(keys::SCRIPT, Text("a".repeat(32 * 1024 + 1))));
    assert!(valid(keys::SCRIPT_EVERY_S, Int(0)));
    assert!(valid(keys::SCRIPT_EVERY_S, Int(1)));
    assert!(!valid(keys::SCRIPT_EVERY_S, Int(86_401)));
    assert!(!valid(keys::SCRIPT_EVERY_S, Int(-1)));
    assert!(!valid(keys::ZOOM_PERCENT, Int(24)));
    assert!(valid(keys::ZOOM_PERCENT, Int(25)));
    assert!(valid(keys::ZOOM_PERCENT, Int(500)));
    assert!(!valid(keys::ZOOM_PERCENT, Int(501)));
    for flag in [
        keys::RELOAD_ON_FILE_CHANGE,
        keys::KEEP_SIGNED_IN,
        keys::SCRIPT_ON_LOAD,
        keys::KEEP_RUNNING,
        keys::RETRY_ON_ERROR,
    ] {
        assert!(valid(flag, Bool(true)), "{flag}");
        assert!(!valid(flag, Int(1)), "{flag}");
    }
    assert!(valid(
        keys::PROFILE_ID,
        Text("0123456789abcdef0123456789abcdef".into())
    ));
    assert!(!valid(
        keys::PROFILE_ID,
        Text("0123456789ABCDEF0123456789ABCDEF".into())
    ));
    assert!(!valid(keys::PROFILE_ID, Text("short".into())));
}

#[test]
fn page_settings_take_their_defaults() {
    let page = rbs_sources::browser_settings(&settings("https://example.com/")).unwrap();
    assert_eq!(page.zoom_percent, 100);
    assert!(page.retry_on_error);
    assert!(!page.keep_running && !page.script_on_load && !page.reload_on_file_change);
    assert_eq!((page.reload_every_s, page.script_every_s), (0, 0));
    assert!(page.css.is_empty() && page.script.is_empty() && page.profile_id.is_none());
    // A profile id counts only while keep_signed_in is on.
    let mut kept = with(
        keys::PROFILE_ID,
        SettingValue::Text("0123456789abcdef0123456789abcdef".into()),
    );
    assert!(
        rbs_sources::browser_settings(&kept)
            .unwrap()
            .profile_id
            .is_none()
    );
    kept.insert(keys::KEEP_SIGNED_IN.to_owned(), SettingValue::Bool(true));
    assert!(
        rbs_sources::browser_settings(&kept)
            .unwrap()
            .profile_id
            .is_some()
    );
}

#[test]
fn cap_prefers_program_over_preview() {
    let budget = BrowserBudget::shared();
    let claims = [
        BrowserClaim {
            source: SourceId::from_raw(5),
            class: ShowClass::Program,
        },
        BrowserClaim {
            source: SourceId::from_raw(1),
            class: ShowClass::Preview,
        },
        BrowserClaim {
            source: SourceId::from_raw(2),
            class: ShowClass::Preview,
        },
    ];
    let grants = budget.grant(&claims);
    let slot = |id: u32| {
        grants
            .iter()
            .find(|g| g.source == SourceId::from_raw(id))
            .unwrap()
            .slot
    };
    assert!(slot(5), "program takes a slot before preview");
    assert!(slot(1), "lower preview id wins the remaining slot");
    assert!(!slot(2));
    assert_eq!(
        grants
            .iter()
            .find(|g| g.source == SourceId::from_raw(2))
            .unwrap()
            .reason,
        Some(rbs_sources::BROWSER_CAP)
    );
    budget.release(SourceId::from_raw(5));
    assert!(
        !budget.holds(SourceId::from_raw(5)),
        "a dead helper does not hold a slot"
    );
    let again = budget.grant(&claims);
    assert!(
        again
            .iter()
            .all(|g| g.slot || g.reason == Some(rbs_sources::BROWSER_CAP))
    );
    assert_eq!(budget.running(), 2);
}

#[test]
fn no_helper_while_standby() {
    let budget = BrowserBudget::shared();
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: false,
        missing: AtomicBool::new(false),
        exit_reason: None,
    });
    let reg = SourceTypes::new();
    let ctx = rbs_sources::InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    )
    .with_browser(Arc::clone(&budget), Some(tree.clone()));
    let mut collection = rbs_scene::SceneCollection::new();
    let (id, _) = collection
        .add_source(
            "Page",
            rbs_graph::node::NodeTypeId::new(types::BROWSER_SOURCE),
            settings("https://example.com/"),
        )
        .unwrap();
    budget.grant(&[BrowserClaim {
        source: id,
        class: ShowClass::Program,
    }]);
    let instance = reg
        .instantiate(collection.source(id).unwrap(), &ctx)
        .unwrap();
    let SourceInstance::Browser(mut browser) = instance else {
        panic!("browser");
    };
    assert!(!browser.node.helper_running());
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0);
    browser.node.activate().unwrap();
    assert!(browser.node.helper_running());
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    browser.node.standby();
    assert!(!browser.node.helper_running());
    assert_eq!(browser.node.starts(), 1);
}

#[test]
fn a_third_active_browser_is_refused_without_cef() {
    let budget = BrowserBudget::shared();
    let claims = [
        BrowserClaim {
            source: SourceId::from_raw(1),
            class: ShowClass::Program,
        },
        BrowserClaim {
            source: SourceId::from_raw(2),
            class: ShowClass::Program,
        },
        BrowserClaim {
            source: SourceId::from_raw(3),
            class: ShowClass::Program,
        },
    ];
    budget.grant(&claims);
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: false,
        missing: AtomicBool::new(false),
        exit_reason: None,
    });
    let mut third = browser_node(3, &budget, tree.clone());
    let error = third.activate().unwrap_err();
    assert_eq!(error, BrowserStartError::Cap);
    assert_eq!(third.reason(), Some(rbs_sources::BROWSER_CAP));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0);
    assert!(!third.helper_running());
}

#[test]
fn helper_exit_leaves_the_host_running() {
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(4);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: true,
        missing: AtomicBool::new(false),
        exit_reason: None,
    });
    let mut node = browser_node(4, &budget, tree);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| node.activate()));
    let error = result.expect("host panicked").unwrap_err();
    assert_eq!(error, BrowserStartError::Exited);
    assert!(node.attempts() >= 1);
    assert!(!node.helper_running());
    assert!(!budget.holds(source));
    assert!(node.retry_after().is_some());
}

#[test]
fn no_engine_makes_the_source_runtime_missing_with_no_slot_until_it_is_installed() {
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(5);
    let claim = [BrowserClaim {
        source,
        class: ShowClass::Program,
    }];
    budget.grant(&claim);
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: false,
        missing: AtomicBool::new(true),
        exit_reason: None,
    });
    let mut node = browser_node(5, &budget, tree.clone());
    let error = node.activate().unwrap_err();
    assert_eq!(
        error,
        BrowserStartError::Unavailable(rbs_sources::BROWSER_RUNTIME_MISSING)
    );
    assert_eq!(node.reason(), Some(rbs_sources::BROWSER_RUNTIME_MISSING));
    assert_eq!(
        node.control().engine_reason(),
        Some(rbs_sources::BROWSER_RUNTIME_MISSING)
    );
    assert!(!budget.holds(source), "no slot");
    assert_eq!(node.attempts(), 0, "no back-off");
    assert!(node.retry_after().is_none());
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0, "nothing started");

    // Installed: the studio grants again and the next start runs.
    tree.missing.store(false, Ordering::Relaxed);
    budget.grant(&claim);
    node.activate().unwrap();
    assert!(node.helper_running());
    assert_eq!(node.reason(), None);
    assert_eq!(node.control().engine_reason(), None);
}

#[test]
fn a_runner_exit_maps_to_its_engine_reason() {
    for (exit, reason) in [
        (
            Some(rbs_sources::BROWSER_RUNTIME_MISSING),
            rbs_sources::BROWSER_RUNTIME_MISSING,
        ),
        (
            Some(rbs_sources::BROWSER_RUNTIME_MISMATCH),
            rbs_sources::BROWSER_RUNTIME_MISMATCH,
        ),
        (None, rbs_sources::HELPER_EXIT),
    ] {
        let budget = BrowserBudget::shared();
        let source = SourceId::from_raw(6);
        budget.grant(&[BrowserClaim {
            source,
            class: ShowClass::Program,
        }]);
        let tree = Arc::new(FakeTree {
            starts: AtomicU32::new(0),
            exit_immediately: true,
            missing: AtomicBool::new(false),
            exit_reason: exit,
        });
        let mut node = browser_node(6, &budget, tree);
        assert_eq!(node.activate(), Err(BrowserStartError::Exited));
        assert_eq!(node.reason(), Some(reason));
        assert_eq!(node.control().engine_reason(), exit);
        assert!(!budget.holds(source));
    }
}

#[test]
fn control_messages_reach_only_a_running_page_with_its_profile() {
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(7);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: false,
        missing: AtomicBool::new(false),
        exit_reason: None,
    });
    let profile = "0123456789abcdef0123456789abcdef";
    let mut node = rbs_sources::BrowserSourceNode::new(
        rbs_sources::BrowserSettings {
            width: 1234,
            height: 1080,
            profile_id: Some(profile.into()),
            ..rbs_sources::BrowserSettings::page("https://example.com/")
        },
        Arc::clone(&budget),
        source,
        Some(tree),
    );
    let reload = BrowserMessage::Reload { bypass_cache: true };
    assert!(!node.control().send(&reload), "no page runs yet");
    node.activate().unwrap();
    assert!(
        PROFILES
            .lock()
            .unwrap()
            .contains(&format!("/profiles/{profile}"))
    );
    assert!(node.control().send(&reload));
    assert!(node.control().send(&BrowserMessage::RunScript(None)));
    node.standby();
    assert!(!node.control().send(&reload), "a stopped page takes none");
    let sent: Vec<BrowserMessage> = SENT
        .lock()
        .unwrap()
        .iter()
        .filter(|(width, _)| *width == 1234)
        .map(|(_, m)| m.clone())
        .collect();
    assert_eq!(sent, [reload, BrowserMessage::RunScript(None)]);
}

#[test]
fn agent_url_without_network_permission_is_not_permitted() {
    for url in [
        "https://example.com/a",
        "http://example.com/a",
        "file:///C:/page.html",
    ] {
        let denied = agent_may_set_browser_url(url, false).unwrap_err();
        assert_eq!(denied.kind, NOT_PERMITTED);
        assert_eq!(denied.reason, "network");
    }
    assert!(agent_may_set_browser_url("file:///C:/page.html", true).is_ok());
}

fn browser_node(
    id: u32,
    budget: &Arc<BrowserBudget>,
    tree: Arc<FakeTree>,
) -> rbs_sources::BrowserSourceNode {
    rbs_sources::BrowserSourceNode::new(
        rbs_sources::BrowserSettings {
            width: 1920,
            height: 1080,
            ..rbs_sources::BrowserSettings::page("https://example.com/")
        },
        Arc::clone(budget),
        SourceId::from_raw(id),
        Some(tree),
    )
}

#[test]
fn a_background_page_never_takes_a_slot_from_a_shown_page() {
    // ADR-0149 decision 5: background after program and preview, lower id first.
    let budget = BrowserBudget::shared();
    let claim = |id: u32, class| BrowserClaim {
        source: SourceId::from_raw(id),
        class,
    };
    let grants = budget.grant(&[
        claim(1, ShowClass::Background),
        claim(7, ShowClass::Preview),
        claim(2, ShowClass::Background),
    ]);
    let slot = |id: u32| {
        grants
            .iter()
            .find(|g| g.source == SourceId::from_raw(id))
            .unwrap()
            .slot
    };
    assert!(slot(7) && slot(1) && !slot(2));
    // A second shown page takes the background page's slot.
    budget.grant(&[
        claim(1, ShowClass::Background),
        claim(7, ShowClass::Preview),
        claim(9, ShowClass::Program),
    ]);
    assert!(!budget.holds(SourceId::from_raw(1)));
    assert!(budget.holds(SourceId::from_raw(9)) && budget.holds(SourceId::from_raw(7)));
}

#[test]
fn the_studio_starts_a_background_page_hidden_and_a_release_keeps_it() {
    // ADR-0149 decision 5, ADR-0152 decision 4: the picture node is not scheduled, so
    // the control handle starts the page; a release of the node keeps a background
    // page's helper, and stopping it frees the slot.
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(11);
    let tree = Arc::new(FakeTree {
        starts: AtomicU32::new(0),
        exit_immediately: false,
        missing: AtomicBool::new(false),
        exit_reason: None,
    });
    // A width no other test uses tells this test's messages apart.
    let width = 1288;
    let mut node = rbs_sources::BrowserSourceNode::new(
        rbs_sources::BrowserSettings {
            width,
            height: 720,
            ..rbs_sources::BrowserSettings::page("https://example.com/")
        },
        Arc::clone(&budget),
        source,
        Some(tree.clone()),
    );
    let control = node.control();
    let sent = || -> Vec<BrowserMessage> {
        SENT.lock()
            .unwrap()
            .iter()
            .filter(|(w, _)| *w == width)
            .map(|(_, m)| m.clone())
            .collect()
    };
    control.set_shown(false);
    control.set_background(true);
    // No slot yet: refused as capped, nothing started.
    let refused = control.start().unwrap_err();
    assert_eq!(refused.error, BrowserStartError::Cap);
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Background,
    }]);
    assert_eq!(control.start(), Ok(true));
    assert_eq!(control.start(), Ok(false), "one helper only");
    assert!(control.helper_running());
    assert_eq!(sent(), vec![BrowserMessage::Shown(false)], "starts hidden");
    // The picture node's release keeps the background page.
    node.standby();
    assert!(control.helper_running());
    // Shown: the page is told once; a prepare finds it running.
    control.set_shown(true);
    control.set_shown(true);
    assert_eq!(
        sent(),
        vec![BrowserMessage::Shown(false), BrowserMessage::Shown(true)]
    );
    node.activate().unwrap();
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    // A shown page took the slot: stopped, reported, slot free.
    control.stop_background(true);
    assert!(!control.helper_running() && control.capped());
    assert!(!budget.holds(source));
    // Not a background page any more: a release stops the helper as before.
    control.set_background(false);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    node.activate().unwrap();
    assert!(!control.capped());
    node.standby();
    assert!(!control.helper_running());
}
