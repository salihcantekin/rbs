#![allow(clippy::expect_used)] // Tests fail by panicking.

use super::*;
use rbs_ui::command::{ENTRIES, Kind, entry};

/// The window files that declare the menu bar (ADR-0138: one file per window group).
const WINDOW_FILES: [&str; 2] = ["ui/main_window.slint", "ui/settings_window.slint"];

#[test]
fn runtime_registry_methods_use_the_shared_catalog() {
    for command in ENTRIES {
        assert!(
            registry::check_entry(Some(command), |_, _| {}).is_some(),
            "{} references a method outside the shared catalog",
            command.id
        );
    }

    let mut unlisted = *entry("scene.add").expect("scene.add entry");
    unlisted.kind = Kind::Action {
        method: "test.not_in_catalog",
    };
    let mut reported = None;
    assert!(
        registry::check_entry(Some(&unlisted), |id, method| {
            reported = Some((id, method));
        })
        .is_none()
    );
    assert_eq!(reported, Some(("scene.add", "test.not_in_catalog")));
    let mut unlisted_toggle = *entry("record.toggle").expect("record toggle");
    unlisted_toggle.kind = Kind::Toggle {
        on: "record.start",
        off: "test.not_in_catalog",
    };
    assert!(
        registry::check_entry(Some(&unlisted_toggle), |id, method| {
            assert_eq!((id, method), ("record.toggle", "test.not_in_catalog"));
        })
        .is_none()
    );

    // Client-scoped methods are intentionally separate from engine methods.
    assert!(
        registry::check_entry(entry("ui.profile.new"), |_, _| {
            panic!("valid UI method must not be reported")
        })
        .is_some()
    );
    assert!(
        registry::check_entry(None, |_, _| {
            panic!("unknown entries must not be reported")
        })
        .is_none()
    );

    // Catalog validation does not flatten special command semantics.
    let toggle = entry("record.toggle").expect("resolved toggle");
    assert!(matches!(toggle.kind, Kind::ResolvedToggle { .. }));
    assert_eq!(toggle.kind.methods(), ["record.start", "record.stop"]);
    let picker = entry("source.add").expect("source picker");
    assert!(matches!(picker.kind, Kind::Picker { .. }));
    assert_eq!(picker.kind.methods(), ["source.add"]);
}

#[test]
fn the_window_files_have_the_current_menu_bar() {
    let mut copies = 0;
    for file in WINDOW_FILES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let text = std::fs::read_to_string(&path).expect("the window file is readable");
        // A Windows checkout may have CRLF line ends; the generated text has LF.
        let text = text.replace("\r\n", "\n");
        let fresh = refreshed(&text).expect("the window file has the menu bar markers");
        if fresh != text && std::env::var_os("RBS_UPDATE_MENU_BAR").is_some() {
            std::fs::write(&path, &fresh).expect("the window file is writable");
            copies += fresh.matches(BEGIN).count();
            continue;
        }
        assert!(
            fresh == text,
            "the menu bar in {file} is out of date: run \
             `RBS_UPDATE_MENU_BAR=1 cargo test -p rbs-app --bin rbs menu_bar`"
        );
        copies += text.matches(BEGIN).count();
    }
    // Main, floating and Settings windows each declare the menu bar (macOS).
    assert_eq!(copies, 3);
}

#[test]
fn values_follow_the_generated_order() {
    let shape = slint_text("");
    let v = values(&Shape);
    // One `Menus.titles[i]` per value, numbered in order.
    for i in 0..v.titles.len() {
        assert!(shape.contains(&format!("Menus.titles[{i}]")), "{i}");
    }
    assert!(!shape.contains(&format!("Menus.titles[{}]", v.titles.len())));
    for i in 0..v.choices.len() {
        assert!(shape.contains(&format!("Menus.choices[{i}]")), "{i}");
    }
    assert_eq!(v.enabled.len(), v.titles.len());
    assert_eq!(v.checked.len(), v.titles.len());
}

#[test]
fn the_transition_outside_studio_mode_checks_the_current_kind() {
    use rbs_scene::{CollectionTransition, TransitionKind as K};
    let t = CollectionTransition::default();
    let choices = direct_choices(t, 400);
    let titles: Vec<_> = choices.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Cut",
            "Fade (300 ms)",
            "Move (400 ms)",
            "Swipe (500 ms)",
            "Slide (500 ms)",
            "Stinger (no file: a cut)"
        ]
    );
    let checked: Vec<_> = choices.iter().filter(|c| c.2).collect();
    assert_eq!(checked.len(), 1);
    assert_eq!(checked[0].1, "move");
    // Every value names a kind the run parses back (the picker's `direct_kind`).
    for (_, value, _) in &choices {
        assert!(K::parse(value).is_some(), "{value}");
    }
    // A 0 ms glide makes Move a cut, and the title says so.
    let fade = CollectionTransition {
        direct: K::Fade,
        ..t
    };
    let still = direct_choices(fade, 0);
    assert_eq!(still[2].0, "Move (0 ms: a cut)");
    assert!(still[1].2);
}

