//! Browser source checks that do not need CEF (ADR-0109 decision 12).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rbs_browser_ipc::Configure;
use rbs_scene::model::{SettingValue, Settings, SourceId};
use rbs_sources::types::{self, keys};
use rbs_sources::{
    BrowserBudget, BrowserClaim, BrowserScheme, BrowserSession, BrowserStartError, BrowserTree,
    NOT_PERMITTED, ShowClass, SourceError, SourceInstance, SourceTypes, agent_may_set_browser_url,
    browser_scheme, browser_url_is_network, file_directory_scope,
};

struct FakeTree {
    starts: AtomicU32,
    exit_immediately: bool,
}

struct FakeSession {
    alive: bool,
    stopped: Arc<AtomicU32>,
}

impl BrowserSession for FakeSession {
    fn poll(&mut self) -> Option<i32> {
        if self.alive { None } else { Some(2) }
    }
    fn stop(&mut self) {
        self.alive = false;
        self.stopped.fetch_add(1, Ordering::Relaxed);
    }
}

impl BrowserTree for FakeTree {
    fn start(
        &self,
        _config: &Configure,
        _sink: Arc<rbs_sources::BrowserFrameSink>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(FakeSession {
            alive: !self.exit_immediately,
            stopped: Arc::new(AtomicU32::new(0)),
        }))
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
    let mut css = settings("https://example.com/");
    css.insert("css".into(), SettingValue::Text("body{}".into()));
    assert!(rbs_sources::validate_settings(&id, &css).is_err());
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
            url: "https://example.com/".into(),
            width: 1920,
            height: 1080,
            fps: 30,
            transparent: false,
        },
        Arc::clone(budget),
        SourceId::from_raw(id),
        Some(tree),
    )
}
