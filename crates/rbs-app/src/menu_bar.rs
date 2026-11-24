//! The menu bar, built from the command registry (ADR-0050, U20).
//!
//! Slint needs a window's `MenuBar` written out in the window itself, so the menus'
//! shape (which menus, items, separators and choice submenus, in order) is generated
//! from [`rbs_ui::command::MENU_LAYOUT`] by `slint_text` (tests only) and kept in
//! `ui/main_window.slint` and `ui/settings_window.slint` between marker lines; a test fails when that copy is out of date (run it with
//! `RBS_UPDATE_MENU_BAR=1` to rewrite it). What changes at run time (titles, enabled
//! and checked states, a picker's choices) comes from [`Values`], indexed in the same
//! order, through the `Menus` global.

use rbs_api::types::OutputKind;
use rbs_scene::model::SourceMonitor;
use rbs_ui::command::{CommandId, CommandState, Disabled, Kind, MenuItem, menus};
use rbs_ui::layout::RailAt;
use rbs_ui::panel::PanelId;
use rbs_ui::projector::{ProjectorCanvas, ProjectorSize};
use rbs_ui::workspace::{MixerOrientation, Side, Workspace};

use crate::studio_host::{OutputCanvas, STREAMING_OFF, TransformOp};
use crate::studio_ui::filters::filter_types;
use crate::studio_ui::multi_select::ArrangeOp;
use crate::studio_ui::{SourceFacts, StudioUi, VerticalOut, rec_state, st_state};

/// The run-time values of the menu bar, in the generated items' order: one entry
/// per command or choice submenu in `titles`, `enabled` and `checked`, one list per
/// choice submenu in `choices` (`(title, id to run, current)`). `icons` names each
/// item's icon from the set, or is empty while a checkmark takes its place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values {
    pub titles: Vec<String>,
    pub icons: Vec<&'static str>,
    pub enabled: Vec<bool>,
    pub checked: Vec<bool>,
    pub choices: Vec<Vec<(String, String, bool)>>,
}

/// The values for the shell's state.
pub fn values(state: &dyn CommandState) -> Values {
    let mut v = Values::default();
    for (_, items) in menus(state) {
        for item in items {
            match item {
                MenuItem::Command {
                    title,
                    disabled,
                    checked,
                    icon,
                    ..
                } => {
                    v.titles.push(title);
                    v.icons.push(item_icon(icon, checked));
                    v.enabled.push(disabled.is_none());
                    v.checked.push(checked.unwrap_or(false));
                }
                MenuItem::Choices {
                    title,
                    icon,
                    disabled,
                    choices,
                } => {
                    v.titles.push(title);
                    v.icons.push(item_icon(icon, None));
                    v.enabled.push(disabled.is_none() && !choices.is_empty());
                    v.checked.push(false);
                    v.choices.push(
                        choices
                            .into_iter()
                            .map(|(t, id, current)| (t, id.to_string(), current))
                            .collect(),
                    );
                }
                MenuItem::Separator => {}
            }
        }
    }
    v
}

/// The icon a menu item shows: its own, or none while it is checked, since menus draw
/// the checkmark in the icon's place (spec, "Icons").
pub fn item_icon(icon: Option<&'static str>, checked: Option<bool>) -> &'static str {
    if checked == Some(true) {
        ""
    } else {
        icon.unwrap_or("")
    }
}

#[cfg(test)]
/// A state with nothing in it: [`menus`] then gives the shape alone.
struct Shape;

#[cfg(test)]
impl CommandState for Shape {
    fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
        Ok(())
    }
    fn checked(&self, _: &CommandId) -> bool {
        false
    }
    fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
        Vec::new()
    }
}

#[cfg(test)]
/// `s` as a Slint string literal.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
/// The generated `MenuBar` element, each line indented by `indent`.
pub fn slint_text(indent: &str) -> String {
    let mut out = String::new();
    let mut line = |depth: usize, text: &str| {
        out.push_str(indent);
        for _ in 0..depth {
            out.push_str("    ");
        }
        out.push_str(text);
        out.push('\n');
    };
    // macOS only: Windows and Linux use the RBS menu instead (spec, "RBS menu").
    line(0, "if Shell.native-menu-bar: MenuBar {");
    let mut index = 0usize;
    let mut picker = 0usize;
    for (menu, items) in menus(&Shape) {
        line(1, "Menu {");
        line(2, &format!("title: @tr({});", quoted(menu.title())));
        for item in items {
            match item {
                MenuItem::Command { id, checked, .. } => {
                    let run = quoted(&id.to_string());
                    let mut props = format!(
                        "MenuItem {{ title: Menus.titles[{index}]; icon: Menus.icons[{index}]; enabled: Menus.enabled[{index}];"
                    );
                    if checked.is_some() {
                        props.push_str(&format!(
                            " checkable: true; checked: Menus.checked[{index}];"
                        ));
                    }
                    props.push_str(&format!(" activated => {{ Menus.run({run}); }} }}"));
                    line(2, &props);
                    index += 1;
                }
                MenuItem::Choices { .. } => {
                    line(2, "Menu {");
                    line(3, &format!("title: Menus.titles[{index}];"));
                    line(3, &format!("icon: Menus.icons[{index}];"));
                    line(3, &format!("enabled: Menus.enabled[{index}];"));
                    line(
                        3,
                        &format!(
                            "for c in Menus.choices[{picker}]: MenuItem {{ title: c.title; icon: c.icon; checkable: true; checked: c.checked; activated => {{ Menus.run(c.id); }} }}"
                        ),
                    );
                    line(2, "}");
                    index += 1;
                    picker += 1;
                }
                MenuItem::Separator => line(2, "MenuSeparator { }"),
            }
        }
        line(1, "}");
    }
    line(0, "}");
    out
}

#[cfg(test)]
/// The begin and end marker lines around each generated copy in the window files.
pub const BEGIN: &str = "// BEGIN generated menu bar (menu_bar.rs; do not edit)";
#[cfg(test)]
pub const END: &str = "// END generated menu bar";

#[cfg(test)]
/// `text` with every generated copy replaced by the current one, or `None` when it
/// has no marker pair or a broken one.
pub fn refreshed(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut found = false;
    while let Some(begin) = rest.find(BEGIN) {
        let line_start = rest[..begin].rfind('\n').map_or(0, |i| i + 1);
        let indent = &rest[line_start..begin];
        let after = begin + BEGIN.len();
        let end = after + rest[after..].find(END)?;
        let end_line = rest[..end].rfind('\n').map_or(0, |i| i + 1);
        out.push_str(&rest[..after]);
        out.push('\n');
        out.push_str(&slint_text(indent));
        out.push_str(&rest[end_line..end]);
        rest = &rest[end..];
        found = true;
    }
    if !found {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

/// The shell's state as the registry asks for it.
pub struct Shown<'a> {
    pub workspace: &'a Workspace,
    /// Workspace names, with whether each is built in.
    pub workspaces: &'a [(String, bool)],
    pub studio: &'a StudioUi,
    /// The panels the Window menu lists: `(title, id, open)`.
    pub panels: Vec<(String, String, bool)>,
    /// View › Theme: `(name, id, current)`, at most ten.
    pub themes: Vec<(String, String, bool)>,
    /// View › Settings shown (ADR-0113).
    pub experience: crate::background::Experience,
    /// View › Spacing: the density in use (ADR-0113 decision 2).
    pub density: rbs_ui::tokens::Density,
    /// The maximized panel (ADR-0049 decision 2).
    pub maximized: Option<&'a PanelId>,
    /// The virtual camera, for `vcam.toggle`.
    pub camera: CameraShown,
    /// The replay buffer, for `replay.toggle` and `replay.save`.
    pub replay: ReplayShown,
    /// The layout history has a step to undo.
    pub can_undo: bool,
    /// The layout history has a step to redo.
    pub can_redo: bool,
    /// The open projector windows (ADR-0125).
    pub projectors: Vec<crate::projector::Shown>,
    /// The connected displays by name, for a projector's Full screen.
    pub displays: Vec<String>,
}

/// The virtual camera as `vcam.toggle` sees it (ADR-0066 decision 12).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CameraShown {
    /// This platform has a virtual camera.
    pub available: bool,
    /// It is added, nothing else is being done to it, and it does not start now.
    pub ready: bool,
    /// RBS sends to it.
    pub sending: bool,
}

/// The replay buffer as `replay.toggle` and `replay.save` see it (ADR-0088).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayShown {
    /// The buffer keeps the past now.
    pub armed: bool,
    /// A clip can be saved now.
    pub can_save: bool,
}

/// What the Add source picker lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddWhat {
    /// What a source kind can capture (its code in `SourceKind::ALL`).
    Kind(i32),
    /// The sources other scenes have (UI only, not an engine kind).
    Existing,
}

/// The `kind` choice that lists the sources of other scenes (UI only: the control
/// API's `source.add` has no such kind).
const EXISTING: &str = "existing";

fn panel_of(id: &CommandId) -> Option<PanelId> {
    PanelId::parse(id.param("id")?).ok()
}

fn rail_at(id: &CommandId) -> Option<RailAt> {
    match id.param("at")? {
        "between" => Some(RailAt::Between),
        "below" => Some(RailAt::Below),
        _ => None,
    }
}

fn side(id: &CommandId) -> Option<Side> {
    match id.param("area")? {
        "left" => Some(Side::Left),
        "right" => Some(Side::Right),
        "bottom" => Some(Side::Bottom),
        _ => None,
    }
}