#[test]
fn every_settings_section_has_its_window_number() {
    let mut seen: Vec<i32> = rbs_ui::command::SETTINGS_SECTIONS
        .iter()
        .map(|s| settings_section(s).expect("known section"))
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..15).collect::<Vec<_>>());
    assert_eq!(settings_section("mixer"), None);
    let run = |t: &str| open_settings(&CommandId::parse(t).expect("id"));
    assert_eq!(run("ui.settings.open"), Some(Run::OpenSettings(None, None)));
    assert_eq!(
        run("ui.settings.open?section=shortcuts"),
        Some(Run::OpenSettings(Some(6), None))
    );
    assert_eq!(run("ui.settings.open?section=mixer"), None);
}

/// `setting` names an index key in its section, `command` a command in Shortcuts
/// (spec `settings-search.md`).
#[test]
fn settings_open_at_a_setting() {
    let run = |t: &str| open_settings(&CommandId::parse(t).expect("id"));
    let at = |section, key: &str| {
        Some(Run::OpenSettings(
            Some(section),
            Some(SettingsTarget::Setting(key.to_owned())),
        ))
    };
    assert_eq!(
        run("ui.settings.open?section=stream&setting=encoding.b-frames"),
        at(2, "encoding.b-frames")
    );
    // Without a section: the key's first section.
    assert_eq!(
        run("ui.settings.open?setting=encoding.b-frames"),
        at(1, "encoding.b-frames")
    );
    assert_eq!(
        run("ui.settings.open?setting=settings.recording.replay"),
        at(1, "settings.recording.replay")
    );
    // Not shown in that section, unknown, or a command as a key.
    assert_eq!(
        run("ui.settings.open?section=general&setting=encoding.b-frames"),
        None
    );
    assert_eq!(run("ui.settings.open?setting=nope"), None);
    assert_eq!(run("ui.settings.open?setting=record.main.toggle"), None);
    // A command in Shortcuts, with or without the section.
    let shortcut = Some(Run::OpenSettings(
        Some(6),
        Some(SettingsTarget::Command("record.main.toggle".to_owned())),
    ));
    assert_eq!(run("ui.settings.open?command=record.main.toggle"), shortcut);
    assert_eq!(
        run("ui.settings.open?command=record.main.toggle&section=shortcuts"),
        shortcut
    );
    assert_eq!(
        run("ui.settings.open?command=record.main.toggle&section=stream"),
        None
    );
    assert_eq!(run("ui.settings.open?command=no.such"), None);
    // Only a bare id: a parameterised command has no row of its own.
    assert_eq!(
        run("ui.settings.open?command=ui.area.toggle%3Farea%3Dleft"),
        None
    );
    assert_eq!(
        run("ui.settings.open?command=record.main.toggle&setting=encoding.b-frames"),
        None
    );
}

#[test]
fn the_row_splits_at_the_space_and_tidies_separators() {
    let item = |kind, id: &str| ToolItem {
        id: id.to_owned(),
        title: id.to_owned(),
        kind,
        enabled: true,
        checked: false,
        live: false,
        icon: String::new(),
        icon_only: false,
    };
    let c = |id: &str| item(ToolKind::Command, id);
    let sep = || item(ToolKind::Separator, "");
    let space = || item(ToolKind::Space, "");
    let ids = |v: &[ToolItem]| {
        v.iter()
            .map(|i| if i.id.is_empty() { "|" } else { i.id.as_str() })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let row = split_row(vec![
        sep(),
        c("a"),
        sep(),
        sep(),
        c("b"),
        sep(),
        space(),
        sep(),
        c("c"),
        space(),
        c("d"),
        sep(),
    ]);
    assert_eq!(ids(&row.lead), "a | b");
    assert_eq!(ids(&row.end), "c d");
    let row = split_row(vec![c("a"), sep(), c("b")]);
    assert_eq!(ids(&row.lead), "a | b");
    assert!(row.end.is_empty());
    assert_eq!(split_row(vec![sep(), space(), sep()]), Row::default());
}

#[test]
fn titles_are_quoted_for_slint() {
    assert_eq!(quoted("a \"b\" \\"), "\"a \\\"b\\\" \\\\\"");
}

#[test]
fn refreshing_keeps_the_text_around_the_markers() {
    // A marker no menu title contains ("old" is in "Hold stream").
    let text = format!("a\n    {BEGIN}\n    stale-line\n    {END}\nb\n");
    let fresh = refreshed(&text).expect("markers");
    assert!(fresh.starts_with(&format!(
        "a\n    {BEGIN}\n    if Shell.native-menu-bar: MenuBar {{\n"
    )));
    assert!(fresh.ends_with(&format!("    }}\n    {END}\nb\n")));
    assert!(!fresh.contains("stale-line"));
    assert_eq!(refreshed("no markers"), None);
}
