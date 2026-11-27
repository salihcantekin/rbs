use super::*;
use crate::builtin;
use crate::panel::id;

#[test]
fn closed_sections_round_trip_and_old_files_open_everything() {
    let mut w = builtin::edit();
    assert!(!w.to_toml().unwrap().contains("closed_sections"));
    w.set_section_closed("sources.global", true).unwrap();
    w.set_section_closed("properties.crop", true).unwrap();
    assert_eq!(w.closed_sections, ["properties.crop", "sources.global"]);
    let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
    assert_eq!(again.closed_sections, w.closed_sections);
    assert!(again.section_closed("sources.global"));
    assert!(!again.section_closed("mixer.global"));
}

#[test]
fn mixer_orientation_round_trips_and_keeps_unknown_options() {
    let mut w = builtin::edit();
    assert_eq!(w.mixer_orientation(), MixerOrientation::Auto);
    assert!(!w.to_toml().unwrap().contains("panel_options"));
    w.set_mixer_orientation(MixerOrientation::Columns);
    w.panel_options
        .entry("rbs.other".into())
        .or_default()
        .insert("x".into(), toml::Value::Integer(1));
    let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
    assert_eq!(again.mixer_orientation(), MixerOrientation::Columns);
    assert_eq!(again.panel_options, w.panel_options);
    let mut back = again;
    back.set_mixer_orientation(MixerOrientation::Auto);
    assert!(!back.panel_options.contains_key("rbs.mixer"));
    assert!(back.panel_options.contains_key("rbs.other"));
}

#[test]
fn an_invalid_mixer_orientation_means_auto() {
    let mut w = builtin::edit();
    w.panel_options
        .entry("rbs.mixer".into())
        .or_default()
        .insert("orientation".into(), toml::Value::String("diagonal".into()));
    assert_eq!(w.mixer_orientation(), MixerOrientation::Auto);
}

#[test]
fn mixer_scene_share_round_trips_and_clamps() {
    let mut w = builtin::edit();
    assert!((w.mixer_scene_share() - 0.5).abs() < f32::EPSILON);
    assert!(!w.to_toml().unwrap().contains("scene_share"));
    w.set_mixer_scene_share(0.35);
    let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
    assert!((again.mixer_scene_share() - 0.35).abs() < 1e-6);
    w.set_mixer_scene_share(0.5);
    assert!(
        !w.panel_options
            .get("rbs.mixer")
            .is_some_and(|o| o.contains_key("scene_share"))
    );
    w.panel_options
        .entry("rbs.mixer".into())
        .or_default()
        .insert("scene_share".into(), toml::Value::Float(2.0));
    assert!((w.mixer_scene_share() - 0.95).abs() < f32::EPSILON);
}

#[test]
fn closed_sections_are_checked_on_load() {
    let text = builtin::edit().to_toml().unwrap();
    let bad = |list: &str| {
        Workspace::from_toml(&text.replacen(
            "\n[[windows]]",
            &format!("\nclosed_sections = {list}\n[[windows]]"),
            1,
        ))
    };
    assert!(bad("[\"a.b\"]").is_ok());
    assert!(bad("[\"Bad Id\"]").is_err());
    assert!(bad("[\"b\", \"a\"]").is_err());
    assert!(bad("[\"a\", \"a\"]").is_err());
}

const SPEC_EXAMPLE: &str = r#"
format = 1
name = "Minimal live"
locked = true

[[windows]]
kind = "main"
layout = { split = "vertical", ratios = [0.78, 0.22], children = [
  { tabs = ["rbs.program"] },
  { tabs = ["rbs.live-controls", "rbs.mixer"] },
] }

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.stats"] }
placement = { display = "edid:DEL-41A3-7H2K9", connector = "DP-2", size = [2560, 1440], scale = 1.5, rect = [0, 0, 1280, 720] }

[[windows]]
kind = "projector"
content = "program?canvas=vertical"
placement = { display = "edid:SAM-7012-HX55", connector = "HDMI-1", size = [1920, 1080], scale = 1.0, fullscreen = true }
"#;

#[test]
fn reads_the_spec_example() {
    let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap();
    assert!(w.locked);
    assert_eq!(w.windows.len(), 3);
    assert_eq!(w.panels().len(), 4);
    assert!(
        matches!(&w.windows[2].content, WindowContent::Projector(c) if c == "program?canvas=vertical")
    );
    let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
    assert_eq!(again, w);
}