impl Shown<'_> {
    fn areas(&self) -> impl Iterator<Item = (Side, bool, bool)> + '_ {
        let main = self.workspace.main_layout();
        Side::ALL.into_iter().map(move |s| {
            main.map_or((s, false, false), |m| {
                let a = m.area(s);
                (s, a.layout.is_some(), a.visible)
            })
        })
    }

    /// The source an id names (`source` or `item`), or the selected one.
    fn source(&self, id: &CommandId) -> Result<SourceFacts, Disabled> {
        self.studio
            .source_facts(id.param("source"), id.param("item"))
            .ok_or_else(|| match (id.param("source"), id.param("item")) {
                (None, None) => "No source is selected.".into(),
                _ => "The source is not in this scene.".into(),
            })
    }

    /// The scene an id names, or the one being worked on.
    fn scene(&self, id: &CommandId) -> Result<i32, Disabled> {
        self.studio
            .scene_target(id.param("scene"))
            .ok_or_else(|| "The scene is gone.".into())
    }

    fn running(&self) -> Result<(), Disabled> {
        match self.studio.state {
            1 => Ok(()),
            0 => Err("The studio is starting.".into()),
            _ => Err("The studio stopped.".into()),
        }
    }

    /// The vertical canvas's picture panel is open (in any window).
    fn vertical_panel_open(&self) -> bool {
        PanelId::parse(crate::studio_ui::VERTICAL_PANEL).is_ok_and(|p| self.workspace.is_open(&p))
    }

    /// Whether an item edit with `canvas=vertical` can run: the vertical canvas exists
    /// and shows the scene being edited (not in studio mode, ADR-0093 decision 7).
    fn vertical_edited(&self) -> Result<(), Disabled> {
        self.has_vertical()?;
        if self.studio.studio_mode {
            return Err("In studio mode the vertical canvas shows the scene on air.".into());
        }
        Ok(())
    }

    /// The canvas a projector command names.
    fn projector_canvas(id: &CommandId) -> Option<ProjectorCanvas> {
        crate::projector::parse_param(id.param("canvas")?)
    }

    /// The projector of `id`'s canvas is in the workspace (open, or waiting for its
    /// canvas).
    fn projector_kept(&self, id: &CommandId) -> bool {
        Self::projector_canvas(id).is_some_and(|c| {
            self.workspace
                .projectors()
                .iter()
                .any(|(_, p)| p.canvas == c)
        })
    }

    /// The window of the projector `id` names, while open.
    fn projector_window(&self, id: &CommandId) -> Result<&crate::projector::Shown, Disabled> {
        let canvas = Self::projector_canvas(id).ok_or(Disabled::from("Name a canvas."))?;
        self.projectors
            .iter()
            .find(|p| p.canvas == canvas)
            .ok_or_else(|| "The projector is not open.".into())
    }

    fn has_vertical(&self) -> Result<(), Disabled> {
        match self.studio.vertical {
            Some(_) => Ok(()),
            None => Err("Add a vertical output first.".into()),
        }
    }
}

/// What a panel's menu needs to know about the tab stack that holds it.
struct PanelCaps {
    floating: bool,
    collapsed: bool,
    can_collapse: bool,
    can_hide: bool,
    stacks: usize,
}

impl Shown<'_> {
    /// The panel's stack as the window view draws it, so the menu offers what the
    /// stack's header does (the same rules, ADR-0049 and ADR-0064).
    fn panel_caps(&self, id: &CommandId) -> Option<PanelCaps> {
        use slint::Model;
        let panel = panel_of(id)?;
        let (w, ..) = self.workspace.find(&panel)?;
        let win = self.workspace.windows.get(w)?;
        // Any area large enough to lay the window out: only the stack's flags are read.
        let area = rbs_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        let (stacks, ..) =
            crate::view::window_view(w, win, area, self.maximized, self.studio.studio_mode);
        let stack = stacks
            .iter()
            .find(|s| s.tabs.iter().any(|t| t.id.as_str() == panel.as_str()))?;
        Some(PanelCaps {
            floating: !win.is_main(),
            collapsed: stack.collapsed,
            can_collapse: stack.can_collapse,
            can_hide: stack.can_hide,
            stacks: win.stack_count(),
        })
    }
}

mod arrange;
mod group_menu;
pub use group_menu::GroupOp;

