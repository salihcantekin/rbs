//! Tests of the command registry.

use super::*;

#[test]
fn every_entry_has_a_category_rule() {
    for e in ENTRIES {
        assert!(
            Category::rule(e.id).is_some(),
            "{} has no category rule",
            e.id
        );
    }
}

#[test]
fn categories_follow_what_a_command_acts_on() {
    assert_eq!(Category::of("scene.next"), Category::Scene);
    assert_eq!(
        Category::of("scene.select?scene=a&collection=b"),
        Category::Scene
    );
    assert_eq!(
        Category::of("scene.audio.own_levels.toggle"),
        Category::Audio
    );
    assert_eq!(Category::of("source.muted.toggle"), Category::Audio);
    assert_eq!(Category::of("source.monitor?monitor=off"), Category::Audio);
    assert_eq!(Category::of("source.rename"), Category::Source);
    assert_eq!(Category::of("profile.select?profile=x"), Category::Profiles);
    assert_eq!(Category::of("ui.collection.new"), Category::Profiles);
    assert_eq!(Category::of("stream.toggle"), Category::Output);
    assert_eq!(Category::of("canvas.transition"), Category::Scene);
    assert_eq!(Category::of("canvas.add_vertical"), Category::Output);
    assert_eq!(Category::of("ui.panel.close?id=x"), Category::Layout);
    assert_eq!(Category::of("edit.undo"), Category::App);
    assert_eq!(Category::of("com.example.thing"), Category::App);
    // Every category has a command, so no filter shows an empty list by design.
    for c in Category::ALL {
        assert!(
            ENTRIES.iter().any(|e| Category::of(e.id) == c),
            "{c:?} is empty"
        );
    }
}

struct Fake;

impl CommandState for Fake {
    fn enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        if id.base() == "edit.redo" {
            Err("Nothing to redo".into())
        } else {
            Ok(())
        }
    }
    fn checked(&self, id: &CommandId) -> bool {
        id.param("area") == Some("left")
    }
    fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
        vec![
            ("Edit".into(), "Edit".into(), true),
            ("Live show".into(), "Live show".into(), false),
        ]
    }
}

#[test]
fn ids_are_canonical_and_round_trip() {
    let id = CommandId::parse("ui.workspace.switch?name=Live%20show").unwrap();
    assert_eq!(id.param("name"), Some("Live show"));
    assert_eq!(id.to_string(), "ui.workspace.switch?name=Live%20show");
    let a = CommandId::parse("x.y?b=1&a=2").unwrap();
    let b = CommandId::parse("x.y?a=2&b=1").unwrap();
    assert_eq!(a, b);
    assert_eq!(a.to_string(), "x.y?a=2&b=1");
    assert_eq!(
        CommandId::with_params("ui.workspace.switch", &[("name", "Live show")]).unwrap(),
        id
    );
    let turkish = CommandId::parse("ui.workspace.switch?name=%C3%87anl%C4%B1").unwrap();
    assert_eq!(turkish.param("name"), Some("Çanlı"));
    assert_eq!(CommandId::parse(&turkish.to_string()).unwrap(), turkish);
    for bad in [
        "",
        "nodot",
        "Up.per",
        "a.b?x",
        "a.b?x=1&x=2",
        "a.b?x=%zz",
        "a.b?x=a b",
        "A.b",
        "a.b?x=%FF",
        "a.b?x=",
    ] {
        assert!(CommandId::parse(bad).is_err(), "{bad}");
    }
    assert!(CommandId::parse(&format!("a.{}", "b".repeat(127))).is_err());
}

#[test]
fn parameters_are_checked_against_the_entry() {
    let ok = CommandId::parse("ui.area.toggle?area=left").unwrap();
    assert_eq!(ok.resolve().unwrap().unwrap().id, "ui.area.toggle");
    assert!(
        CommandId::parse("ui.area.toggle?area=top")
            .unwrap()
            .resolve()
            .is_err()
    );
    assert!(
        CommandId::parse("edit.undo?x=1")
            .unwrap()
            .resolve()
            .is_err()
    );
    assert_eq!(
        CommandId::parse("com.example.thing").unwrap().resolve(),
        Ok(None)
    );
}

