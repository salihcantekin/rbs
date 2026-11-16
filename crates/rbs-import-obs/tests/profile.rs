//! Profile import (ADR-0131 decisions 4, 5 and 10): output facts per mode, the stream
//! key kept out of every printable form, and hotkeys as shortcuts. Fixtures are
//! hand-written for this repository. Set `UPDATE_GOLDEN=1` to rewrite a golden file,
//! then review the diff.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rbs_graph::node::NodeTypeId;
use rbs_import_obs::discover::read_profile;
use rbs_import_obs::{
    Catalogue, ImportPlan, Lookup, LookupTarget, Mappers, Options, Outcome, plan,
};
use rbs_params::Field;
use rbs_ui::shortcut::Platform;

const SHOW: &str = include_str!("fixtures/show.json");

struct Every;

impl Catalogue for Every {
    fn has(&self, _: &NodeTypeId) -> bool {
        true
    }
    fn fields(&self, _: &NodeTypeId) -> Option<&'static [Field]> {
        None
    }
}

/// Knows nothing about files or devices; runs on Windows.
struct OnWindows;

impl Lookup for OnWindows {
    fn picture_size(&self, _: &Path) -> Option<(u32, u32)> {
        None
    }
    fn file_exists(&self, _: &Path) -> bool {
        false
    }
    fn targets(&self, _: &NodeTypeId) -> Vec<LookupTarget> {
        Vec::new()
    }
    fn platform(&self) -> Platform {
        Platform::Windows
    }
}

fn fixture(dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
}

fn run(dir: &str, options: &Options) -> ImportPlan {
    let files = read_profile(&fixture(dir)).unwrap();
    plan(
        SHOW.as_bytes(),
        Some(&files),
        &Mappers::builtin(),
        &Every,
        &OnWindows,
        options,
    )
    .unwrap()
}

fn summary(p: &ImportPlan) -> String {
    let mut s = String::new();
    let f = p.profile.as_ref().unwrap();
    writeln!(s, "profile {:?}", f.name).unwrap();
    writeln!(s, "{:#?}", f.output).unwrap();
    writeln!(s, "stream key {}", p.secrets.stream_key.is_some()).unwrap();
    for k in &p.shortcuts {
        writeln!(
            s,
            "shortcut {} {} from {}",
            k.command_for("C"),
            k.chord,
            k.obs_action
        )
        .unwrap();
    }
    writeln!(s, "profile and hotkey report").unwrap();
    for e in &p.report.entries {
        if matches!(e.area.as_str(), "profile" | "setting" | "hotkey") {
            writeln!(s, "- {e}").unwrap();
        }
    }
    s
}

fn golden(name: &str, actual: &str) {
    let path = fixture(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        expected, actual,
        "golden {name} differs; run with UPDATE_GOLDEN=1 and review the diff"
    );
}

#[test]
fn an_advanced_profile_imports_as_the_golden() {
    golden(
        "advanced.plan.txt",
        &summary(&run("advanced", &Options::default())),
    );
}

#[test]
fn a_simple_profile_imports_as_the_golden() {
    golden(
        "simple.plan.txt",
        &summary(&run("simple", &Options::default())),
    );
}

#[test]
fn credentials_appear_only_in_the_secrets() {
    let p = run("advanced", &Options::default());
    assert_eq!(
        p.secrets.stream_key.as_ref().map(|k| k.expose()),
        Some("live_SECRET-KEY-42")
    );
    let printed = format!("{p:?}\n{}", p.report);
    for secret in ["SECRET-KEY", "SECRET-PASSWORD", "\"me\""] {
        assert!(!printed.contains(secret), "{secret} in {printed}");
    }
}

#[test]
fn parts_left_out_are_not_in_the_plan_and_are_reported() {
    let options = Options {
        recording: false,
        stream_server: false,
        replay: false,
        audio_settings: false,
        hotkeys: false,
        ..Options::default()
    };
    let p = run("advanced", &options);
    let out = &p.profile.as_ref().unwrap().output;
    assert!(out.recording.is_none() && out.stream.is_none());
    assert!(out.replay.is_none() && out.audio.is_none());
    assert!(p.secrets.stream_key.is_none());
    assert!(p.shortcuts.is_empty());
    assert!(p.report.with_outcome(Outcome::NotChosen).count() >= 6);

    let key_only_off = Options {
        stream_key: false,
        ..Options::default()
    };
    let p = run("advanced", &key_only_off);
    assert!(p.secrets.stream_key.is_none());
    assert!(p.profile.unwrap().output.stream.unwrap().server.is_some());
}