impl CommandState for Shown<'_> {
    fn enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        let s = self.studio;
        // Scene, source and item ids are unique only within a scene collection.
        if let Some(c) = id.param("collection")
            && s.current_collection().as_deref() != Some(c)
        {
            return Err("In another scene collection.".into());
        }
        match id.base() {
            "edit.undo" => {
                self.running()?;
                s.next_undo
                    .as_ref()
                    .map(|_| ())
                    .ok_or_else(|| "Nothing to undo.".into())
            }
            "edit.redo" => {
                self.running()?;
                s.next_redo
                    .as_ref()
                    .map(|_| ())
                    .ok_or_else(|| "Nothing to redo.".into())
            }
            "ui.area.toggle" => match side(id) {
                Some(sd) if self.areas().any(|(a, has, _)| a == sd && has) => Ok(()),
                _ => Err("This area has no panels.".into()),
            },
            "ui.area.hide_all" | "ui.area.show_all" => {
                if self.areas().any(|(_, has, _)| has) {
                    Ok(())
                } else {
                    Err("No area beside the centre has panels.".into())
                }
            }
            "source.add" => {
                self.running()?;
                if id.param("kind") == Some(EXISTING) && !self.studio.has_existing_sources() {
                    return Err("No other scene has a source to add.".into());
                }
                Ok(())
            }
            "canvas.studio_mode.toggle"
            | "collection.direct_transition"
            | "scene.add"
            | "canvas.add_vertical" => self.running(),
            // An open panel can always be closed.
            "canvas.vertical_preview.toggle" if self.vertical_panel_open() => Ok(()),
            "canvas.vertical_preview.toggle" => {
                self.running()?;
                self.has_vertical()
            }
            "scene.duplicate"
            | "scene.rename"
            | "scene.audio.own_levels.toggle"
            | "ui.scene.transition" => {
                self.running()?;
                self.scene(id).map(|_| ())
            }
            "scene.audio.reset_source" => {
                self.running()?;
                let scene = self.scene(id)?;
                let source = heard_source(id).ok_or("No source is named.")?;
                if self.studio.has_scene_level(scene, source) {
                    Ok(())
                } else {
                    Err("The scene uses the source's own level.".into())
                }
            }
            "scene.remove" => {
                self.running()?;
                self.scene(id)?;
                if s.scene_count() > 1 {
                    Ok(())
                } else {
                    Err("The last scene cannot be removed.".into())
                }
            }
            "scene.move" => {
                self.running()?;
                let scene = self.scene(id)?;
                let (up, down) = s.scene_ends(scene).ok_or("The scene is gone.")?;
                match id.param("direction") {
                    Some("up") | Some("front") if up => Ok(()),
                    Some("down") | Some("back") if down => Ok(()),
                    Some("up") | Some("front") => Err("This scene is already the first.".into()),
                    Some("down") | Some("back") => Err("This scene is already the last.".into()),
                    _ => Err("Choose which way to move the scene.".into()),
                }
            }
            "source.active.toggle" => {
                self.running()?;
                self.source(id).map(|_| ())
            }
            "source.duplicate" | "source.copy_attributes" | "ui.source.reset" => {
                self.running()?;
                self.source(id).map(|_| ())
            }
            "ui.source.paste_attributes" => {
                self.running()?;
                self.source(id)?;
                match s.attributes_clipboard {
                    Some(_) => Ok(()),
                    None => Err("Copy a source's attributes first.".into()),
                }
            }
            "item.visible.toggle" | "source.rename" | "source.delete" | "item.remove" => {
                self.running()?;
                self.source(id).map(|_| ())
            }
            "item.move" => {
                self.running()?;
                let f = self.source(id)?;
                if !f.picture {
                    return Err("This source has no layer.".into());
                }
                if f.locked {
                    return Err("Unlock the source to change its layer order.".into());
                }
                match id.param("direction") {
                    Some("up") if f.can_up => Ok(()),
                    Some("down") if f.can_down => Ok(()),
                    Some("front") if f.can_up => Ok(()),
                    Some("back") if f.can_down => Ok(()),
                    Some("up") | Some("front") => {
                        Err("This source is already the top layer.".into())
                    }
                    Some("down") | Some("back") => {
                        Err("This source is already the bottom layer.".into())
                    }
                    _ => Err("Say whether to move the source forward or back.".into()),
                }
            }
            "filter.add" => {
                self.running()?;
                id.param("source").ok_or("No source is selected.")?;
                self.source(id)?;
                match id.param("type") {
                    Some(t)
                        if !filter_types(id.param("audio") == Some("1"))
                            .iter()
                            .any(|(_, i)| i == t) =>
                    {
                        Err("That filter type is not available for this source.".into())
                    }
                    _ => Ok(()),
                }
            }
            "source.reload" => {
                self.running()?;
                if self.source(id)?.reloadable {
                    Ok(())
                } else {
                    Err("This source has nothing to open again.".into())
                }
            }
            "source.settings" => {
                self.running()?;
                if self.source(id)?.has_settings {
                    Ok(())
                } else {
                    Err("This source has no settings window.".into())
                }
            }
            "ui.properties.show" => {
                self.running()?;
                let f = self.source(id)?;
                match id.param("tab") {
                    Some("filters") if f.picture => Ok(()),
                    Some("filters") => Err("This source has no picture filters.".into()),
                    Some("source") if f.reloadable => Ok(()),
                    Some("source") => Err("This source has no target to change.".into()),
                    Some("audio") if f.audio => Ok(()),
                    Some("audio") => Err("This source has no sound.".into()),
                    Some("transform") if f.picture => Ok(()),
                    Some("transform") => Err("This source has no picture.".into()),
                    _ => Err("Say which Properties tab to open.".into()),
                }
            }
            base if arrange::is_arrange(base) => self.arrange_enabled(id),
            "item.locked.toggle" => {
                self.running()?;
                if self.source(id)?.picture {
                    Ok(())
                } else {
                    Err("The source has no picture.".into())
                }
            }
            "source.muted.toggle" => {
                self.running()?;
                if self.source(id)?.audio {
                    Ok(())
                } else {
                    Err("The source has no sound in the Mixer.".into())
                }
            }
            "source.mixer_hidden.toggle" | "source.monitor" | "source.monitor_solo.toggle" => {
                self.running()?;
                if self.source(id)?.audio {
                    Ok(())
                } else {
                    Err("The source has no sound in the Mixer.".into())
                }
            }
            "loudness.shown.toggle" => self.running(),
            "loudness.reset" => {
                self.running()?;
                if s.loudness_shown() {
                    Ok(())
                } else {
                    Err("Loudness is not shown.".into())
                }
            }
            "mixer.hide_all" | "mixer.hide_muted" | "mixer.unhide_all" => {
                self.running()?;
                let (shown, muted, hidden) = s.mixer_counts();
                match id.base() {
                    "mixer.hide_all" if shown == 0 => Err("Every source is hidden.".into()),
                    "mixer.hide_muted" if muted == 0 => Err("No shown source is muted.".into()),
                    "mixer.unhide_all" if hidden == 0 => Err("No source is hidden.".into()),
                    _ => Ok(()),
                }
            }
            "canvas.transition" | "ui.transition.rehearse" => {
                self.running()?;
                if s.studio_mode {
                    Ok(())
                } else {
                    Err("Studio mode is off.".into())
                }
            }
            "scene.select" => self.running(),
            "scene.position" => {
                self.running()?;
                scene_position(id)
                    .and_then(|n| s.scene_at(n))
                    .map(|_| ())
                    .ok_or_else(|| "No scene is at this position.".into())
            }
            "scene.next" | "scene.previous" => {
                self.running()?;
                s.scene_step(1)
                    .map(|_| ())
                    .ok_or_else(|| "There is only one scene.".into())
            }
            "ui.panel.maximize" => match panel_of(id) {
                Some(p) if self.workspace.is_open(&p) => Ok(()),
                _ => Err("The panel is not open.".into()),
            },
            "ui.profile.new"
            | "ui.profile.duplicate"
            | "ui.profile.rename"
            | "ui.profile.delete"
            | "ui.profile.import"
            | "ui.profile.export"
            | "ui.collection.new"
            | "ui.collection.duplicate"
            | "ui.collection.rename"
            | "ui.collection.delete"
            | "ui.collection.import"
            | "ui.collection.export" => {
                let (kind, op) = entry_op(id.base()).ok_or("Unknown command.")?;
                self.studio
                    .entry_blocked(kind == EntryKind::Collection, op.name())
                    .map_err(Disabled::from)
            }
            "ui.layout.undo" if !self.can_undo => Err("There is no layout change to undo.".into()),
            "ui.layout.redo" if !self.can_redo => Err("There is no layout change to redo.".into()),
            "ui.panel.collapse" | "ui.panel.auto_hide" | "ui.panel.move" | "ui.panel.float"
            | "ui.panel.close" => {
                let Some(c) = self.panel_caps(id) else {
                    return Err("The panel is not open.".into());
                };
                match id.base() {
                    // The lock stops what rearranges; collapse and auto-hide still work.
                    "ui.panel.move" | "ui.panel.float" | "ui.panel.close"
                        if self.workspace.locked =>
                    {
                        Err("The layout is locked.".into())
                    }
                    "ui.panel.collapse" if !c.can_collapse => Err(
                        "Preview and Program, and the last open stack of the centre, stay open."
                            .into(),
                    ),
                    "ui.panel.auto_hide" if !c.can_hide => Err(
                        "Only a stack at the edge of the main window can hide, and not the \
                         only stack of the centre."
                            .into(),
                    ),
                    "ui.panel.move" if id.param("to") == Some("next_stack") && c.stacks < 2 => {
                        Err("This window has one tab stack.".into())
                    }
                    _ => Ok(()),
                }
            }
            "ui.layout.rail_size" => match (self.workspace.studio_rail(), id.param("px")) {
                (None, _) => Err("This workspace has no Preview and Program side by side in \
                                  the centre."
                    .into()),
                (Some(_), Some(px)) if px.parse::<u32>().is_ok() => Ok(()),
                _ => Err("The width is a whole number of pixels.".into()),
            },
            "ui.layout.studio_rail" => match self.workspace.studio_rail() {
                Some(_) => Ok(()),
                None => Err("This workspace has no Preview and Program side by side in \
                             the centre."
                    .into()),
            },
            "ui.workspace.reset" => match rbs_ui::builtin::by_name(&self.workspace.name) {
                None => Err("Only a built-in workspace has a default layout.".into()),
                Some(mut d) => {
                    d.name.clone_from(&self.workspace.name);
                    if d == *self.workspace {
                        Err("The workspace already has its default layout.".into())
                    } else {
                        Ok(())
                    }
                }
            },
            "ui.toolbar.reset" => match self.workspace.toolbar {
                Some(_) => Ok(()),
                None => Err("The toolbar already has its default items.".into()),
            },
            // Only Go live is refused while streaming is off, never End stream (ADR-0148 d. 8).
            "stream.toggle" if !s.streaming_shows() => Err(STREAMING_OFF.into()),
            "stream.main.toggle" if !s.stream_shows(OutputCanvas::Main) => {
                Err(STREAMING_OFF.into())
            }
            "stream.toggle" if s.go_live_is_vertical() => {
                CommandId::parse("stream.vertical.toggle")
                    .map_err(Disabled::from)
                    .and_then(|v| self.enabled(&v))
            }
            "stream.toggle" | "stream.main.toggle" => match s.st_state {
                st_state::LIVE | st_state::STARTING => Ok(()),
                st_state::ENDING => Err("The stream is ending.".into()),
                _ => {
                    self.running()?;
                    if s.rec_state != rec_state::OFF {
                        return Err("Stop the recording to go live.".into());
                    }
                    s.cannot_record.clone().map_or(Ok(()), |r| Err(r.into()))
                }
            },
            "vcam.toggle" => {
                let c = self.camera;
                if c.sending {
                    return Ok(());
                }
                if !c.available {
                    return Err("The virtual camera is not available on this computer.".into());
                }
                self.running()?;
                if !c.ready {
                    return Err("Add the virtual camera in Settings, Virtual camera.".into());
                }
                Ok(())
            }
            "replay.toggle" => self.running(),
            "stream.hold_toggle" => {
                self.running()?;
                if s.st_state == st_state::LIVE || matches!(s.vertical_st, VerticalOut::On) {
                    Ok(())
                } else {
                    Err("Hold works while a stream is live.".into())
                }
            }
            "output.break_toggle" => {
                self.running()?;
                let live = s.st_state == st_state::LIVE || matches!(s.vertical_st, VerticalOut::On);
                let recording = s.rec_state == rec_state::RECORDING
                    || matches!(s.vertical_rec, VerticalOut::On);
                if live || recording {
                    Ok(())
                } else {
                    Err("Break works while a stream is live or a recording runs.".into())
                }
            }
            "record.pause_toggle" => {
                self.running()?;
                let vertical = matches!(s.vertical_rec, VerticalOut::On);
                if s.rec_state == rec_state::RECORDING || vertical {
                    Ok(())
                } else {
                    Err("Pause works while a recording runs.".into())
                }
            }
            "marker.add" => {
                self.running()?;
                let vertical = matches!(s.vertical_rec, VerticalOut::On);
                if s.rec_state == rec_state::RECORDING || vertical {
                    Ok(())
                } else {
                    Err("Markers are added while a recording runs.".into())
                }
            }
            "canvas.auto_layout" => {
                self.running()?;
                self.has_vertical()
            }
            "ui.projector.open" => {
                if !cfg!(any(target_os = "macos", windows)) {
                    return Err("Projectors need macOS or Windows.".into());
                }
                if self.projector_kept(id) {
                    return Ok(());
                }
                self.running()?;
                match Self::projector_canvas(id) {
                    Some(ProjectorCanvas::Main) => Ok(()),
                    Some(ProjectorCanvas::Vertical) => self.has_vertical(),
                    None => Err("Name a canvas.".into()),
                }
            }
            "ui.projector.size" => self.projector_window(id).map(|_| ()),
            // Also one waiting for its canvas.
            "ui.projector.close" if self.projector_kept(id) => Ok(()),
            "ui.projector.close" => Err("The projector is not open.".into()),
            // A line that informs: never run.
            "ui.projector.shared" => {
                self.projector_window(id)?;
                Err("The size a video call receives from this window.".into())
            }
            "canvas.vertical_size" => {
                self.running()?;
                self.has_vertical()?;
                if size_of(id).is_none() {
                    return Err("Say the width and height in pixels.".into());
                }
                if s.vertical_st.busy() || s.vertical_rec.busy() {
                    return Err("Stop the vertical outputs first.".into());
                }
                Ok(())
            }
            "canvas.remove_vertical" => {
                self.running()?;
                self.has_vertical()?;
                if s.vertical_st.busy() || s.vertical_rec.busy() {
                    return Err("Stop the vertical outputs first.".into());
                }
                Ok(())
            }
            "replay.save" => {
                self.running()?;
                if self.replay.can_save {
                    Ok(())
                } else if self.replay.armed {
                    Err("A clip is being saved.".into())
                } else {
                    Err("Arm the replay buffer first.".into())
                }
            }
            "stream.vertical.toggle" => match s.vertical_st {
                VerticalOut::On | VerticalOut::Starting => Ok(()),
                VerticalOut::Ending => Err("The vertical stream is ending.".into()),
                VerticalOut::Off => {
                    self.running()?;
                    self.has_vertical()?;
                    if !s.stream_available(OutputCanvas::Vertical) {
                        return Err(STREAMING_OFF.into());
                    }
                    s.cannot_record.clone().map_or(Ok(()), |r| Err(r.into()))
                }
            },
            "record.vertical.toggle" => match s.vertical_rec {
                VerticalOut::On => Ok(()),
                VerticalOut::Starting => Err("The vertical recording is starting.".into()),
                VerticalOut::Ending => Err("The vertical recording is stopping.".into()),
                VerticalOut::Off => {
                    self.running()?;
                    self.has_vertical()?;
                    s.recording_blocked(true).map_or(Ok(()), |r| Err(r.into()))
                }
            },
            "record.toggle" | "record.main.toggle" => match s.rec_state {
                rec_state::RECORDING => Ok(()),
                rec_state::STARTING => Err("The recording is starting.".into()),
                rec_state::STOPPING => Err("The recording is stopping.".into()),
                _ => {
                    self.running()?;
                    if s.st_state != st_state::OFF {
                        return Err("End the stream to record.".into());
                    }
                    s.recording_blocked(false).map_or(Ok(()), |r| Err(r.into()))
                }
            },
            _ => Ok(()),
        }
    }

    fn checked(&self, id: &CommandId) -> bool {
        match id.base() {
            "ui.area.toggle" => {
                side(id).is_some_and(|sd| self.areas().any(|(a, has, v)| a == sd && has && v))
            }
            "ui.panel.maximize" => panel_of(id).is_some_and(|p| self.maximized == Some(&p)),
            "ui.panel.collapse" => self.panel_caps(id).is_some_and(|c| c.collapsed),
            "ui.layout.lock" => self.workspace.locked,
            "ui.panel.float" => self.panel_caps(id).is_some_and(|c| c.floating),
            "ui.layout.studio_rail" => {
                self.workspace.studio_rail().is_some()
                    && self.workspace.studio_rail() == rail_at(id)
            }
            "canvas.studio_mode.toggle" => self.studio.studio_mode,
            "canvas.vertical_preview.toggle" => self.vertical_panel_open(),
            "ui.projector.open" => self.projector_kept(id),
            "canvas.vertical_size" => match (self.studio.vertical, size_of(id)) {
                (Some(v), Some((w, h))) => v.size.width == w && v.size.height == h,
                _ => false,
            },
            "item.flip" => self.source(id).is_ok_and(|f| match id.param("axis") {
                Some("horizontal") => f.flip.horizontal,
                Some("vertical") => f.flip.vertical,
                _ => false,
            }),
            "ui.group.visible.toggle" | "ui.group.locked.toggle" => self.group_checked(id),
            "item.visible.toggle" => self.source(id).is_ok_and(|f| f.visible),
            "source.active.toggle" => self.source(id).is_ok_and(|f| f.active),
            "item.locked.toggle" => self.source(id).is_ok_and(|f| f.locked),
            "source.muted.toggle" => self.source(id).is_ok_and(|f| f.muted),
            "scene.audio.own_levels.toggle" => self
                .scene(id)
                .is_ok_and(|s| self.studio.scene_own_levels(s)),
            "source.mixer_hidden.toggle" => self.source(id).is_ok_and(|f| f.mixer_hidden),
            "source.monitor_solo.toggle" => self.source(id).is_ok_and(|f| f.soloed),
            "loudness.shown.toggle" => self.studio.loudness_shown(),
            // The resolved toggles show the main canvas's state (ADR-0111 decision 5).
            // With only the vertical canvas showing stream items, Go live is its toggle
            // (ADR-0148 decision 8).
            "stream.toggle" if self.studio.go_live_is_vertical() => self.studio.vertical_st.on(),
            "stream.toggle" | "stream.main.toggle" => {
                matches!(self.studio.st_state, st_state::LIVE | st_state::STARTING)
            }
            "vcam.toggle" => self.camera.sending,
            "replay.toggle" => self.replay.armed,
            "stream.vertical.toggle" => self.studio.vertical_st.on(),
            "record.vertical.toggle" => self.studio.vertical_rec.on(),
            "record.toggle" | "record.main.toggle" => matches!(
                self.studio.rec_state,
                rec_state::RECORDING | rec_state::STARTING
            ),
            _ => false,
        }
    }

    fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
        match id.base() {
            "ui.workspace.select" => self
                .workspaces
                .iter()
                .map(|(n, _)| {
                    let current = rbs_ui::workspace::same_name(n, &self.workspace.name);
                    (n.clone(), n.clone(), current)
                })
                .collect(),
            "ui.panel.select" => self.panels.clone(),
            // A locked layout offers no place to move to: the submenu is disabled.
            "ui.panel.move" if self.workspace.locked => Vec::new(),
            "ui.panel.move" => {
                let stacks = self.panel_caps(id).map_or(0, |c| c.stacks);
                let mut out: Vec<_> = [
                    ("Left edge", "left"),
                    ("Right edge", "right"),
                    ("Top edge", "top"),
                    ("Bottom edge", "bottom"),
                ]
                .into_iter()
                .map(|(t, v)| (t.to_owned(), v.to_owned(), false))
                .collect();
                if stacks > 1 {
                    out.push(("Next tab stack".to_owned(), "next_stack".to_owned(), false));
                }
                out
            }
            "source.monitor" => {
                let Ok(f) = self.source(id) else {
                    return Vec::new();
                };
                [
                    (SourceMonitor::Off, "Monitor off"),
                    (SourceMonitor::Also, "Monitor and output"),
                    (SourceMonitor::Only, "Monitor only"),
                ]
                .into_iter()
                .map(|(m, title)| (title.to_owned(), m.as_str().to_owned(), m == f.monitor))
                .collect()
            }
            "ui.mixer.orientation" => {
                let now = self.workspace.mixer_orientation();
                [
                    (MixerOrientation::Auto, "Automatic"),
                    (MixerOrientation::Rows, "Rows"),
                    (MixerOrientation::Columns, "Columns"),
                ]
                .into_iter()
                .map(|(o, title)| (title.to_owned(), o.name().to_owned(), o == now))
                .collect()
            }
            "profile.select" => self.studio.library_choices(false),
            "collection.select" => self.studio.library_choices(true),
            "collection.direct_transition" => {
                let (t, glide_ms) = self.studio.direct_transition();
                direct_choices(t, glide_ms)
            }
            "ui.theme.select" => self.themes.clone(),
            "ui.experience.select" => {
                use crate::background::Experience;
                [
                    (Experience::Simple, "Simple"),
                    (Experience::Standard, "Standard"),
                    (Experience::Pro, "Pro"),
                ]
                .into_iter()
                .map(|(e, title)| (title.to_owned(), e.id().to_owned(), e == self.experience))
                .collect()
            }
            "ui.density.select" => {
                use rbs_ui::tokens::Density;
                [
                    (Density::Regular, "Regular"),
                    (Density::Compact, "Compact"),
                    (Density::Roomy, "Roomy"),
                ]
                .into_iter()
                .map(|(d, title)| {
                    (
                        title.to_owned(),
                        density_id(d).to_owned(),
                        d == self.density,
                    )
                })
                .collect()
            }
            "output.with_vertical" => {
                let Some(kind) = output_kind(id) else {
                    return Vec::new();
                };
                let on = self.studio.vertical_flags.with_main(kind);
                // The hint of ADR-0111 decision 6: Go live skips a vertical stream that
                // has no server.
                let both = if kind == OutputKind::Stream && !self.studio.vertical_stream_set_up() {
                    "Main + vertical (vertical stream not set up)"
                } else {
                    "Main + vertical"
                };
                vec![
                    (both.to_owned(), "on".to_owned(), on),
                    ("Main only".to_owned(), "off".to_owned(), !on),
                ]
            }
            "scene.select" => self.studio.scene_choices(),
            "ui.projector.size" => {
                let Ok(p) = self.projector_window(id) else {
                    return Vec::new();
                };
                let window = p.fullscreen.is_none();
                let mut out = vec![
                    (
                        "Window".to_owned(),
                        "window".to_owned(),
                        window && p.size == ProjectorSize::Window,
                    ),
                    (
                        "Exact pixels".to_owned(),
                        "exact".to_owned(),
                        window && p.size == ProjectorSize::Exact,
                    ),
                ];
                out.extend(self.displays.iter().map(|d| {
                    (
                        format!("Full screen on {d}"),
                        format!("{FULL_SCREEN_ON}{d}"),
                        p.fullscreen.as_deref() == Some(d),
                    )
                }));
                out
            }
            "canvas.auto_layout" => vec![
                ("Fit each source".into(), "fit".into(), false),
                ("Fill with each source".into(), "fill".into(), false),
                ("Stack top to bottom".into(), "stack".into(), false),
            ],
            "item.align_items" => vec![
                ("Align left".into(), "left".into(), false),
                ("Align horizontal centres".into(), "center".into(), false),
                ("Align right".into(), "right".into(), false),
                ("Align top".into(), "top".into(), false),
                ("Align vertical centres".into(), "middle".into(), false),
                ("Align bottom".into(), "bottom".into(), false),
            ],
            "item.distribute" => vec![
                ("Distribute horizontally".into(), "horizontal".into(), false),
                ("Distribute vertically".into(), "vertical".into(), false),
            ],
            "item.rotate" => vec![
                ("Rotate 90° clockwise".into(), "90".into(), false),
                ("Rotate 90° counter-clockwise".into(), "-90".into(), false),
            ],
            "item.fit" => vec![
                ("Fit to canvas".into(), "fit".into(), false),
                ("Fill canvas".into(), "fill".into(), false),
                ("Stretch to canvas".into(), "stretch".into(), false),
            ],
            "item.align" => vec![
                ("Center on canvas".into(), "both".into(), false),
                ("Center horizontally".into(), "horizontal".into(), false),
                ("Center vertically".into(), "vertical".into(), false),
            ],
            "filter.add" => filter_types(id.param("audio") == Some("1"))
                .into_iter()
                .map(|(name, type_id)| (name, type_id, false))
                .collect(),
            "source.add" => {
                let mut list: Vec<(String, String, bool)> = self
                    .studio
                    .add_source_kinds()
                    .into_iter()
                    .map(|(_, api_id, label)| (label.to_owned(), api_id.to_owned(), false))
                    .collect();
                if self.studio.has_existing_sources() {
                    list.push(("Existing source".to_owned(), EXISTING.to_owned(), false));
                }
                list
            }
            _ => Vec::new(),
        }
    }

    fn context(&self, id: &CommandId) -> Vec<(&'static str, String)> {
        match (id.base(), self.studio.current_collection()) {
            ("scene.select" | "filter.add", Some(c)) => vec![("collection", c)],
            _ => Vec::new(),
        }
    }

    fn title(&self, id: &CommandId) -> Option<String> {
        if id.base() == "record.toggle" && self.studio.record_button_audio() {
            let running = self.studio.rec_state != rec_state::OFF;
            return Some(
                if running {
                    "Stop audio recording"
                } else {
                    "Record audio"
                }
                .to_owned(),
            );
        }
        if id.base() == "stream.hold_toggle" && self.studio.stream_held() {
            return Some("Resume stream".to_owned());
        }
        if id.base() == "record.pause_toggle" && self.studio.record_paused() {
            return Some("Resume recording".to_owned());
        }
        if id.base() == "canvas.vertical_size"
            && let Some((w, h)) = size_of(id)
        {
            return Some(format!("{w} x {h}"));
        }
        if id.base().starts_with("ui.projector.")
            && let Some(canvas) = Self::projector_canvas(id)
        {
            let name = match canvas {
                ProjectorCanvas::Main => "Program",
                ProjectorCanvas::Vertical => "Vertical",
            };
            match id.base() {
                "ui.projector.open" => return Some(format!("Open projector: {name}")),
                "ui.projector.close" => return Some(format!("Close projector: {name}")),
                "ui.projector.size" if id.param("size").is_none() => {
                    return Some(format!("{name} projector size"));
                }
                "ui.projector.shared" => {
                    return self.projector_window(id).ok().map(|p| p.shared.clone());
                }
                _ => {}
            }
        }
        if id.base() == "ui.layout.lock" {
            return Some(
                if self.workspace.locked {
                    "Unlock layout"
                } else {
                    "Lock layout"
                }
                .to_owned(),
            );
        }
        if let Some(p) = panel_of(id) {
            let name = crate::view::title(&p);
            match id.base() {
                "ui.panel.collapse" => {
                    let collapsed = self.panel_caps(id).is_some_and(|c| c.collapsed);
                    return Some(if collapsed { "Expand" } else { "Collapse" }.to_owned());
                }
                "ui.panel.float" => {
                    let floating = self.panel_caps(id).is_some_and(|c| c.floating);
                    return Some(
                        if floating {
                            "Dock back"
                        } else {
                            "Float in its own window"
                        }
                        .to_owned(),
                    );
                }
                "ui.panel.maximize" => {
                    return Some(if self.maximized == Some(&p) {
                        "Restore layout".to_owned()
                    } else {
                        format!("Maximize {name}")
                    });
                }
                // The vertical picture's panel: the registry's "Hide vertical preview"
                // says that the vertical output stays.
                "ui.panel.close" if id.param("id") == Some(crate::studio_ui::VERTICAL_PANEL) => {
                    return None;
                }
                "ui.panel.close" => return Some(format!("Close {name}")),
                _ => {}
            }
        }
        match id.param("axis") {
            Some("horizontal") if id.base() == "item.flip" => {
                return Some("Flip horizontally".to_owned());
            }
            Some("vertical") if id.base() == "item.flip" => {
                return Some("Flip vertically".to_owned());
            }
            _ => {}
        }
        match id.base() {
            "canvas.add_vertical" if self.studio.vertical.is_some() => {
                return Some("Vertical output settings...".to_owned());
            }
            // In the ▾ menu: where the vertical stream is set up.
            "ui.settings.open"
                if id.param("section") == Some("stream") && self.studio.vertical.is_some() =>
            {
                return Some(
                    if self.studio.vertical_stream_set_up() {
                        "Stream settings..."
                    } else {
                        "Set up vertical stream..."
                    }
                    .to_owned(),
                );
            }
            "stream.vertical.toggle" if self.studio.vertical_st.on() => {
                return Some("End vertical stream".to_owned());
            }
            "record.vertical.toggle" if self.studio.vertical_rec.on() => {
                return Some("Stop vertical recording".to_owned());
            }
            _ => {}
        }
        match (id.base(), id.param("scene")) {
            // One scene, from its row's menu: what a click on the row does.
            ("scene.select", Some(_)) if self.studio.studio_mode => {
                Some("Show in preview".to_owned())
            }
            ("scene.select", Some(_)) => Some("Put on air".to_owned()),
            ("scene.rename", Some(_)) => Some("Rename".to_owned()),
            ("scene.remove", Some(_)) => Some("Remove".to_owned()),
            ("source.rename", _) if id.param("source").is_some() => Some("Rename".to_owned()),
            ("source.reload", _) if id.param("source").is_some() => Some("Reload".to_owned()),
            ("source.delete", _) if id.param("source").is_some() => {
                Some("Delete everywhere".to_owned())
            }
            ("item.remove", _) if id.param("source").is_some() => {
                Some("Remove from this scene".to_owned())
            }
            _ => None,
        }
    }

    fn applies(&self, id: &CommandId) -> bool {
        // Type-specific commands leave the menu when the selected source cannot use
        // them (a mic has no Transform). With no source selected they still apply so
        // the menu bar can show the entry disabled ("No source is selected.").
        match id.base() {
            "item.locked.toggle"
            | "item.flip"
            | "item.rotate"
            | "item.fit"
            | "item.align"
            | "item.reset_transform"
            | "item.move" => match self.source(id) {
                Ok(f) => f.picture,
                Err(_) => true,
            },
            "source.settings" => match self.source(id) {
                Ok(f) => f.has_settings,
                Err(_) => true,
            },
            "source.reload" => match self.source(id) {
                Ok(f) => f.reloadable,
                Err(_) => true,
            },
            "ui.properties.show" => match (id.param("tab"), self.source(id)) {
                (Some("filters") | Some("transform"), Ok(f)) => f.picture,
                (Some("source"), Ok(f)) => f.reloadable,
                (Some("audio"), Ok(f)) => f.audio,
                (Some("filters" | "transform" | "source" | "audio"), Err(_)) => true,
                _ => false,
            },
            "source.muted.toggle"
            | "source.mixer_hidden.toggle"
            | "source.monitor"
            | "source.monitor_solo.toggle" => match self.source(id) {
                Ok(f) => f.audio,
                Err(_) => true,
            },
            // Only while the program scene has its own level for the source (the
            // Mixer strip's menu, ADR-0091).
            "scene.audio.reset_source" => match (self.scene(id), heard_source(id)) {
                (Ok(scene), Some(source)) => self.studio.has_scene_level(scene, source),
                _ => false,
            },
            // Stream items while their canvas shows them (ADR-0148 decision 8): the
            // frame's Go live and Hold while either canvas does.
            "stream.toggle" | "stream.hold_toggle" => self.studio.streaming_shows(),
            "stream.main.toggle" if !self.studio.stream_shows(OutputCanvas::Main) => false,
            "stream.vertical.toggle" if !self.studio.stream_shows(OutputCanvas::Vertical) => false,
            "output.with_vertical"
                if id.param("kind") == Some("stream")
                    && !(self.studio.stream_shows(OutputCanvas::Main)
                        && self.studio.stream_shows(OutputCanvas::Vertical)) =>
            {
                false
            }
            // A vertical canvas's outputs, while it exists (or one of them still runs).
            "stream.vertical.toggle"
            | "record.vertical.toggle"
            | "stream.main.toggle"
            | "record.main.toggle"
            | "output.with_vertical" => {
                self.studio.vertical.is_some()
                    || self.studio.vertical_st.busy()
                    || self.studio.vertical_rec.busy()
            }
            "canvas.remove_vertical" | "canvas.auto_layout" | "canvas.vertical_size" => {
                self.studio.vertical.is_some()
            }
            // Stream settings in the ▾ menu (the only layout that names this id), while
            // a vertical canvas exists.
            "ui.settings.open" if id.param("section") == Some("stream") => {
                self.studio.vertical.is_some()
            }
            // The vertical picture's toggle, while a vertical canvas exists (or its panel
            // is still open).
            "canvas.vertical_preview.toggle" => {
                self.studio.vertical.is_some() || self.vertical_panel_open()
            }
            // The scene shown already: putting it on air again does nothing.
            "scene.select" => {
                id.param("scene").is_none() || self.scene(id).ok() != self.studio.scene_target(None)
            }
            _ => true,
        }
    }
}