#[test]
fn registry_is_consistent() {
    let mut ids: Vec<&str> = ENTRIES.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), ENTRIES.len(), "duplicate ids");
    for e in ENTRIES {
        CommandId::plain(e.id).unwrap();
        assert!(!e.title.is_empty() && !e.tooltip.is_empty(), "{}", e.id);
        for m in e.kind.methods() {
            CommandId::plain(m).unwrap();
        }
        if matches!(e.kind, Kind::Toggle { .. } | Kind::ResolvedToggle { .. }) {
            assert!(e.checkable, "{}", e.id);
        }
        // Every entry is in its own menu exactly once (area toggles once per area).
        let placed: Vec<&str> = MENU_LAYOUT
            .iter()
            .flat_map(|(m, items)| items.iter().map(move |i| (*m, *i)))
            .filter(|(_, i)| CommandId::parse(i).is_ok_and(|id| id.base() == e.id))
            .map(|(m, _)| {
                assert_eq!(m, e.menu, "{} is in the wrong menu", e.id);
                m.title()
            })
            .collect();
        assert_eq!(
            placed.is_empty(),
            NOT_IN_MENU.contains(&e.id),
            "{} is in no menu, or in one and in NOT_IN_MENU",
            e.id
        );
    }
    for (_, items) in MENU_LAYOUT {
        for i in *items {
            if *i != "|" && *i != "}" && !i.starts_with(['#', '{']) {
                let id = CommandId::parse(i).unwrap();
                assert!(id.resolve().unwrap().is_some(), "{i}");
                assert_eq!(id.to_string(), *i, "menu ids are written canonically");
            }
        }
    }
}

#[test]
fn maximize_names_its_panel_and_add_source_is_on_the_default_toolbar() {
    let id = CommandId::parse("ui.panel.maximize?id=rbs.preview").unwrap();
    let e = id.resolve().unwrap().unwrap();
    assert_eq!(title(&id, e), "Maximize Preview");
    assert_eq!(icon(&id, e), Some("expand"));
    assert!(
        default_toolbar()
            .items
            .contains(&ToolbarItem::Command("source.add".into()))
    );
}

#[test]
fn settings_is_last_on_the_default_toolbar_as_an_icon() {
    let t = default_toolbar();
    assert_eq!(
        t.items.last(),
        Some(&ToolbarItem::Command("ui.settings.open".into()))
    );
    let id = CommandId::parse("ui.settings.open").unwrap();
    let e = id.resolve().unwrap().unwrap();
    assert_eq!(icon(&id, e), Some("gear"));
    assert!(icon_only(&id));
    assert_eq!(e.menu, Menu::File);
    for s in SETTINGS_SECTIONS {
        let id = CommandId::parse(&format!("ui.settings.open?section={s}")).unwrap();
        assert!(id.resolve().unwrap().is_some(), "{s}");
    }
    let bad = CommandId::parse("ui.settings.open?section=mixer").unwrap();
    assert!(bad.resolve().is_err());
}

#[test]
fn menus_follow_state() {
    let m = menus(&Fake);
    let titles: Vec<&str> = m.iter().map(|(m, _)| m.title()).collect();
    assert_eq!(
        titles,
        [
            "File", "Edit", "View", "Scene", "Source", "Output", "Window", "Help"
        ]
    );
    let file = &m[0].1;
    let MenuItem::Choices { title, choices, .. } = &file[1] else {
        panic!("scene collection picker")
    };
    assert_eq!(title, "Scene collection");
    assert_eq!(choices[1].1.to_string(), "collection.select?id=Live%20show");
    let edit = &m[1].1;
    assert!(
        matches!(&edit[1], MenuItem::Command { disabled: Some(r), .. } if r == "Nothing to redo")
    );
    let view = &m[2].1;
    let MenuItem::Choices { choices, .. } = &view[0] else {
        panic!("workspace picker")
    };
    assert_eq!(
        choices[1].1.to_string(),
        "ui.workspace.select?name=Live%20show"
    );
    assert!(choices[1].1.resolve().unwrap().is_some());
    assert!(choices[0].2 && !choices[1].2);
    let areas = view
        .iter()
        .find_map(|i| match i {
            MenuItem::Choices { title, choices, .. } if title == "Areas" => Some(choices),
            _ => None,
        })
        .expect("Areas submenu");
    assert!(areas.iter().any(|(t, _, on)| t == "Left area" && *on));
    assert!(areas.iter().any(|(t, _, on)| t == "Right area" && !*on));
}