#[test]
fn export_strips_hardware_ids() {
    let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap().exported();
    let text = w.to_toml().unwrap();
    assert!(!text.contains("edid:") && !text.contains("DP-2"), "{text}");
    assert_eq!(
        w.windows[1].placement.as_ref().unwrap().size,
        Some([2560, 1440])
    );
}

#[test]
fn rejects_broken_files() {
    let cases = [
        ("format = 4\nname = \"x\"\nwindows = []", "newer"),
        ("name = \"x\"\nwindows = []", "missing"),
        ("format = 1\nname = \"x\"\nwindows = []", "first window"),
        (
            "format = 1\nname = \"x\"\n[[windows]]\nkind = \"floating\"\nlayout = { tabs = [\"rbs.mixer\"] }",
            "first window",
        ),
        (
            "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\"] }\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
            "exactly one",
        ),
        (
            "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\", \"rbs.mixer\"] }",
            "more than once",
        ),
        (
            "format = 1\nname = \"\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
            "name",
        ),
        (
            "format = 1\nname = \"x\"\n[[windows]]\nkind = \"projector\"\nlayout = { tabs = [] }",
            "content",
        ),
        (
            "format = 1\nname = \"x\"\nextra = 1\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [] }",
            "unknown field",
        ),
    ];
    for (text, needle) in cases {
        let e = Workspace::from_toml(text).unwrap_err().to_string();
        assert!(e.contains(needle), "{needle:?} not in {e:?}");
    }
}

#[test]
fn keeps_unknown_panels() {
    let text = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"com.example.chat\"] }";
    let w = Workspace::from_toml(text).unwrap();
    assert!(w.to_toml().unwrap().contains("com.example.chat"));
}

#[test]
fn close_open_float_dock() {
    let mut w = builtin::edit();
    let program = id("rbs.preview");
    let n = w.panels().len();
    w.close(&program).unwrap();
    assert!(!w.is_open(&program));
    assert_eq!(
        w.close(&program),
        Err(WorkspaceError::NotOpen(program.clone()))
    );
    w.open(program.clone()).unwrap();
    assert_eq!(w.panels().len(), n);
    // The Pictures leaf is fixed (ADR-0073): it cannot float.
    assert!(matches!(w.float(&program), Err(WorkspaceError::Invalid(_))));
    let stats = id("rbs.stats");
    let f = w.float(&stats).unwrap();
    assert_eq!(f, 1);
    assert_eq!(w.find(&stats).map(|l| l.0), Some(1));
    w.dock(f).unwrap();
    assert_eq!(w.windows.len(), 1);
    assert!(w.is_open(&stats) && w.is_open(&program));
    assert_eq!(w.dock(0), Err(WorkspaceError::NoTarget));
    w.validate().unwrap();
}

#[test]
fn closing_last_panel_of_floating_window_removes_it() {
    let mut w = builtin::edit();
    w.float(&id("rbs.stats")).unwrap();
    assert_eq!(w.windows.len(), 2);
    w.close(&id("rbs.stats")).unwrap();
    assert_eq!(w.windows.len(), 1);
}

#[test]
fn every_panel_can_be_closed_and_reopened() {
    for mut w in builtin::all() {
        let ids: Vec<PanelId> = w.panels().into_iter().cloned().collect();
        // Closing Preview closes the Pictures leaf, and Program with it.
        for p in &ids {
            if w.is_open(p) {
                w.close(p).unwrap();
            }
        }
        assert!(w.main().is_empty());
        for p in &ids {
            w.open(p.clone()).unwrap();
        }
        assert!(ids.iter().all(|p| w.is_open(p)));
        w.validate().unwrap();
    }
}

#[test]
fn move_between_stacks_and_windows() {
    let mut w = builtin::edit();
    let mixer = id("rbs.mixer");
    let stacks = w.windows[0].stack_count();
    let (_, s, _) = w.find(&mixer).unwrap();
    let target = (s + 1) % stacks;
    w.move_to_stack(&mixer, 0, target).unwrap();
    w.validate().unwrap();
    assert!(w.visible().contains(&&mixer));

    // From a floating window back into the main window: the floating window goes away.
    let f = w.float(&mixer).unwrap();
    w.move_to_stack(&mixer, 0, 0).unwrap();
    assert_eq!(w.windows.len(), f);
    assert_eq!(w.find(&mixer).map(|l| (l.0, l.1)), Some((0, 0)));

    // Into a floating window's stack.
    let f = w.float(&id("rbs.stats")).unwrap();
    w.move_to_stack(&mixer, f, 0).unwrap();
    assert_eq!(w.find(&mixer).map(|l| l.0), Some(f));
    assert_eq!(w.move_to_stack(&mixer, 9, 0), Err(WorkspaceError::NoTarget));
    w.validate().unwrap();
}