/// What `ui.settings.open` runs (spec `settings-search.md`): Settings at the id's
/// section, if it names one, and at its `setting` (a settings index key shown in that
/// section; without a section, the key's first one) or its `command` (a registry id
/// without parameters, in Shortcuts; implies that section). An unknown section, key or
/// command, a key the section does not show, both `setting` and `command`, or
/// `command` with another section, runs nothing.
fn open_settings(id: &CommandId) -> Option<Run> {
    let section = id.param("section");
    match (id.param("setting"), id.param("command")) {
        (None, None) => {
            let shown = match section {
                Some(name) => Some(settings_section(name)?),
                None => None,
            };
            Some(Run::OpenSettings(shown, None))
        }
        (Some(key), None) => {
            let place = rbs_ui::settings_index::place(key, section)?;
            Some(Run::OpenSettings(
                Some(settings_section(place.section)?),
                Some(SettingsTarget::Setting(key.to_owned())),
            ))
        }
        (None, Some(command)) => {
            if section.is_some_and(|s| s != "shortcuts") {
                return None;
            }
            let parsed = CommandId::parse(command).ok()?;
            if parsed.to_string() != parsed.base() {
                return None;
            }
            rbs_ui::command::entry(parsed.base())?;
            Some(Run::OpenSettings(
                Some(settings_section("shortcuts")?),
                Some(SettingsTarget::Command(command.to_owned())),
            ))
        }
        (Some(_), Some(_)) => None,
    }
}

