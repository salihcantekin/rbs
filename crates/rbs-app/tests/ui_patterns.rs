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
    ("app_menu.slint", 0),
    ("appearance.slint", 0),
    ("components.slint", 0),
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

/// Whether a trimmed line declares an element (`name := Type {`, `Type {`, or one under
/// `if ...:` / `for ... in ...:`).
fn declares_element(line: &str) -> bool {
    let mut rest = line;
    if let Some((_, after)) = rest.split_once(":=") {
        rest = after.trim_start();
    }
    if rest.starts_with("if ") || rest.starts_with("for ") {
        match rest.split_once(": ") {
            Some((_, after)) => rest = after.trim_start(),
            None => return false,
        }
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && rest[name.len()..].trim_start().starts_with('{')
}

/// A focus scope over other controls: a `FocusScope` or `RingFocus` without child
/// elements, declared after a sibling element, must set `focus-on-click: false`. A scope
/// that takes focus on click accepts the first press while it has no focus, so the
/// control under it needs a second click (seen on the Properties tabs after a click in
/// another panel). The control's click handler calls `focus-pointer()` instead.
#[test]
fn focus_scopes_over_controls_do_not_take_the_first_click() {
    let mut wrong = Vec::new();
    for (name, text) in slint_files() {
        let lines: Vec<&str> = text.lines().collect();
        let mut depth = Vec::with_capacity(lines.len());
        let mut d: i64 = 0;
        for l in &lines {
            depth.push(d);
            let code = l.split("//").next().unwrap_or("");
            let mut in_str = false;
            for c in code.chars() {
                match c {
                    '"' => in_str = !in_str,
                    '{' if !in_str => d += 1,
                    '}' if !in_str => d -= 1,
                    _ => {}
                }
            }
        }
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim();
            let scope = t.split_once(":=").map_or(t, |(_, a)| a.trim_start());
            if !(scope.starts_with("RingFocus {") || scope.starts_with("FocusScope {")) {
                continue;
            }
            let end = (i + 1..lines.len())
                .find(|&j| depth[j] <= depth[i])
                .unwrap_or(lines.len());
            let body = &lines[i + 1..end];
            let leaf = !body
                .iter()
                .zip(i + 1..)
                .any(|(b, j)| depth[j] == depth[i] + 1 && declares_element(b.trim()));
            let off = body.iter().any(|b| b.contains("focus-on-click: false"));
            let mut prior = false;
            for j in (0..i).rev() {
                if depth[j] < depth[i] {
                    break;
                }
                if depth[j] == depth[i] && declares_element(lines[j].trim()) {
                    prior = true;
                    break;
                }
            }
            if leaf && prior && !off {
                wrong.push(format!("{name}:{}", i + 1));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "focus scopes over other elements without `focus-on-click: false`: {wrong:?}"
    );
}