#[test]
fn edge_of_own_floating_window_is_a_no_op() {
    let mut w = builtin::edit();
    let stats = id("rbs.stats");
    let f = w.float(&stats).unwrap();
    w.float(&id("rbs.outputs")).unwrap();
    let before = w.clone();
    w.move_to_edge(&stats, f, Edge::Left).unwrap();
    assert_eq!(w, before);
    // Last window too: no index out of bounds.
    let g = w.float(&id("rbs.scenes")).unwrap();
    w.move_to_edge(&id("rbs.scenes"), g, Edge::Top).unwrap();
    w.validate().unwrap();
}

#[test]
fn empty_stacks_only_at_the_main_root() {
    let base = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\n";
    let nested = format!(
        "{base}layout = {{ split = \"vertical\", ratios = [0.5, 0.5], children = [{{ tabs = [] }}, {{ tabs = [\"rbs.mixer\"] }}] }}"
    );
    assert!(Workspace::from_toml(&nested).is_err());
    let floating = format!(
        "{base}layout = {{ tabs = [] }}\n[[windows]]\nkind = \"floating\"\nlayout = {{ split = \"vertical\", ratios = [0.5, 0.5], children = [{{ tabs = [] }}, {{ tabs = [] }}] }}"
    );
    assert!(Workspace::from_toml(&floating).is_err());
    assert!(Workspace::from_toml(&format!("{base}layout = {{ tabs = [] }}")).is_ok());
}

#[test]
fn move_to_edge_from_floating_window() {
    let mut w = builtin::edit();
    let stats = id("rbs.stats");
    w.float(&stats).unwrap();
    w.float(&id("rbs.outputs")).unwrap();
    // Moving the panel of window 1 into window 2 renumbers window 2 to 1.
    w.move_to_edge(&stats, 2, Edge::Right).unwrap();
    assert_eq!(w.windows.len(), 2);
    assert_eq!(w.find(&stats).map(|l| l.0), Some(1));
    // The main window's left edge is the outer side of its left area.
    w.move_to_edge(&stats, 0, Edge::Left).unwrap();
    let left = main_of(&w)
        .left
        .layout
        .as_ref()
        .map(|l| l.panels().contains(&&stats));
    assert_eq!(left, Some(true));
    w.validate().unwrap();
}

/// The format 2 example of the spec, without the parts that come with the studio
/// split (fixed-size children).
const FORMAT_2: &str = r#"
format = 2
name = "Edit"
toolbar = { style = "icons-text", items = [
  "|", "ui.workspace.select", "|", "|", "scene.create", "source.add", "|",
  "~", "ui.area.toggle?area=left", "stream.toggle", "record.toggle", "|",
] }

[[windows]]
kind = "main"
centre = { tabs = ["rbs.preview"] }
left = { size = 260, layout = { split = "vertical", ratios = [0.4, 0.6], children = [
  { tabs = ["rbs.scenes"] },
  { tabs = ["rbs.sources"] },
] } }
right = { size = 290, visible = false, layout = { split = "vertical", ratios = [0.6, 0.25, 0.15], children = [
  { tabs = ["rbs.properties"] },
  { tabs = ["rbs.live-controls"] },
  { tabs = ["rbs.transition"], state = "collapsed" },
] } }
bottom = { size = 200, layout = { tabs = ["rbs.mixer"] } }
auto_hide = [
  { edge = "left", tabs = ["rbs.library"], size = 340, anchor = "rbs.scenes" },
  { edge = "right", tabs = ["rbs.stats", "rbs.outputs"], active = 1, anchor = "rbs.properties" },
]

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.status-bar"] }
"#;

fn main_of(w: &Workspace) -> &MainLayout {
    w.main_layout().unwrap()
}