/// What `ui.settings.open` shows inside its section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsTarget {
    /// A settings index key (a card id or a row key).
    Setting(String),
    /// A registry command's row in Shortcuts.
    Command(String),
}

/// The `SettingsWindow.section` number of a `ui.settings.open` section name.
pub fn settings_section(name: &str) -> Option<i32> {
    Some(match name {
        "video" => 0,
        "recording" => 1,
        "stream" => 2,
        "remote_control" => 3,
        "general" => 4,
        "appearance" => 5,
        "shortcuts" => 6,
        "global_sources" => 7,
        "virtual_camera" => 8,
        "audio" => 9,
        "ai_tools" => 10,
        "permissions" => 11,
        "add_ons" => 12,
        "advanced" => 13,
        _ => return None,
    })
}

fn scene_position(id: &CommandId) -> Option<usize> {
    id.param("n")?.parse().ok()
}

/// One toolbar item as the frame draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolItem {
    /// The registry id it runs.
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub enabled: bool,
    /// A toggle or area toggle that is on.
    pub checked: bool,
    /// It stops a live output now (a normal button with a `stop` icon in the live
    /// colour, U12).
    pub live: bool,
    /// Icon name from the design system's icon set, or empty.
    pub icon: String,
    /// Drawn as its icon alone (area toggles, and every item in the icons-only style);
    /// the title stays its accessible name and tooltip.
    pub icon_only: bool,
}

/// What a toolbar item is (`ToolbarData.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Command = 0,
    WorkspacePicker = 1,
    Separator = 2,
    Space = 3,
    /// An id this version has no entry or toolbar control for: a disabled placeholder.
    Unknown = 4,
}

/// The toolbar row's two groups (ADR-0071): the items before the flexible space and
/// the items after it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub lead: Vec<ToolItem>,
    pub end: Vec<ToolItem>,
    /// Go live and Record: the frame's fixed group (ADR-0073 decision 8), drawn at the
    /// row's end whether or not the toolbar is shown, never customisable.
    pub fixed: Vec<ToolItem>,
}

/// The commands of the frame's fixed group; a toolbar never draws them itself.
pub const FIXED_COMMANDS: [&str; 2] = ["stream.toggle", "record.toggle"];

/// A command that belongs to streaming: it leaves the menus while no canvas shows
/// stream items, and Shortcuts dims it (ADR-0148 decision 8).
pub fn is_stream_item(command: &str) -> bool {
    command.starts_with("stream.") || command == "output.with_vertical?kind=stream"
}

