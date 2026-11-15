//! The settings search index (`rbs_ui::settings_index`, spec
//! `docs/specs/settings-search.md`) against the Settings pages in `ui/`: Slint has no
//! document tree to search, so the index is a table, and this test keeps it in sync.
//!
//! - Every row (`SettingRow`, `SettingBlock`) with a fixed label names its index key
//!   (`find: "<key>";`) or says why search leaves it out (`// find: none (<why>).`).
//! - Every key a row names is in the index, and every row key of the index is named.
//! - A fixed label equals the index title.
//! - The row's level is the `advanced` or `expert` branch it is drawn in.
//! - The row's card is one of the index places of that key.
//! - Every card (`SettingsCard` with an `id: "settings.…"`) is an index card with the
//!   same title, and every index card is in the pages.

#![allow(clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use std::collections::{BTreeMap, BTreeSet};

use rbs_ui::settings_index::{Level, SETTINGS, find};

/// Files without Settings pages: the component library and the Settings layout parts,
/// the test probes, and the Source settings window.
const NOT_PAGES: &[&str] = &[
    "components.slint",
    "settings.slint",
    "overlay_probe.slint",
    "source_settings.slint",
];

/// A line's code: without its `//` comment and with string contents blanked, so
/// braces and slashes in texts ("rtmp://", "{}") do not count.
fn code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
                out.push('"');
                continue;
            }
            out.push(' ');
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == '/' && chars.peek() == Some(&'/') {
            break;
        }
        out.push(c);
    }
    out
}

/// One open block: the code before its `{`, and the card id when it is a card.
struct Open {
    header: String,
    card: Option<String>,
    is_card: bool,
    row: Option<usize>,
}

/// The level a block header sets: only the conditions the pages use for levels
/// (ADR-0113). Any other mention of a level, a negated one included, stops the test so
/// the scan is taught the new form instead of guessing.
fn header_level(at: &str, header: &str) -> Option<Level> {
    const EXPERT: &[&str] = &["if root.expert:", "if Experience.expert:"];
    const ADVANCED: &[&str] = &[
        "if root.advanced:",
        "if Experience.advanced:",
        "if Experience.advanced || conn-card.show-all:",
    ];
    if EXPERT.iter().any(|f| header.starts_with(f)) {
        return Some(Level::Expert);
    }
    if ADVANCED.iter().any(|f| header.starts_with(f)) {
        return Some(Level::Advanced);
    }
    assert!(
        !header.contains("expert") && !header.contains("advanced"),
        "{at}: a level condition the settings index scan does not know: {header:?}"
    );
    None
}

/// One row found in a page.
#[derive(Debug)]
struct Row {
    at: String,
    label: Option<String>,
    find: Option<String>,
    none: bool,
    card: Option<String>,
    level: Level,
}

/// One card found in a page.
#[derive(Debug)]
struct Card {
    at: String,
    id: String,
    title: Option<String>,
}

fn static_text(value: &str) -> Option<String> {
    let inner = value.strip_prefix("@tr(\"")?.strip_suffix("\")")?;
    (!inner.contains('"')).then(|| inner.to_owned())
}

fn quoted(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_owned())
}