#[test]
fn reads_and_writes_format_2() {
    let w = Workspace::from_toml(FORMAT_2).unwrap();
    let m = main_of(&w);
    assert_eq!((m.left.size, m.right.size, m.bottom.size), (260, 290, 200));
    assert!(m.left.visible && !m.right.visible);
    assert_eq!(m.auto_hide.len(), 2);
    assert_eq!(m.auto_hide[1].size, DEFAULT_FLYOUT);
    assert_eq!(m.auto_hide[1].active, 1);
    let collapsed = w.windows[0].stack(5).unwrap();
    assert_eq!(collapsed.tabs, vec![id("rbs.transition")]);
    assert!(collapsed.collapsed);
    // Separators tidied on load.
    let items = &w.toolbar.as_ref().unwrap().items;
    assert_eq!(
        items.first(),
        Some(&ToolbarItem::Command("ui.workspace.select".into()))
    );
    assert_eq!(
        items.last(),
        Some(&ToolbarItem::Command("record.toggle".into()))
    );
    assert_eq!(
        items
            .iter()
            .filter(|i| **i == ToolbarItem::Separator)
            .count(),
        2
    );
    let again = Workspace::from_toml(&w.to_toml().unwrap()).unwrap();
    assert_eq!(again, w);
}

#[test]
fn stacks_are_numbered_through_the_centre_then_the_areas() {
    let w = Workspace::from_toml(FORMAT_2).unwrap();
    let main = &w.windows[0];
    assert_eq!(main.stack_count(), 7);
    let order: Vec<&str> = (0..7)
        .map(|s| main.stack(s).unwrap().tabs[0].as_str())
        .collect();
    assert_eq!(
        order,
        [
            "rbs.preview",
            "rbs.scenes",
            "rbs.sources",
            "rbs.properties",
            "rbs.live-controls",
            "rbs.transition",
            "rbs.mixer"
        ]
    );
    assert_eq!(w.find(&id("rbs.mixer")), Some((0, 6, 0)));
    // Auto-hidden panels are open but in no stack.
    assert_eq!(w.find(&id("rbs.stats")), None);
    assert!(w.is_open(&id("rbs.stats")));
}

#[test]
fn visible_skips_hidden_areas_collapsed_stacks_and_auto_hidden_panels() {
    let w = Workspace::from_toml(FORMAT_2).unwrap();
    let visible: Vec<&str> = w.visible().iter().map(|p| p.as_str()).collect();
    assert_eq!(
        visible,
        [
            "rbs.preview",
            "rbs.scenes",
            "rbs.sources",
            "rbs.mixer",
            "rbs.status-bar"
        ]
    );
}

#[test]
fn format_1_becomes_the_centre() {
    let w = Workspace::from_toml(SPEC_EXAMPLE).unwrap();
    assert_eq!(w.format, FORMAT);
    let m = main_of(&w);
    assert_eq!(m.centre.panels().len(), 3);
    for s in Side::ALL {
        assert_eq!(*m.area(s), Area::empty(s));
    }
    assert!(m.auto_hide.is_empty() && w.toolbar.is_none());
    let text = w.to_toml().unwrap();
    assert!(
        text.contains("format = 3") && text.contains("centre"),
        "{text}"
    );
    assert!(
        !text.contains("left") && !text.contains("auto_hide"),
        "{text}"
    );
    // Format 2 keys are not format 1.
    let mixed = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\nlayout = { tabs = [\"rbs.mixer\"], state = \"collapsed\" }";
    let e = Workspace::from_toml(mixed).unwrap_err().to_string();
    assert!(e.contains("state"), "{e}");
    let mixed = "format = 1\nname = \"x\"\n[[windows]]\nkind = \"main\"\ncentre = { tabs = [] }";
    assert!(Workspace::from_toml(mixed).is_err());
}

#[test]
fn rejects_broken_format_2_files() {
    let main = "format = 2\nname = \"x\"\n[[windows]]\nkind = \"main\"\n";
    let cases = [
        (format!("{main}layout = {{ tabs = [] }}"), "centre"),
        (
            format!("{main}centre = {{ tabs = [\"rbs.preview\"], state = \"collapsed\" }}"),
            "not collapsed",
        ),
        (
            format!("{main}centre = {{ tabs = [] }}\nleft = {{ layout = {{ tabs = [] }} }}"),
            "empty area",
        ),
        (
            format!("{main}centre = {{ tabs = [] }}\nleft = {{ size = 20000 }}"),
            "at most",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [\"rbs.mixer\"] }}\nbottom = {{ layout = {{ tabs = [\"rbs.mixer\"] }} }}"
            ),
            "more than once",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"top\", tabs = [\"rbs.stats\"] }}]"
            ),
            "unknown variant",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [] }}]"
            ),
            "1 to 32",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [\"rbs.stats\"], active = 1 }}]"
            ),
            "out of range",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [] }}\nauto_hide = [{{ edge = \"left\", tabs = [\"rbs.stats\"], anchor = \"rbs.stats\" }}]"
            ),
            "anchor",
        ),
        (
            format!(
                "{main}centre = {{ tabs = [] }}\n[[windows]]\nkind = \"floating\"\nlayout = {{ tabs = [\"rbs.stats\"] }}\nleft = {{ size = 200 }}"
            ),
            "only the main window",
        ),
        (
            format!(
                "toolbar = {{ items = [\"~\", \"a.b\", \"~\"] }}\n{main}centre = {{ tabs = [] }}"
            ),
            "flexible space",
        ),
        (
            format!("toolbar = {{ items = [\"a.b\", \"a.b\"] }}\n{main}centre = {{ tabs = [] }}"),
            "more than once",
        ),
        (
            format!("toolbar = {{ items = [\"Not An Id\"] }}\n{main}centre = {{ tabs = [] }}"),
            "toolbar item",
        ),
    ];
    for (text, needle) in cases {
        let e = Workspace::from_toml(&text).unwrap_err().to_string();
        assert!(e.contains(needle), "{needle:?} not in {e:?} for {text}");
    }
}