/// A toolbar item that is not drawn while its command does not apply: a canvas's own
/// toggle without the canvas, a stream item while streaming is off (ADR-0148).
fn hidden_with_its_feature(command: &str) -> bool {
    command == "canvas.vertical_preview.toggle" || is_stream_item(command)
}

/// Splits items at the first flexible space (later ones are dropped) and drops the
/// separators that would lead, end or double within a group. Without a space, every
/// item leads.
pub fn split_row(items: Vec<ToolItem>) -> Row {
    let mut row = Row::default();
    let mut after = false;
    for item in items {
        if item.kind == ToolKind::Space {
            after = true;
            continue;
        }
        let group = if after { &mut row.end } else { &mut row.lead };
        let sep = item.kind == ToolKind::Separator;
        let last_sep = group.last().is_none_or(|l| l.kind == ToolKind::Separator);
        if sep && last_sep {
            continue;
        }
        group.push(item);
    }
    for group in [&mut row.lead, &mut row.end] {
        if group.last().is_some_and(|l| l.kind == ToolKind::Separator) {
            group.pop();
        }
    }
    row
}

/// The workspace's toolbar (its own or the default one), split into the row's groups,
/// and whether it is shown.
pub fn toolbar(state: &Shown<'_>) -> (Row, bool) {
    use rbs_ui::workspace::{Toolbar, ToolbarItem, ToolbarStyle};
    let default;
    let bar = match &state.workspace.toolbar {
        Some(t) => t,
        None => {
            default = rbs_ui::command::default_toolbar();
            &default
        }
    };
    let mut row = split_row(build_items(state, bar));
    let fixed = Toolbar {
        style: ToolbarStyle::IconsText,
        visible: true,
        items: FIXED_COMMANDS
            .iter()
            .map(|c| ToolbarItem::Command((*c).to_owned()))
            .collect(),
    };
    row.fixed = build_items(state, &fixed);
    (row, bar.visible)
}

/// The items of `bar` as toolbar data, without the frame's fixed commands when `bar`
/// is a workspace's toolbar (a saved toolbar from before ADR-0073 may still list them).
fn build_items(state: &Shown<'_>, bar: &rbs_ui::workspace::Toolbar) -> Vec<ToolItem> {
    use rbs_ui::workspace::{ToolbarItem, ToolbarStyle};
    let is_fixed_bar = bar.items.len() == FIXED_COMMANDS.len()
        && bar
            .items
            .iter()
            .all(|i| matches!(i, ToolbarItem::Command(c) if FIXED_COMMANDS.contains(&c.as_str())));
    let blank = |kind| ToolItem {
        id: String::new(),
        title: String::new(),
        kind,
        enabled: false,
        checked: false,
        live: false,
        icon: String::new(),
        icon_only: false,
    };
    bar.items
        .iter()
        .filter(|item| {
            is_fixed_bar
                || !matches!(item, ToolbarItem::Command(c) if FIXED_COMMANDS.contains(&c.as_str()))
        })
        // A canvas's own toggle shows only while that canvas exists, and a stream item
        // only while its canvas shows stream items (ADR-0148 decision 8: a placed item
        // stays saved but is not drawn).
        .filter(|item| match item {
            ToolbarItem::Command(c) if hidden_with_its_feature(c) => {
                CommandId::parse(c).is_ok_and(|id| state.applies(&id))
            }
            _ => true,
        })
        .map(|item| match item {
            ToolbarItem::Separator => blank(ToolKind::Separator),
            ToolbarItem::Space => blank(ToolKind::Space),
            ToolbarItem::Command(text) => {
                let unknown = ToolItem {
                    id: text.clone(),
                    title: text.clone(),
                    ..blank(ToolKind::Unknown)
                };
                let Ok(id) = CommandId::parse(text) else {
                    return unknown;
                };
                let Ok(Some(entry)) = id.resolve() else {
                    return unknown;
                };
                let title = rbs_ui::command::title(&id, entry);
                if let Kind::Picker { .. } = entry.kind {
                    return match id.base() {
                        "ui.workspace.select" => ToolItem {
                            title,
                            enabled: true,
                            ..blank(ToolKind::WorkspacePicker)
                        },
                        // Opens the Add source menu: a button, like a command.
                        "source.add" => ToolItem {
                            id: id.to_string(),
                            title,
                            kind: ToolKind::Command,
                            enabled: state.enabled(&id).is_ok(),
                            icon: rbs_ui::command::icon(&id, entry)
                                .unwrap_or_default()
                                .to_owned(),
                            icon_only: bar.style == ToolbarStyle::Icons,
                            ..blank(ToolKind::Command)
                        },
                        _ => ToolItem { title, ..unknown },
                    };
                }
                let on = entry.checkable && state.checked(&id);
                // A running output's toggle says what it does now (as Live controls do).
                let audio = state.studio.record_button_audio();
                let (title, live) = match (id.base(), on) {
                    ("stream.toggle", true) => ("End stream".to_owned(), true),
                    ("record.toggle", true) if audio => ("Stop audio recording".to_owned(), true),
                    ("record.toggle", true) => ("Stop recording".to_owned(), true),
                    ("record.toggle", false) if audio => ("Record audio".to_owned(), false),
                    ("vcam.toggle", true) => ("Stop virtual camera".to_owned(), true),
                    ("replay.toggle", true) => ("Disarm replay buffer".to_owned(), false),
                    _ => (title, false),
                };
                // A running output's toggle shows a stop square in the live colour; the
                // button itself stays a normal one (U12: calm, red only as a mark).
                let icon = if live {
                    "stop"
                } else {
                    rbs_ui::command::icon(&id, entry).unwrap_or_default()
                };
                let icon_only = !icon.is_empty()
                    && (bar.style == ToolbarStyle::Icons || rbs_ui::command::icon_only(&id));
                ToolItem {
                    id: id.to_string(),
                    title,
                    kind: ToolKind::Command,
                    enabled: state.enabled(&id).is_ok(),
                    checked: on,
                    live,
                    icon: icon.to_owned(),
                    icon_only,
                }
            }
        })
        .collect()
}

/// The Mixer's bulk commands (ADR-0080).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixerBulk {
    /// Hide every shown source.
    HideAll,
    /// Hide the shown sources that are muted.
    HideMuted,
    /// Show every hidden source.
    ShowAll,
}

/// The two libraries the status bar's pickers switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Profile,
    Collection,
}

/// What an Output profile or Scene collection command does to the one in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryOp {
    New,
    Duplicate,
    Rename,
    Delete,
    Import,
    Export,
}

impl EntryOp {
    /// The word the registry ids end in.
    pub fn name(self) -> &'static str {
        match self {
            EntryOp::New => "new",
            EntryOp::Duplicate => "duplicate",
            EntryOp::Rename => "rename",
            EntryOp::Delete => "delete",
            EntryOp::Import => "import",
            EntryOp::Export => "export",
        }
    }
}

/// The library and operation of a `ui.profile.*` or `ui.collection.*` id.
fn entry_op(base: &str) -> Option<(EntryKind, EntryOp)> {
    let (kind, op) = if let Some(op) = base.strip_prefix("ui.profile.") {
        (EntryKind::Profile, op)
    } else {
        (EntryKind::Collection, base.strip_prefix("ui.collection.")?)
    };
    let op = [
        EntryOp::New,
        EntryOp::Duplicate,
        EntryOp::Rename,
        EntryOp::Delete,
        EntryOp::Import,
        EntryOp::Export,
    ]
    .into_iter()
    .find(|o| o.name() == op)?;
    Some((kind, op))
}