#[test]
fn context_layouts_use_known_commands_and_fit() {
    for c in Context::ALL {
        assert_eq!(Context::parse(c.name()), Some(c));
        let (_, ids) = CONTEXT_LAYOUT
            .iter()
            .find(|(l, _)| *l == c)
            .expect("every context has a layout");
        let mut shown = 0;
        let mut grouped = false;
        for i in *ids {
            match *i {
                "}" => grouped = false,
                _ if i.starts_with('{') => {
                    grouped = true;
                    shown += 1;
                }
                _ if !grouped => shown += 1,
                _ => {}
            }
        }
        assert!(shown <= MAX_CONTEXT_ITEMS, "{c:?}");
        for i in *ids {
            if *i != "|" && *i != "}" && !i.starts_with('{') {
                let i = i.strip_prefix('*').unwrap_or(i);
                let id = CommandId::parse(i).unwrap();
                assert!(entry(id.base()).is_some(), "{i}");
                assert_eq!(id.to_string(), i, "layout ids are written canonically");
            }
        }
    }
    assert_eq!(Context::parse("nothing"), None);
}

#[test]
fn groups_collapse_sections_in_the_menu_bar() {
    let app = app_menu(&Fake);
    let (_, view) = app.iter().find(|(m, _)| *m == Menu::View).unwrap();
    assert!(matches!(
        &view[0],
        AppRow::Item(MenuItem::Choices { title, .. }) if title == "Workspace"
    ));
    assert!(view.iter().any(|r| matches!(
        r,
        AppRow::Item(MenuItem::Choices { title, .. }) if title == "Areas"
    )));
    assert!(view.iter().any(|r| matches!(
        r,
        AppRow::Item(MenuItem::Choices { title, .. }) if title == "Display"
    )));
    assert!(!matches!(
        view.last(),
        Some(AppRow::Heading(_) | AppRow::Separator)
    ));
    let bar = menus(&Fake);
    let (_, view) = bar.iter().find(|(m, _)| *m == Menu::View).unwrap();
    assert!(
        !matches!(view[0], MenuItem::Separator),
        "no separator leads a menu"
    );
    assert!(view.iter().any(|i| matches!(i, MenuItem::Separator)));
    let (_, source) = bar.iter().find(|(m, _)| *m == Menu::Source).unwrap();
    assert!(source.iter().any(|i| matches!(
        i,
        MenuItem::Choices { title, .. } if title == "Transform"
    )));
    assert!(search(&Fake, " ").is_empty());
    let found = search(&Fake, "SETTINGS stream");
    assert!(found.iter().any(|i| matches!(i, MenuItem::Command { id, .. } if id.to_string() == "ui.settings.open?section=stream")));
}

/// The search finds single settings, after every command, with the section and the
/// setting in the id (spec `settings-search.md`).
#[test]
fn search_finds_settings_after_the_commands() {
    let found = search(&Fake, "keyframe");
    let ids: Vec<String> = found
        .iter()
        .filter_map(|i| match i {
            MenuItem::Command { id, .. } => Some(id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        [
            "ui.settings.open?section=recording&setting=encoding.keyframe-interval",
            "ui.settings.open?section=stream&setting=encoding.keyframe-interval",
        ]
    );
    let MenuItem::Command { title, .. } = &found[0] else {
        panic!("a command")
    };
    assert_eq!(title, "Settings: Recording › Encoding › Keyframe interval");
    // The menu's words count, as for sections.
    assert_eq!(search(&Fake, "settings keyframe").len(), 2);
    // A card.
    let found = search(&Fake, "replay buffer");
    assert!(
        found
            .iter()
            .any(|i| matches!(i, MenuItem::Command { id, title, .. }
        if id.to_string() == "ui.settings.open?section=recording&setting=settings.recording.replay"
            && title == "Settings: Recording › Replay buffer"))
    );
    // Commands come first.
    let found = search(&Fake, "record");
    let first_setting = found
        .iter()
        .position(|i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some()))
        .unwrap_or(found.len());
    assert!(
        found[first_setting..]
            .iter()
            .all(|i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some()))
    );
    assert!(first_setting > 0);
    // A word in many commands still leaves room for settings.
    let found = search(&Fake, "e");
    assert_eq!(found.len(), MAX_SEARCH_RESULTS);
    let settings = found
        .iter()
        .filter(|i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some()))
        .count();
    assert!(settings >= SETTINGS_SEARCH_ROOM, "{settings}");
    // Ids parse back with both parameters.
    let id = CommandId::parse("ui.settings.open?section=stream&setting=encoding.b-frames").unwrap();
    assert_eq!(id.param("setting"), Some("encoding.b-frames"));
}