#[test]
fn panels_move_out_of_areas_and_edge_groups() {
    let mut w = Workspace::from_toml(FORMAT_2).unwrap();
    // The bottom area's only panel moves into the centre: the area becomes empty.
    w.move_to_stack(&id("rbs.mixer"), 0, 0).unwrap();
    assert_eq!(main_of(&w).bottom.layout, None);
    assert_eq!(w.find(&id("rbs.mixer")), Some((0, 0, 1)));
    // A lone centre stack stays when its panel moves into an area: the area's
    // stacks keep their numbers.
    let mut v = Workspace::from_toml(FORMAT_2).unwrap();
    v.move_to_stack(&id("rbs.preview"), 0, 6).unwrap();
    assert_eq!(v.find(&id("rbs.preview")), Some((0, 6, 1)));
    assert!(v.main().is_empty());
    v.validate().unwrap();
    // A stack of an area that vanishes renumbers the later ones.
    let mut v = Workspace::from_toml(FORMAT_2).unwrap();
    v.move_to_stack(&id("rbs.scenes"), 0, 6).unwrap();
    assert_eq!(v.find(&id("rbs.scenes")), Some((0, 5, 1)));
    v.validate().unwrap();
    // Out of an edge group into a stack; the group goes when empty.
    w.move_to_stack(&id("rbs.library"), 0, 1).unwrap();
    assert_eq!(main_of(&w).auto_hide.len(), 1);
    assert!(w.find(&id("rbs.library")).is_some());
    // Opening an auto-hidden panel makes it the group's tab; closing removes it.
    w.open(id("rbs.stats")).unwrap();
    assert_eq!(main_of(&w).auto_hide[0].active, 0);
    w.close(&id("rbs.outputs")).unwrap();
    assert_eq!(main_of(&w).auto_hide[0].tabs, vec![id("rbs.stats")]);
    w.float(&id("rbs.stats")).unwrap();
    assert!(main_of(&w).auto_hide.is_empty());
    w.move_to_edge(&id("rbs.properties"), 0, Edge::Left)
        .unwrap();
    w.validate().unwrap();
}

#[test]
fn move_beside_splits_the_target_stack() {
    let mut w = builtin::edit();
    let (mixer, scenes) = (id("rbs.mixer"), id("rbs.scenes"));
    let before = w.windows[0].stack_count();
    let (_, s, _) = w.find(&scenes).unwrap();
    w.move_beside(&mixer, 0, s, Edge::Right).unwrap();
    w.validate().unwrap();
    // Mixer was the bottom area's only panel: it left that stack and has its own,
    // right after Scenes.
    let (_, m, _) = w.find(&mixer).unwrap();
    let (_, s2, _) = w.find(&scenes).unwrap();
    assert_eq!(m, s2 + 1);
    assert_eq!(w.windows[0].stack_count(), before);
    // The only tab of a stack dropped beside that stack: nothing changes.
    let same = w.clone();
    w.move_beside(&mixer, 0, m, Edge::Top).unwrap();
    assert_eq!(w, same);
    // From a floating window: the floating window goes away.
    let f = w.float(&id("rbs.stats")).unwrap();
    let (_, s, _) = w.find(&scenes).unwrap();
    w.move_beside(&id("rbs.stats"), 0, s, Edge::Bottom).unwrap();
    assert_eq!(w.windows.len(), f);
    assert_eq!(w.find(&id("rbs.stats")).map(|l| l.1), Some(s + 1));
    assert_eq!(
        w.move_beside(&mixer, 0, 99, Edge::Left),
        Err(WorkspaceError::NoTarget)
    );
    w.validate().unwrap();
}