fn scan(name: &str, text: &str, rows: &mut Vec<Row>, cards: &mut Vec<Card>) {
    let mut stack: Vec<Open> = Vec::new();
    // Cards whose block is open, by stack depth, with their index in `cards`.
    let mut open_cards: Vec<(usize, usize)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let at = format!("{name}:{}", n + 1);
        let trimmed = line.trim();
        // Properties of the block this line is directly in.
        if let Some(top) = stack.last() {
            let depth = stack.len();
            if let Some(r) = top.row {
                if let Some(v) = trimmed.strip_prefix("label:") {
                    let v = v.trim().trim_end_matches(';').trim();
                    rows[r].label.get_or_insert_with(|| v.to_owned());
                } else if let Some(v) = trimmed.strip_prefix("find:") {
                    let v = v.trim().trim_end_matches(';').trim();
                    rows[r].find =
                        Some(quoted(v).unwrap_or_else(|| panic!("{at}: find takes a plain key")));
                } else if trimmed.starts_with("// find: none (") {
                    rows[r].none = true;
                }
            }
            if let Some(&(d, c)) = open_cards.last()
                && d == depth
            {
                if let Some(v) = trimmed.strip_prefix("id:") {
                    let v = v.trim().trim_end_matches(';').trim();
                    if let Some(id) = quoted(v) {
                        cards[c].id = id;
                    }
                } else if let Some(v) = trimmed.strip_prefix("title:") {
                    let v = v.trim().trim_end_matches(';').trim();
                    cards[c]
                        .title
                        .get_or_insert_with(|| static_text(v).unwrap_or_default());
                }
            }
        }
        let code = code(line);
        let mut before = String::new();
        for ch in code.chars() {
            match ch {
                '{' => {
                    let header = before.trim().to_owned();
                    let mut open = Open {
                        header: header.clone(),
                        card: None,
                        is_card: false,
                        row: None,
                    };
                    let element = header.trim_end();
                    if element.ends_with("SettingRow") || element.ends_with("SettingBlock") {
                        let rest = code[code.find('{').unwrap_or(0)..].trim_start_matches('{');
                        assert!(
                            rest.trim().is_empty() || rest.trim() == "}",
                            "{at}: write a row's properties on their own lines, so the settings index scan reads them"
                        );
                        let mut level = Level::Simple;
                        for h in stack.iter().map(|o| o.header.as_str()).chain([element]) {
                            match header_level(&at, h) {
                                Some(Level::Expert) => level = Level::Expert,
                                Some(Level::Advanced) if level == Level::Simple => {
                                    level = Level::Advanced;
                                }
                                _ => {}
                            }
                        }
                        let in_card = stack.iter().any(|o| o.is_card);
                        let card = stack.iter().rev().find_map(|o| o.card.clone());
                        assert!(
                            !in_card || card.is_some(),
                            "{at}: put the card's `id:` before its rows, so the settings index scan knows the card"
                        );
                        rows.push(Row {
                            at: at.clone(),
                            label: None,
                            find: None,
                            none: false,
                            card,
                            level,
                        });
                        open.row = Some(rows.len() - 1);
                    }
                    if element.ends_with("SettingsCard") {
                        open.is_card = true;
                        cards.push(Card {
                            at: at.clone(),
                            id: String::new(),
                            title: None,
                        });
                        open_cards.push((stack.len() + 1, cards.len() - 1));
                    }
                    stack.push(open);
                    before.clear();
                }
                '}' => {
                    if open_cards.last().is_some_and(|&(d, _)| d == stack.len()) {
                        open_cards.pop();
                    }
                    stack.pop();
                    before.clear();
                }
                _ => before.push(ch),
            }
        }
        // A card's id is known once its `id:` line is read: give it to the open block.
        if let Some(&(d, c)) = open_cards.last()
            && !cards[c].id.is_empty()
            && let Some(o) = stack.get_mut(d - 1)
        {
            o.card = Some(cards[c].id.clone());
        }
    }
    assert!(stack.is_empty(), "{name}: unbalanced braces");
}

fn pages() -> (Vec<Row>, Vec<Card>) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the ui folder")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".slint") && !NOT_PAGES.contains(&n.as_str()))
        .collect();
    names.sort();
    let mut rows = Vec::new();
    let mut cards = Vec::new();
    for n in names {
        let text = std::fs::read_to_string(format!("{dir}/{n}")).expect("a .slint file");
        scan(&n, &text, &mut rows, &mut cards);
    }
    (rows, cards)
}