/// Every command has an icon from the set, so no menu item is blank (spec,
/// "Icons": menus show an icon on every command).
#[test]
fn every_command_has_an_icon_from_the_set() {
    for e in ENTRIES {
        let name = e.icon.unwrap_or_else(|| panic!("{} has no icon", e.id));
        assert!(crate::icon::find(name).is_some(), "{}: {name}", e.id);
    }
    let layouts = MENU_LAYOUT
        .iter()
        .flat_map(|(_, ids)| ids.iter())
        .chain(CONTEXT_LAYOUT.iter().flat_map(|(_, ids)| ids.iter()));
    for text in layouts.filter(|t| **t != "|" && **t != "}" && !t.starts_with(['#', '{'])) {
        let text = text.strip_prefix('*').unwrap_or(text);
        let id = CommandId::parse(text).unwrap();
        let e = entry(id.base()).unwrap();
        let name = icon(&id, e).unwrap_or_else(|| panic!("{text} has no icon"));
        assert!(crate::icon::find(name).is_some(), "{text}: {name}");
    }
}

#[test]
fn picker_choices_have_icons_except_while_checked() {
    let screen = CommandId::parse("source.add?kind=screen").unwrap();
    assert_eq!(choice_icon(&screen, false), Some("monitor"));
    assert_eq!(choice_icon(&screen, true), None);
    let off = CommandId::parse("source.monitor?monitor=off&source=1").unwrap();
    assert_eq!(choice_icon(&off, false), Some("headphones-off"));
    let theme = CommandId::parse("ui.theme.select?id=builtin%3Adark").unwrap();
    assert_eq!(choice_icon(&theme, false), Some("palette"));
}

#[test]
fn an_inline_picker_lists_its_choices_as_items_or_says_why_it_cannot() {
    struct Types(bool);
    impl CommandState for Types {
        fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
            if self.0 {
                Ok(())
            } else {
                Err("The studio is not running.".into())
            }
        }
        fn checked(&self, _: &CommandId) -> bool {
            false
        }
        fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
            vec![("Blur".into(), "rbs.filter.blur".into(), false)]
        }
    }
    let target = [("source".to_owned(), "3".to_owned())];
    let items = context_menu(Context::AddFilter, &target, &Types(true));
    assert_eq!(ids(&items), ["filter.add?source=3&type=rbs.filter.blur"]);
    // The Audio tab's button says so in its target, and the choices carry it.
    let audio = [target[0].clone(), ("audio".to_owned(), "1".to_owned())];
    let items = context_menu(Context::AddFilter, &audio, &Types(true));
    assert_eq!(
        ids(&items),
        ["filter.add?audio=1&source=3&type=rbs.filter.blur"]
    );
    let items = context_menu(Context::AddFilter, &target, &Types(false));
    assert!(matches!(
        &items[0],
        MenuItem::Command { disabled: Some(r), .. } if r == "The studio is not running."
    ));
}

/// Fills targets, leaves out what does not apply, and tidies separators.
struct Picky;

impl CommandState for Picky {
    fn enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        match id.base() {
            "scene.remove" => Err("The last scene cannot be removed.".into()),
            _ => Ok(()),
        }
    }
    fn checked(&self, id: &CommandId) -> bool {
        id.base() == "item.visible.toggle"
    }
    fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
        Vec::new()
    }
    fn title(&self, id: &CommandId) -> Option<String> {
        (id.base() == "scene.select").then(|| "Put on air".into())
    }
    fn applies(&self, id: &CommandId) -> bool {
        !matches!(id.base(), "source.muted.toggle" | "scene.select")
    }
}

fn ids(items: &[MenuItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            MenuItem::Command { id, .. } => id.to_string(),
            MenuItem::Choices { title, .. } => format!("[{title}]"),
            MenuItem::Separator => "|".into(),
        })
        .collect()
}