#[test]
fn collapse_keeps_one_expanded_stack_in_the_centre() {
    let mut ws = builtin::edit();
    assert!(ws.set_collapsed(&id("rbs.properties"), true).is_ok());
    let (_, stack, _) = ws.find(&id("rbs.properties")).unwrap();
    assert!(ws.windows[0].stack(stack).unwrap().collapsed);
    // The Pictures leaf is fixed: it cannot collapse (ADR-0073).
    assert!(ws.set_collapsed(&id("rbs.preview"), true).is_err());
    // With the leaf closed, the centre's only stack cannot collapse either.
    ws.close(&id("rbs.program")).unwrap();
    ws.open(id("rbs.scenes")).unwrap();
    ws.main_layout_mut().unwrap().centre = Node::tabs(vec![id("rbs.notes")]);
    assert!(ws.set_collapsed(&id("rbs.notes"), true).is_err());
    let (_, stack, _) = ws.find(&id("rbs.notes")).unwrap();
    assert!(!ws.windows[0].stack(stack).unwrap().collapsed);
    assert!(ws.set_collapsed(&id("rbs.properties"), false).is_ok());
    assert!(ws.validate().is_ok());
}

#[test]
fn auto_hide_and_pin_round_trip_in_an_area() {
    let mut ws = builtin::edit();
    let before = ws.main_layout().unwrap().right.clone();
    let content = Rect::sized(1440.0, 800.0);
    ws.auto_hide(&id("rbs.properties"), content).unwrap();
    let m = ws.main_layout().unwrap();
    let g = &m.auto_hide[m.group_of(&id("rbs.properties")).unwrap()];
    assert_eq!(g.edge, Side::Right);
    assert!(ws.validate().is_ok());
    ws.pin(&id("rbs.properties")).unwrap();
    let after = &ws.main_layout().unwrap().right;
    // Back in the right area.
    assert_eq!(
        after.layout.as_ref().unwrap().panels(),
        before.layout.as_ref().unwrap().panels()
    );
    assert!(ws.validate().is_ok());
}

#[test]
fn pin_without_a_docked_anchor_goes_to_its_area() {
    let mut ws = builtin::edit();
    ws.main_layout_mut().unwrap().right.visible = false;
    ws.pin(&id("rbs.stats")).unwrap();
    let m = ws.main_layout().unwrap();
    assert!(m.right.visible);
    assert!(
        m.right
            .layout
            .as_ref()
            .unwrap()
            .find(&id("rbs.outputs"))
            .is_some()
    );
    // Stats left the right edge; the Library stays on the left.
    assert!(m.auto_hide.iter().all(|g| g.edge != Side::Right));
    assert!(ws.validate().is_ok());
}

#[test]
fn auto_hide_from_the_centre_picks_the_nearest_edge() {
    let mut ws = builtin::live();
    let content = Rect::sized(1440.0, 800.0);
    // Stats is in the centre's right column.
    ws.auto_hide(&id("rbs.stats"), content).unwrap();
    let m = ws.main_layout().unwrap();
    assert_eq!(m.auto_hide[0].edge, Side::Right);
    // A centre that is one stack keeps it.
    let mut ws = builtin::edit();
    ws.close(&id("rbs.program")).unwrap();
    assert!(ws.auto_hide(&id("rbs.preview"), content).is_err());
}

#[test]
fn floating_windows_do_not_auto_hide() {
    let mut ws = builtin::edit();
    ws.float(&id("rbs.properties")).unwrap();
    assert_eq!(
        ws.auto_hide(&id("rbs.properties"), Rect::sized(800.0, 600.0)),
        Err(WorkspaceError::NoTarget)
    );
}