#[test]
fn settings_search_index_matches_the_ui_files() {
    let (rows, cards) = pages();
    assert!(rows.len() > 50, "the scan found the rows: {}", rows.len());
    let mut named = BTreeSet::new();
    for r in &rows {
        let label = r.label.as_deref().and_then(static_text);
        let Some(key) = &r.find else {
            assert!(
                label.is_none() || r.none,
                "{}: the row {:?} needs `find: \"<key>\";` (an index key) or `// find: none (<why>).`",
                r.at,
                label.unwrap_or_default()
            );
            continue;
        };
        assert!(!r.none, "{}: a row with a key is not left out", r.at);
        let s = find(key).unwrap_or_else(|| panic!("{}: {key} is not in the settings index", r.at));
        assert!(!s.is_card(), "{}: {key} is a card key", r.at);
        if let Some(label) = label {
            assert_eq!(label, s.title, "{}: the index title of {key}", r.at);
        }
        assert_eq!(r.level, s.level, "{}: the index level of {key}", r.at);
        if let Some(card) = &r.card {
            assert!(
                s.places.iter().any(|p| p.card == card.as_str()),
                "{}: {key} shows in {card}, which its places do not list",
                r.at
            );
        }
        named.insert(key.as_str());
    }
    for s in SETTINGS.iter().filter(|s| !s.is_card()) {
        assert!(
            named.contains(s.key),
            "{} is in the index but no row names it",
            s.key
        );
    }
    let mut in_pages = BTreeMap::new();
    for c in cards.iter().filter(|c| c.id.starts_with("settings.")) {
        let s = find(&c.id)
            .unwrap_or_else(|| panic!("{}: the card {} is not in the settings index", c.at, c.id));
        assert_eq!(
            c.title.as_deref(),
            Some(s.title),
            "{}: the index title of {}",
            c.at,
            c.id
        );
        in_pages.insert(c.id.as_str(), c);
    }
    for s in SETTINGS.iter().filter(|s| s.is_card()) {
        assert!(
            in_pages.contains_key(s.key),
            "the card {} is in the index but not in the pages",
            s.key
        );
    }
}

#[test]
fn the_scan_reads_levels_cards_and_texts() {
    let text = r#"
component A {
    SettingsCard {
        title: @tr("Files");
        id: "settings.x.files";
        SettingRow {
            find: "x.one";
            label: @tr("One // {two}");
        }
        if root.advanced: VerticalLayout {
            if root.expert: SettingRow {
                label: root.dynamic;
            }
        }
    }
    SettingBlock {
        // find: none (a test).
        label: @tr("Free");
    }
}
"#;
    let mut rows = Vec::new();
    let mut cards = Vec::new();
    scan("t.slint", text, &mut rows, &mut cards);
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].id, "settings.x.files");
    assert_eq!(cards[0].title.as_deref(), Some("Files"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].find.as_deref(), Some("x.one"));
    assert_eq!(
        rows[0].label.as_deref().and_then(static_text).as_deref(),
        Some("One // {two}")
    );
    assert_eq!(rows[0].card.as_deref(), Some("settings.x.files"));
    assert_eq!(rows[0].level, Level::Simple);
    assert_eq!(rows[1].level, Level::Expert);
    assert_eq!(rows[1].card.as_deref(), Some("settings.x.files"));
    assert!(rows[2].none);
    assert_eq!(rows[2].card, None);
}

fn scan_text(text: &str) {
    scan("t.slint", text, &mut Vec::new(), &mut Vec::new());
}

#[test]
#[should_panic(expected = "on their own lines")]
fn the_scan_refuses_a_row_on_one_line() {
    scan_text("A {\n    SettingRow { label: @tr(\"X\"); }\n}\n");
}

#[test]
#[should_panic(expected = "level condition")]
fn the_scan_refuses_an_unknown_level_condition() {
    scan_text("A {\n    if !root.advanced: SettingRow {\n        find: \"x\";\n    }\n}\n");
}

#[test]
#[should_panic(expected = "card's `id:` before its rows")]
fn the_scan_refuses_rows_before_the_card_id() {
    scan_text(
        "A {\n    SettingsCard {\n        SettingRow {\n            find: \"x\";\n        }\n        id: \"settings.x.y\";\n    }\n}\n",
    );
}