#[test]
fn context_menus_fill_targets_and_tidy_separators() {
    let t = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let scene = context_menu(Context::Scene, &t(&[("scene", "7")]), &Picky);
    assert_eq!(
        ids(&scene),
        [
            "scene.rename?scene=7",
            "scene.duplicate?scene=7",
            "scene.duplicate?scene=7&sources=copied",
            "scene.active.toggle?scene=7",
            "scene.audio.own_levels.toggle?scene=7",
            "ui.scene.transition?scene=7",
            "|",
            "scene.move?direction=up&scene=7",
            "scene.move?direction=down&scene=7",
            "|",
            "scene.add",
            "|",
            "scene.remove?scene=7"
        ]
    );
    assert!(matches!(
        scene.iter().find(|i| matches!(i, MenuItem::Command { id, .. } if id.to_string().starts_with("scene.remove"))),
        Some(MenuItem::Command { disabled: Some(r), .. }) if r == "The last scene cannot be removed."
    ));
    let source = context_menu(
        Context::Source,
        &t(&[("item", "40"), ("source", "12")]),
        &Picky,
    );
    assert_eq!(
        ids(&source),
        [
            "[Transform]",
            "|",
            "[Arrange]",
            "|",
            "source.active.toggle?source=12",
            "item.visible.toggle?item=40",
            "item.locked.toggle?item=40",
            "|",
            "[Source]",
            "[Attributes]",
            "|",
            "item.remove?item=40",
            "source.delete?source=12"
        ]
    );
    assert!(matches!(
        &source[5],
        MenuItem::Command {
            checked: Some(true),
            ..
        }
    ));
    // A picker without its choice is a submenu; with it, one item with the
    // state's title.
    let preview = context_menu(Context::Preview, &t(&[("id", "rbs.program")]), &Picky);
    assert_eq!(
        ids(&preview),
        [
            "[Add source]",
            "|",
            "canvas.studio_mode.toggle",
            "ui.panel.maximize?id=rbs.program",
            "|",
            "ui.projector.open?canvas=main"
        ]
    );
    // The Preview panel shows the program outside studio mode, so it opens
    // the main canvas's projector too.
    let main_preview = context_menu(Context::Preview, &t(&[("id", "rbs.preview")]), &Picky);
    assert_eq!(
        ids(&main_preview).last().map(String::as_str),
        Some("ui.projector.open?canvas=main")
    );
    // A source on a picture offers the projector of that picture's canvas.
    let on_picture = context_menu(
        Context::Source,
        &t(&[("item", "1"), ("source", "2"), ("picture", "main")]),
        &Picky,
    );
    assert_eq!(
        ids(&on_picture).last().map(String::as_str),
        Some("ui.projector.open?canvas=main")
    );
    // Without the panel, Maximize has no required `id` and is left out.
    let bare = context_menu(Context::Preview, &[], &Picky);
    assert_eq!(
        ids(&bare),
        ["[Add source]", "|", "canvas.studio_mode.toggle"]
    );
    struct OnAir;
    impl CommandState for OnAir {
        fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
            Ok(())
        }
        fn checked(&self, _: &CommandId) -> bool {
            false
        }
        fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
            Vec::new()
        }
        fn title(&self, id: &CommandId) -> Option<String> {
            (id.base() == "scene.select").then(|| "Put on air".into())
        }
    }
    let other = context_menu(Context::Scene, &t(&[("scene", "3")]), &OnAir);
    assert!(matches!(
        &other[0],
        MenuItem::Command { id, title, .. }
            if id.to_string() == "scene.select?scene=3" && title == "Put on air"
    ));
}

#[test]
fn a_picker_listed_twice_in_a_group_names_itself_in_each_choice() {
    let menus = app_menu(&Fake);
    let (_, output) = menus.iter().find(|(m, _)| *m == Menu::Output).unwrap();
    let vertical = output
        .iter()
        .find_map(|r| match r {
            AppRow::Item(MenuItem::Choices { title, choices, .. }) if title == "Vertical" => {
                Some(choices)
            }
            _ => None,
        })
        .unwrap();
    let titles: Vec<&str> = vertical.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(
        titles[..4],
        [
            "Recording: Edit",
            "Recording: Live show",
            "Going live: Edit",
            "Going live: Live show"
        ]
    );
    assert!(titles.contains(&"Vertical preview"));
    // A picker listed once keeps its choices as they are (View's Display group).
    let (_, view) = menus.iter().find(|(m, _)| *m == Menu::View).unwrap();
    assert!(view.iter().any(|r| matches!(
        r,
        AppRow::Item(MenuItem::Choices { choices, .. })
            if choices.iter().any(|(t, _, _)| t == "Live show")
    )));
}