#[test]
fn the_vertical_picture_opens_beside_the_pictures_and_closes_again() {
    // ADR-0093 decision 7: a stack of its own right of the pictures, never a tab
    // next to Preview or Program, and closing it leaves the rest as it was.
    fn right_of_pictures(n: &Node, id: &PanelId) -> bool {
        match n {
            Node::Split(s) => {
                s.children.windows(2).any(|w| {
                    s.axis == crate::layout::Axis::Horizontal
                        && matches!(&w[0], Node::Pictures(_))
                        && matches!(&w[1], Node::Tabs(t) if t.tabs == [id.clone()])
                }) || s.children.iter().any(|c| right_of_pictures(c, id))
            }
            _ => false,
        }
    }
    let vertical = id("rbs.program?canvas=vertical");
    for mut w in [builtin::live(), builtin::edit()] {
        let before = w.clone();
        w.open(vertical.clone()).unwrap();
        let (win, stack, _) = w.find(&vertical).unwrap();
        assert_eq!(win, 0, "{}", w.name);
        assert_eq!(
            w.windows[0].stack(stack).unwrap().tabs,
            vec![vertical.clone()]
        );
        assert!(w.main().pictures_leaf().is_some());
        assert!(!w.windows[0].in_pictures(&vertical));
        assert!(right_of_pictures(w.main(), &vertical), "{}", w.name);
        w.validate().unwrap();
        w.close(&vertical).unwrap();
        assert!(!w.is_open(&vertical));
        assert_eq!(w.windows, before.windows, "{}", w.name);
    }
}

#[test]
fn opening_a_panel_shows_its_area_expands_it_and_selects_its_edge_tab() {
    let mut w = builtin::edit();
    let properties = id("rbs.properties");
    w.main_layout_mut().unwrap().right.visible = false;
    w.set_collapsed(&properties, true).unwrap();
    w.open(properties.clone()).unwrap();
    assert!(w.main_layout().unwrap().right.visible);
    let (_, stack, _) = w.find(&properties).unwrap();
    assert!(!w.windows[0].stack(stack).unwrap().collapsed);

    w.auto_hide(&properties, Rect::sized(1440.0, 800.0))
        .unwrap();
    w.open(properties.clone()).unwrap();
    let m = w.main_layout().unwrap();
    let g = m.group_of(&properties).unwrap();
    let group = &m.auto_hide[g];
    assert_eq!(
        group.tabs.iter().position(|p| *p == properties),
        Some(group.active)
    );
}

#[test]
fn opening_properties_never_lands_beside_the_pictures() {
    // The bug of 2026-11-08: Properties became a tab next to Preview.
    for mut w in [builtin::live(), builtin::edit()] {
        w.close(&id("rbs.properties")).ok();
        w.open(id("rbs.properties")).unwrap();
        let (win, stack, _) = w.find(&id("rbs.properties")).unwrap();
        let tabs = &w.windows[win].stack(stack).unwrap().tabs;
        assert!(
            !tabs.contains(&id("rbs.preview")) && !tabs.contains(&id("rbs.program")),
            "{}: {tabs:?}",
            w.name
        );
        assert!(w.main().pictures_leaf().is_some());
        assert!(w.is_open(&id("rbs.preview")) && w.is_open(&id("rbs.program")));
        w.validate().unwrap();
    }
}