/// What running an id from the menu bar does.
#[derive(Clone, Debug, PartialEq)]
pub enum Run {
    /// Add the filter type (its id) to the source (`filter.add`).
    AddFilter(i32, String),
    /// An Output profile or Scene collection command (`ui.profile.new`, ...). New,
    /// Duplicate and Rename take a name; without one the window asks for it.
    Entry(EntryKind, EntryOp, Option<String>),
    /// Read the token, appearance and theme files again (`ui.theme.reload`).
    ReloadThemes,
    /// Use this theme id in Fixed mode (`ui.theme.select`).
    SelectTheme(String),
    /// Draw the sections of this level (`ui.experience.select`, ADR-0113).
    SelectExperience(crate::background::Experience),
    /// Use this density (`ui.density.select`, ADR-0113 decision 2).
    SelectDensity(rbs_ui::tokens::Density),
    /// Show the Settings window, at this section (`SettingsWindow.section`) when
    /// given, and at this setting or Shortcuts command (`ui.settings.open`).
    OpenSettings(Option<i32>, Option<SettingsTarget>),
    /// Open the RBS menu with its search field (`ui.app_menu.open`).
    OpenAppMenu,
    /// Open the Import from OBS window (`ui.collection.import_obs`, ADR-0131).
    OpenObsImport,
    /// Scene edit undo (`true`) or redo.
    UndoEdit(bool),
    /// Load and switch to the named workspace.
    Workspace(String),
    /// Open (or show) a panel.
    Panel(String),
    /// Use the output profile with this id.
    Profile(String),
    /// Run the scene collection with this id.
    Collection(String),
    /// The kind a scene switch uses outside studio mode (ADR-0153).
    DirectTransition(rbs_scene::TransitionKind),
    /// Put the scene with this id on air.
    Scene(i32),
    /// A layout change.
    Layout(rbs_ui::LayoutCommand),
    /// Maximize the panel (`true`) or restore the layout.
    Maximize(PanelId, bool),
    /// Write the shown workspace to its file now (`ui.workspace.save`).
    SaveWorkspace,
    /// Save the shown workspace as a new one named so and switch to it
    /// (`ui.workspace.save_as`); without a name the window asks for one.
    SaveWorkspaceAs(Option<String>),
    /// Undo (`true`) or redo (`false`) a layout change.
    LayoutHistory(bool),
    /// Auto-hide the panel's tab stack to the edge (`ui.panel.auto_hide`).
    AutoHide(PanelId),
    /// Move the panel to an edge of its window or to the next tab stack
    /// (`ui.panel.move`, `to`: `left`, `right`, `top`, `bottom`, `next_stack`).
    MovePanel(PanelId, String),
    /// Open the device picker of a source type, or of the sources other scenes have.
    AddSource(AddWhat),
    /// Turn studio mode on (`true`) or off (ADR-0061).
    StudioMode(bool),
    /// Put the preview scene on air (ADR-0061).
    Transition,
    /// Rehearse on Preview (ADR-0135 decision 11).
    Rehearse,
    /// Add a scene.
    AddScene,
    /// Copy a scene with the same sources and layouts.
    DuplicateScene(i32),
    /// Duplicate scene with new sources (ADR-0133 decision 4).
    DuplicateSceneWithCopies(i32),
    /// Duplicate source (ADR-0133 decision 3).
    DuplicateSource(i32),
    /// Copy attributes of a source (ADR-0133 decision 6).
    CopyAttributes(i32),
    /// Open the Paste attributes (`true`) or Reset source window for a source.
    OpenAttributes(i32, bool),
    /// Open the Add vertical output dialog (ADR-0093 decision 8).
    AddVertical,
    /// Rename the scene with this id; without a name, ask for one in its row.
    RenameScene(i32, Option<String>),
    /// Opens the scene's "Transition into this scene" card (ADR-0135).
    SceneTransition(i32),
    /// Remove the scene with this id.
    RemoveScene(i32),
    /// Rename the source with this id; without a name, ask for one in its row.
    RenameSource(i32, Option<String>),
    /// Open a source again (ADR-0077).
    ReloadSource(i32),
    /// Open the Source settings window (ADR-0077 / ADR-0084).
    OpenSourceSettings(i32),
    /// Open Properties on a tab: 0 Transform, 1 Filters, 2 Audio, 3 Source.
    ShowProperties {
        source: i32,
        tab: i32,
    },
    /// Delete the source with this id.
    DeleteSource(i32),
    RemoveFromScene(i32),
    /// Move the source with this id in the layer order (ADR-0084 Arrange).
    MoveSource(i32, rbs_api::request::LayerDirection),
    /// Move a scene in the scene list.
    MoveScene(i32, rbs_api::request::LayerDirection),
    /// Keep a source's device open (`true`) or release it.
    SetSourceActive(i32, bool),
    /// Show (`true`) or hide the source with this id in the scene.
    SourceVisible(i32, bool),
    /// Lock (`true`) or unlock the source with this id.
    SourceLocked(i32, bool),
    /// Flip, turn, fit or centre the source with this id (ADR-0084), on this canvas
    /// (`canvas`, protocol 1.44), or without one on the canvas Properties edits.
    Transform(i32, TransformOp, Option<OutputCanvas>),
    /// Group, ungroup, align or distribute the selected sources (ADR-0182).
    Arrange(ArrangeOp),
    /// A group row's menu entry, on the group with this id (ADR-0182).
    Group(i32, GroupOp),
    /// Arrange the scene the vertical canvas shows (`canvas.auto_layout`).
    AutoLayoutVertical(rbs_api::request::VerticalLayout),
    /// The vertical canvas's size in pixels (`canvas.vertical_size`).
    VerticalSize(u32, u32),
    /// Mute (`true`) or unmute the source with this id.
    SourceMuted(i32, bool),
    /// Hide (`true`) or show the source with this id in the Mixer (ADR-0080).
    SourceMixerHidden(i32, bool),
    /// A source's monitor mode (`source.monitor`, ADR-0097).
    SourceMonitor(i32, SourceMonitor),
    /// Solo a source on the monitor or end its solo (`source.monitor_solo.toggle`).
    SourceSolo(i32, bool),
    /// Hide every shown source, only the muted ones, or show every hidden one.
    MixerBulk(MixerBulk),
    /// Show loudness on or off (`loudness.shown.toggle`, ADR-0092).
    LoudnessShown(bool),
    /// A scene's Own levels on (`true`) or back to Shared levels, which asks first
    /// from a window (`scene.audio.own_levels.toggle`, ADR-0091).
    SceneOwnLevels(i32, bool),
    /// Remove a scene's own level for a source (`scene.audio.reset_source`).
    ResetSceneLevel(i32, i32),
    /// Restart the loudness measurement (`loudness.reset`).
    ResetLoudness,
    /// The Mixer's layout (ADR-0083); view state, not layout history.
    MixerOrientation(MixerOrientation),
    /// Go live; `true`: also the vertical stream (ADR-0111, resolved at the press).
    GoLive(bool),
    /// Ask first, then end the stream (U4); `true`: also the vertical one.
    AskEndStream(bool),
    /// Record; `true`: also the vertical recording.
    Record(bool),
    /// Stop recording; `true`: also the vertical recording.
    StopRecording(bool),
    /// Whether Record (or Go live) also acts on the vertical canvas (ADR-0111).
    WithVertical(OutputKind, bool),
    /// Remove the vertical canvas.
    RemoveVertical,
    /// Stream the vertical canvas (ADR-0093 decision 6).
    GoLiveVertical,
    /// Ask first, then end the vertical stream (U4).
    AskEndVerticalStream,
    RecordVertical,
    StopVerticalRecording,
    /// Start (`true`) or stop sending to the virtual camera.
    VirtualCamera(bool),
    /// Arm (`true`) or disarm the replay buffer.
    Replay(bool),
    /// Save the replay buffer's past as a clip.
    SaveReplay,
    /// A marker in every running recording (ADR-0089 decision 1).
    AddMarker,
    /// Pause the running recordings, or resume them (ADR-0124).
    PauseRecording,
    /// Hold the live streams, or resume them (ADR-0124).
    HoldStream,
    /// Break: hold every stream and pause every recording, or resume them all.
    Break,
    /// Open (`true`) or close a canvas's projector (`ui.projector.open`,
    /// `ui.projector.close`, ADR-0125).
    Projector(ProjectorCanvas, bool),
    /// A projector's size, or full screen on the display named (`ui.projector.size`).
    ProjectorSize(ProjectorCanvas, ProjectorSize, Option<String>),
}

/// The `ui.projector.size` value that puts a projector full screen on a display; the
/// display's name follows.
const FULL_SCREEN_ON: &str = "display:";

/// The output kind an `output.with_vertical` id names.
fn output_kind(id: &CommandId) -> Option<OutputKind> {
    match id.param("kind")? {
        "record" => Some(OutputKind::Record),
        "stream" => Some(OutputKind::Stream),
        _ => None,
    }
}

/// The kinds Scene › Transition outside studio mode offers, in the rail's order
/// (ADR-0153 decision 6).
const DIRECT_KINDS: [(rbs_scene::TransitionKind, &str); 6] = [
    (rbs_scene::TransitionKind::Cut, "Cut"),
    (rbs_scene::TransitionKind::Fade, "Fade"),
    (rbs_scene::TransitionKind::Move, "Move"),
    (rbs_scene::TransitionKind::Swipe, "Swipe"),
    (rbs_scene::TransitionKind::Slide, "Slide"),
    (rbs_scene::TransitionKind::Stinger, "Stinger"),
];

/// Scene › Transition outside studio mode: `(title, kind, current)`. Each title
/// names the kind's duration, which both modes share (Move's is the glide), and says
/// when the kind cuts (0 ms, or a Stinger with no file), so a choice that looks like a
/// Cut is never a surprise (owner, 2026-11-19).
fn direct_choices(
    t: rbs_scene::CollectionTransition,
    glide_ms: u32,
) -> Vec<(String, String, bool)> {
    use rbs_scene::TransitionKind as K;
    DIRECT_KINDS
        .into_iter()
        .map(|(k, name)| {
            let ms = match k {
                K::Fade => Some(t.fade.duration_ms),
                K::Move => Some(glide_ms),
                K::Swipe => Some(t.swipe.duration_ms),
                K::Slide => Some(t.slide.duration_ms),
                _ => None,
            };
            let title = match ms {
                Some(0) => format!("{name} (0 ms: a cut)"),
                Some(ms) => format!("{name} ({ms} ms)"),
                None if k == K::Stinger && t.stinger.source.is_none() => {
                    format!("{name} (no file: a cut)")
                }
                None => name.to_owned(),
            };
            (title, k.as_str().to_owned(), k == t.direct)
        })
        .collect()
}