#[test]
fn the_vertical_panel_menu_hides_the_picture_and_removes_the_output_apart() {
    let target = [("id".to_owned(), VERTICAL_PANEL.to_owned())];
    let titles: Vec<String> = context_menu(Context::Panel, &target, &Fake)
        .into_iter()
        .filter_map(|i| match i {
            MenuItem::Command { title, .. } => Some(title),
            _ => None,
        })
        .collect();
    let n = titles.len();
    assert_eq!(
        titles[n - 2..],
        ["Hide vertical preview", "Remove vertical output"]
    );
    // Other panels keep Close and have no Remove vertical output.
    let mixer = [("id".to_owned(), "rbs.mixer".to_owned())];
    let titles: Vec<String> = context_menu(Context::Panel, &mixer, &Fake)
        .into_iter()
        .filter_map(|i| match i {
            MenuItem::Command { title, .. } => Some(title),
            _ => None,
        })
        .collect();
    assert_eq!(titles.last().map(String::as_str), Some("Close"));
}

struct Layouts;

impl CommandState for Layouts {
    fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
        Ok(())
    }
    fn checked(&self, _: &CommandId) -> bool {
        false
    }
    fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
        if id.base() == "ui.mixer.orientation" {
            vec![
                ("Automatic".into(), "auto".into(), true),
                ("Rows".into(), "rows".into(), false),
                ("Columns".into(), "columns".into(), false),
            ]
        } else {
            Vec::new()
        }
    }
}

fn layout_submenu(items: &[MenuItem]) -> Option<Vec<(&str, bool)>> {
    items.iter().find_map(|i| match i {
        MenuItem::Choices { title, choices, .. } if title == "Mixer layout" => {
            Some(choices.iter().map(|(t, _, on)| (t.as_str(), *on)).collect())
        }
        _ => None,
    })
}

#[test]
fn the_mixer_menus_offer_the_layout_as_a_submenu() {
    let want = vec![("Automatic", true), ("Rows", false), ("Columns", false)];
    assert_eq!(
        layout_submenu(&context_menu(Context::Mixer, &[], &Layouts)),
        Some(want.clone())
    );
    assert_eq!(
        layout_submenu(&context_menu(
            Context::MixerSource,
            &[("source".into(), "1".into())],
            &Layouts,
        )),
        Some(want.clone())
    );
    assert_eq!(
        layout_submenu(&context_menu(
            Context::Panel,
            &[("id".into(), "rbs.mixer".into())],
            &Layouts,
        )),
        Some(want)
    );
    // Another panel's menu does not grow a Mixer layout.
    assert_eq!(
        layout_submenu(&context_menu(
            Context::Panel,
            &[("id".into(), "rbs.sources".into())],
            &Layouts,
        )),
        None
    );
    let MenuItem::Choices { choices, .. } = &context_menu(Context::Mixer, &[], &Layouts)[0] else {
        panic!("layout submenu");
    };
    assert_eq!(
        choices[0].1.to_string(),
        "ui.mixer.orientation?orientation=auto"
    );
}

#[test]
fn default_toolbar_uses_known_commands() {
    let t = default_toolbar();
    for i in &t.items {
        if let ToolbarItem::Command(text) = i {
            let id = CommandId::parse(text).unwrap();
            assert!(id.resolve().unwrap().is_some(), "{text}");
        }
    }
}

#[test]
fn required_parameters_pickers_and_escapes() {
    // A required parameter must be there; a picker's choice need not.
    assert!(
        CommandId::parse("ui.area.toggle")
            .unwrap()
            .resolve()
            .is_err()
    );
    let bare = CommandId::parse("ui.workspace.select").unwrap();
    assert!(bare.resolve().unwrap().is_some());
    let pick = CommandId::parse("ui.workspace.select?name=Live%20show").unwrap();
    assert_eq!(pick.resolve().unwrap().unwrap().id, "ui.workspace.select");
    for bad in ["a.b?x=%+1", "a.b?x=%2", "a.b?x=%2g"] {
        assert!(CommandId::parse(bad).is_err(), "{bad}");
    }
    // Lower case escapes are read and written in capitals.
    let lower = CommandId::parse("ui.workspace.switch?name=a%2ab").unwrap();
    assert_eq!(lower.to_string(), "ui.workspace.switch?name=a%2Ab");
}