#[test]
fn no_command_puts_a_panel_beside_the_pictures() {
    // A deterministic walk over open, close, move, float, collapse and auto-hide:
    // Preview and Program only ever belong to the Pictures leaf.
    let panels = [
        "rbs.scenes",
        "rbs.sources",
        "rbs.properties",
        "rbs.mixer",
        "rbs.stats",
        "rbs.outputs",
        "rbs.live-controls",
        "rbs.preview",
        "rbs.program",
        "rbs.pictures",
    ];
    let mut seed: u32 = 12345;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) as usize % n
    };
    for start in builtin::all().into_iter().take(2) {
        let mut w = start;
        for _ in 0..400 {
            let p = id(panels[next(panels.len())]);
            let stacks = w.windows[0].stack_count().max(1);
            let _ = match next(8) {
                0 | 1 => w.open(p),
                2 => w.close(&p),
                3 => w.move_to_stack(&p, 0, next(stacks)),
                4 => w.move_to_edge(&p, 0, Edge::Left),
                5 => w.set_collapsed(&p, next(2) == 0),
                6 => w.float(&p).map(|_| ()),
                _ => w.auto_hide(&p, Rect::sized(1000.0, 600.0)),
            };
            w.validate()
                .unwrap_or_else(|e| panic!("{e}: {}", w.to_toml().unwrap()));
            for win in &w.windows {
                for s in 0..win.stack_count() {
                    let tabs = &win.stack(s).unwrap().tabs;
                    for t in tabs {
                        assert!(
                            !matches!(t.as_str(), "rbs.preview" | "rbs.program")
                                || w.main().pictures_leaf().is_none(),
                            "{t} sits in a stack beside the leaf"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_format_2_studio_split_becomes_the_pictures_leaf() {
    let text = r#"format = 2
name = "Old live"
[[windows]]
kind = "main"
[windows.centre]
split = "horizontal"
studio = "below"
ratios = [0.5, 0.5]
children = [{ tabs = ["rbs.preview"] }, { tabs = ["rbs.program"] }]
"#;
    let w = Workspace::from_toml(text).unwrap();
    assert_eq!(w.format, FORMAT);
    assert_eq!(w.studio_rail(), Some(RailAt::Below));
    assert_eq!(w.rail_size(), Some(crate::layout::RAIL_SIZE_DEFAULT));
    let out = w.to_toml().unwrap();
    assert!(
        out.contains("pictures = true") && !out.contains("studio"),
        "{out}"
    );
    assert_eq!(Workspace::from_toml(&out).unwrap(), w);
    // Format 3 is not readable by a format 2 reader's rules, and two leaves are refused.
    let two = out.replace(
            "pictures = true",
            "pictures = true\n[windows.left]\nsize = 200\n[windows.left.layout]\npictures = true\nrail = \"below\"\nx = 1",
        );
    assert!(Workspace::from_toml(&two).is_err());
}

#[test]
fn the_rail_size_is_set_within_its_limits() {
    let mut w = builtin::edit();
    w.set_rail_size(150).unwrap();
    assert_eq!(w.rail_size(), Some(150));
    w.set_rail_size(1).unwrap();
    assert_eq!(w.rail_size(), Some(crate::layout::RAIL_SIZE_RANGE.0));
    w.close(&id("rbs.pictures")).unwrap();
    assert_eq!(w.set_rail_size(100), Err(WorkspaceError::NoTarget));
    assert_eq!(
        w.set_studio_rail(RailAt::Below),
        Err(WorkspaceError::NoTarget)
    );
}

#[test]
fn the_leaf_cannot_move_collapse_hide_or_float() {
    let mut w = builtin::edit();
    let pic = id("rbs.pictures");
    assert!(w.set_collapsed(&pic, true).is_err());
    assert!(w.auto_hide(&pic, Rect::sized(1000.0, 600.0)).is_err());
    assert!(w.float(&pic).is_err());
    assert!(w.move_to_stack(&pic, 0, 0).is_err());
    assert!(w.move_to_edge(&pic, 0, Edge::Left).is_err());
    // Opening it while it exists changes nothing; closing it and opening it again
    // brings it back as the first child of the centre.
    let before = w.clone();
    w.open(pic.clone()).unwrap();
    assert_eq!(w, before);
    w.close(&pic).unwrap();
    assert!(w.main().pictures_leaf().is_none());
    w.open(id("rbs.preview")).unwrap();
    assert!(w.main().pictures_leaf().is_some());
    w.validate().unwrap();
}

#[test]
fn full_height_areas_round_trip_and_old_files_are_not_full_height() {
    let mut w = builtin::edit();
    assert!(!w.to_toml().unwrap().contains("full_height"));
    w.set_area_full(Side::Right, true).unwrap();
    let text = w.to_toml().unwrap();
    assert!(text.contains("full_height"));
    let again = Workspace::from_toml(&text).unwrap();
    assert!(again.main_layout().is_some_and(|m| m.right.full_height));
    assert!(again.main_layout().is_some_and(|m| !m.left.full_height));
    assert!(w.set_area_full(Side::Bottom, true).is_err());
}

#[test]
fn a_panel_moved_to_the_full_height_edge_makes_that_area_full_height() {
    let mut w = builtin::edit();
    w.move_to_full_edge(&id("rbs.mixer"), Edge::Left).unwrap();
    let m = w.main_layout().expect("main window");
    assert!(m.left.full_height && m.left.layout.is_some());
    assert!(w.move_to_full_edge(&id("rbs.mixer"), Edge::Top).is_err());
}
