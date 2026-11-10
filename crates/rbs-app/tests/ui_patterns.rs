//! Guards for the interaction patterns of ADR-0094. Each check has an allowlist of the
//! places that still use the old way; the list may only shrink as screens migrate, so a
//! new screen cannot add another.

#![allow(clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

fn slint_files() -> BTreeMap<String, String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
    std::fs::read_dir(dir)
        .expect("the ui folder")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.ends_with(".slint").then_some(name)
        })
        .map(|n| {
            let text = std::fs::read_to_string(format!("{dir}/{n}")).expect("a .slint file");
            (n, text)
        })
        .collect()
}

/// Rule B1: one menu system. A `PopupWindow` takes no keyboard focus and draws outside
/// the window layers, so menus move to `CommandMenu`. These are the ones not yet moved
/// (file, count); lower a number when a menu migrates, never raise one.
const POPUP_WINDOWS: &[(&str, usize)] = &[
    ("app_menu.slint", 1),
    ("appearance.slint", 0),
    ("components.slint", 2),
    ("studio.slint", 0),
];

#[test]
fn no_new_popup_windows() {
    let mut found = BTreeMap::new();
    for (name, text) in slint_files() {
        let n = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("PopupWindow {") || l.contains("inherits PopupWindow"))
            .count();
        if n > 0 {
            found.insert(name, n);
        }
    }
    let allowed: BTreeMap<String, usize> = POPUP_WINDOWS
        .iter()
        .map(|(n, c)| ((*n).to_string(), *c))
        .collect();
    for (name, n) in &found {
        let max = allowed.get(name).copied().unwrap_or(0);
        assert!(
            *n <= max,
            "{name} has {n} PopupWindow(s), {max} allowed: use CommandMenu (ADR-0094 B1)"
        );
    }
    for (name, max) in &allowed {
        let n = found.get(name).copied().unwrap_or(0);
        assert!(
            n >= *max,
            "{name} has {n} PopupWindow(s): lower its number in POPUP_WINDOWS to {n}"
        );
    }
}