/// What `text` runs in this state; `None` for ids that cannot run (unknown, bad
/// parameters, a bare picker, disabled).
pub fn run(text: &str, state: &Shown<'_>) -> Option<Run> {
    use rbs_ui::LayoutCommand as L;
    let id = CommandId::parse(text).ok()?;
    let entry = id.resolve().ok()??;
    state.enabled(&id).ok()?;
    if let Kind::Picker { param, .. } = entry.kind {
        // `source.add?kind=K` opens the device picker of that kind; a bare one does
        // nothing (the menu is opened by the buttons).
        if id.base() == "source.add" {
            let what = match id.param(param)? {
                EXISTING => AddWhat::Existing,
                k => AddWhat::Kind(state.studio.kind_code_for_api(k)?),
            };
            return Some(Run::AddSource(what));
        }
        let value = id.param(param)?.to_owned();
        return Some(match id.base() {
            "ui.workspace.select" => Run::Workspace(value),
            "ui.panel.select" => Run::Panel(value),
            "ui.panel.move" => Run::MovePanel(panel_of(&id)?, value),
            "filter.add" => Run::AddFilter(state.source(&id).ok()?.id, value),
            "source.monitor" => Run::SourceMonitor(
                state.source(&id).ok()?.id,
                SourceMonitor::from_name(&value)?,
            ),
            "ui.mixer.orientation" => Run::MixerOrientation(MixerOrientation::parse(&value)?),
            "profile.select" => Run::Profile(value),
            "collection.select" => Run::Collection(value),
            "collection.direct_transition" => {
                Run::DirectTransition(rbs_scene::TransitionKind::parse(&value)?)
            }
            "ui.theme.select" => Run::SelectTheme(value),
            "ui.experience.select" => {
                Run::SelectExperience(crate::background::Experience::from_id(&value)?)
            }
            "ui.density.select" => Run::SelectDensity(density_from_id(&value)?),
            "output.with_vertical" => Run::WithVertical(
                output_kind(&id)?,
                match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return None,
                },
            ),
            "scene.select" => Run::Scene(value.parse().ok()?),
            "ui.projector.size" => {
                let canvas = Shown::projector_canvas(&id)?;
                match value.as_str() {
                    "window" => Run::ProjectorSize(canvas, ProjectorSize::Window, None),
                    "exact" => Run::ProjectorSize(canvas, ProjectorSize::Exact, None),
                    v => Run::ProjectorSize(
                        canvas,
                        ProjectorSize::Window,
                        Some(v.strip_prefix(FULL_SCREEN_ON)?.to_owned()),
                    ),
                }
            }
            "canvas.auto_layout" => {
                use rbs_api::request::VerticalLayout as V;
                Run::AutoLayoutVertical(match value.as_str() {
                    "fit" => V::Fit,
                    "fill" => V::Fill,
                    "stack" => V::Stack,
                    _ => return None,
                })
            }
            "item.rotate" | "item.fit" | "item.align" | "item.align_items" | "item.distribute" => {
                return arrange::picked(&id, &value, state);
            }
            _ => return None,
        });
    }
    let on = state.checked(&id);
    Some(match id.base() {
        "edit.undo" => Run::UndoEdit(true),
        "edit.redo" => Run::UndoEdit(false),
        "ui.area.toggle" => Run::Layout(L::SetAreaVisible {
            side: side(&id)?,
            visible: !on,
        }),
        "ui.area.hide_all" => Run::Layout(L::SetAllAreasVisible(false)),
        "ui.area.show_all" => Run::Layout(L::SetAllAreasVisible(true)),
        "ui.layout.reset_sizes" => Run::Layout(L::ResetSizes),
        "ui.layout.studio_rail" => Run::Layout(L::SetStudioRail(rail_at(&id)?)),
        "ui.layout.rail_size" => Run::Layout(L::SetRailSize(id.param("px")?.parse().ok()?)),
        "ui.toolbar.reset" => Run::Layout(L::ResetToolbar),
        "ui.workspace.reset" => Run::Layout(L::ResetWorkspace),
        "ui.theme.reload" => Run::ReloadThemes,
        "ui.settings.open" => open_settings(&id)?,
        "ui.app_menu.open" => Run::OpenAppMenu,
        "ui.collection.import_obs" => Run::OpenObsImport,
        "ui.panel.maximize" => Run::Maximize(panel_of(&id)?, !on),
        "ui.panel.collapse" => Run::Layout(L::SetCollapsed {
            panel: panel_of(&id)?,
            collapsed: !on,
        }),
        "ui.panel.auto_hide" => Run::AutoHide(panel_of(&id)?),
        "ui.workspace.save" => Run::SaveWorkspace,
        base if entry_op(base).is_some() => {
            let (kind, op) = entry_op(base)?;
            Run::Entry(kind, op, id.param("name").map(str::to_owned))
        }
        "ui.workspace.save_as" => Run::SaveWorkspaceAs(id.param("name").map(str::to_owned)),
        "ui.layout.lock" => Run::Layout(L::SetLocked(!on)),
        "ui.layout.undo" => Run::LayoutHistory(true),
        "ui.layout.redo" => Run::LayoutHistory(false),
        "ui.panel.float" => {
            let panel = panel_of(&id)?;
            if on {
                Run::Layout(L::Dock {
                    window: state.workspace.find(&panel)?.0,
                })
            } else {
                Run::Layout(L::Float(panel))
            }
        }
        "ui.panel.close" => Run::Layout(L::ClosePanel(panel_of(&id)?)),
        "canvas.studio_mode.toggle" => Run::StudioMode(!on),
        "canvas.vertical_preview.toggle" => {
            let panel = PanelId::parse(crate::studio_ui::VERTICAL_PANEL).ok()?;
            Run::Layout(if on {
                L::ClosePanel(panel)
            } else {
                L::OpenPanel(panel)
            })
        }
        "canvas.transition" => Run::Transition,
        "ui.transition.rehearse" => Run::Rehearse,
        "scene.position" => Run::Scene(state.studio.scene_at(scene_position(&id)?)?),
        "scene.next" => Run::Scene(state.studio.scene_step(1)?),
        "scene.previous" => Run::Scene(state.studio.scene_step(-1)?),
        "scene.add" => Run::AddScene,
        "scene.duplicate" if id.param("sources") == Some("copied") => {
            Run::DuplicateSceneWithCopies(state.scene(&id).ok()?)
        }
        "scene.duplicate" => Run::DuplicateScene(state.scene(&id).ok()?),
        "source.duplicate" => Run::DuplicateSource(state.source(&id).ok()?.id),
        "source.copy_attributes" => Run::CopyAttributes(state.source(&id).ok()?.id),
        "ui.source.paste_attributes" => Run::OpenAttributes(state.source(&id).ok()?.id, true),
        "ui.source.reset" => Run::OpenAttributes(state.source(&id).ok()?.id, false),
        "scene.rename" => {
            Run::RenameScene(state.scene(&id).ok()?, id.param("name").map(str::to_owned))
        }
        "scene.remove" => Run::RemoveScene(state.scene(&id).ok()?),
        "ui.scene.transition" => Run::SceneTransition(state.scene(&id).ok()?),
        "source.rename" => Run::RenameSource(
            state.source(&id).ok()?.id,
            id.param("name").map(str::to_owned),
        ),
        "source.reload" => Run::ReloadSource(state.source(&id).ok()?.id),
        "source.settings" => Run::OpenSourceSettings(state.source(&id).ok()?.id),
        "ui.properties.show" => {
            let tab = match id.param("tab")? {
                "transform" => 0,
                "filters" => 1,
                "audio" => 2,
                "source" => 3,
                _ => return None,
            };
            Run::ShowProperties {
                source: state.source(&id).ok()?.id,
                tab,
            }
        }
        "source.delete" => Run::DeleteSource(state.source(&id).ok()?.id),
        "item.remove" => Run::RemoveFromScene(state.source(&id).ok()?.id),
        "item.move" => {
            use rbs_api::request::LayerDirection::*;
            let direction = match id.param("direction")? {
                "up" => Up,
                "down" => Down,
                "front" => Front,
                "back" => Back,
                _ => return None,
            };
            Run::MoveSource(state.source(&id).ok()?.id, direction)
        }
        "scene.move" => {
            use rbs_api::request::LayerDirection::*;
            let direction = match id.param("direction")? {
                "up" => Up,
                "down" => Down,
                "front" => Front,
                "back" => Back,
                _ => return None,
            };
            Run::MoveScene(state.scene(&id).ok()?, direction)
        }
        "source.active.toggle" => Run::SetSourceActive(state.source(&id).ok()?.id, !on),
        "item.visible.toggle" => Run::SourceVisible(state.source(&id).ok()?.id, !on),
        "item.locked.toggle" => Run::SourceLocked(state.source(&id).ok()?.id, !on),
        base if arrange::runs(base) => return arrange::run(&id, state, on),
        "source.muted.toggle" => Run::SourceMuted(state.source(&id).ok()?.id, !on),
        "source.mixer_hidden.toggle" => Run::SourceMixerHidden(state.source(&id).ok()?.id, !on),
        "source.monitor_solo.toggle" => Run::SourceSolo(state.source(&id).ok()?.id, !on),
        "loudness.shown.toggle" => Run::LoudnessShown(!on),
        "scene.audio.own_levels.toggle" => Run::SceneOwnLevels(state.scene(&id).ok()?, !on),
        "scene.audio.reset_source" => {
            Run::ResetSceneLevel(state.scene(&id).ok()?, heard_source(&id)?)
        }
        "loudness.reset" => Run::ResetLoudness,
        "mixer.hide_all" => Run::MixerBulk(MixerBulk::HideAll),
        "mixer.hide_muted" => Run::MixerBulk(MixerBulk::HideMuted),
        "mixer.unhide_all" => Run::MixerBulk(MixerBulk::ShowAll),
        "stream.toggle" if state.studio.go_live_is_vertical() => {
            if on {
                Run::AskEndVerticalStream
            } else {
                Run::GoLiveVertical
            }
        }
        "stream.toggle" | "stream.main.toggle" => {
            let all =
                id.base() == "stream.toggle" && state.studio.with_vertical(OutputKind::Stream);
            if on {
                Run::AskEndStream(all)
            } else {
                Run::GoLive(all)
            }
        }
        "record.toggle" | "record.main.toggle" => {
            let all =
                id.base() == "record.toggle" && state.studio.with_vertical(OutputKind::Record);
            if on {
                Run::StopRecording(all)
            } else {
                Run::Record(all)
            }
        }
        "canvas.remove_vertical" => Run::RemoveVertical,
        "stream.vertical.toggle" if on => Run::AskEndVerticalStream,
        "stream.vertical.toggle" => Run::GoLiveVertical,
        "record.vertical.toggle" if on => Run::StopVerticalRecording,
        "record.vertical.toggle" => Run::RecordVertical,
        "vcam.toggle" => Run::VirtualCamera(!on),
        "replay.toggle" => Run::Replay(!on),
        "replay.save" => Run::SaveReplay,
        "marker.add" => Run::AddMarker,
        "record.pause_toggle" => Run::PauseRecording,
        "stream.hold_toggle" => Run::HoldStream,
        "output.break_toggle" => Run::Break,
        "canvas.add_vertical" => Run::AddVertical,
        "ui.projector.open" => Run::Projector(Shown::projector_canvas(&id)?, true),
        "ui.projector.close" => Run::Projector(Shown::projector_canvas(&id)?, false),
        "canvas.vertical_size" => {
            let (w, h) = size_of(&id)?;
            Run::VerticalSize(w, h)
        }
        _ => return None,
    })
}

/// `canvas.vertical_size`'s width and height, when both are numbers.
fn size_of(id: &CommandId) -> Option<(u32, u32)> {
    Some((
        id.param("width")?.parse().ok()?,
        id.param("height")?.parse().ok()?,
    ))
}

/// The command and API value of a density (`regular`, `compact`, `roomy`).
pub fn density_id(density: rbs_ui::tokens::Density) -> &'static str {
    match density {
        rbs_ui::tokens::Density::Regular => "regular",
        rbs_ui::tokens::Density::Compact => "compact",
        rbs_ui::tokens::Density::Roomy => "roomy",
    }
}

/// The density named `id`, if any.
pub fn density_from_id(id: &str) -> Option<rbs_ui::tokens::Density> {
    use rbs_ui::tokens::Density;
    [Density::Regular, Density::Compact, Density::Roomy]
        .into_iter()
        .find(|d| density_id(*d) == id)
}

/// The source an id names by its `source` parameter: a source the Mixer hears may not
/// be in the scene being edited (studio mode), so it is not looked up there.
fn heard_source(id: &CommandId) -> Option<i32> {
    id.param("source")?.parse().ok()
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// The window files that declare the menu bar (ADR-0138: one file per window group).
    const WINDOW_FILES: [&str; 2] = ["ui/main_window.slint", "ui/settings_window.slint"];

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
        assert_eq!(seen, (0..14).collect::<Vec<_>>());
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
}
