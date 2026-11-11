//! The menu bar, built from the command registry (ADR-0050, U20).
//!
//! Slint needs a window's `MenuBar` written out in the window itself, so the menus'
//! shape (which menus, items, separators and choice submenus, in order) is generated
//! from [`rbs_ui::command::MENU_LAYOUT`] by `slint_text` (tests only) and kept in `ui/app.slint`
//! between marker lines; a test fails when that copy is out of date (run it with
//! `RBS_UPDATE_MENU_BAR=1` to rewrite it). What changes at run time (titles, enabled
//! and checked states, a picker's choices) comes from [`Values`], indexed in the same
//! order, through the `Menus` global.

use rbs_scene::model::SourceMonitor;
use rbs_scene::{CenterAxis, FitMode};
use rbs_ui::command::{CommandId, CommandState, Kind, MenuItem, menus};
use rbs_ui::layout::RailAt;
use rbs_ui::panel::PanelId;
use rbs_ui::workspace::{MixerOrientation, Side, Workspace};

use crate::studio_host::TransformOp;
use crate::studio_ui::filters::filter_types;
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
                    choices,
                } => {
                    v.titles.push(title);
                    v.icons.push(item_icon(icon, None));
                    v.enabled.push(!choices.is_empty());
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
    fn enabled(&self, _: &CommandId) -> Result<(), String> {
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
                            "for c in Menus.choices[{picker}]: MenuItem {{ title: c.title; checkable: true; checked: c.checked; activated => {{ Menus.run(c.id); }} }}"
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
/// The begin and end marker lines around each generated copy in `ui/app.slint`.
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

/// The control API names of the source kinds, in `SourceKind::ALL` order (the
/// `kind` choices of `source.add`).
const KIND_IDS: [&str; 6] = [
    "screen",
    "window",
    "camera",
    "audio_input",
    "system_audio",
    "application_audio",
];

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
    fn source(&self, id: &CommandId) -> Result<SourceFacts, String> {
        self.studio
            .source_facts(id.param("source"), id.param("item"))
            .ok_or_else(|| match (id.param("source"), id.param("item")) {
                (None, None) => "No source is selected.".to_owned(),
                _ => "The source is not in this scene.".to_owned(),
            })
    }

    /// The scene an id names, or the one being worked on.
    fn scene(&self, id: &CommandId) -> Result<i32, String> {
        self.studio
            .scene_target(id.param("scene"))
            .ok_or_else(|| "The scene is gone.".to_owned())
    }

    fn running(&self) -> Result<(), String> {
        match self.studio.state {
            1 => Ok(()),
            0 => Err("The studio is starting.".to_owned()),
            _ => Err("The studio stopped.".to_owned()),
        }
    }

    /// The vertical canvas's picture panel is open (in any window).
    fn vertical_panel_open(&self) -> bool {
        PanelId::parse(crate::studio_ui::VERTICAL_PANEL).is_ok_and(|p| self.workspace.is_open(&p))
    }

    fn has_vertical(&self) -> Result<(), String> {
        match self.studio.vertical {
            Some(_) => Ok(()),
            None => Err("Add a vertical output first.".to_owned()),
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

impl CommandState for Shown<'_> {
    fn enabled(&self, id: &CommandId) -> Result<(), String> {
        let s = self.studio;
        // Scene, source and item ids are unique only within a scene collection.
        if let Some(c) = id.param("collection")
            && s.current_collection().as_deref() != Some(c)
        {
            return Err("In another scene collection.".to_owned());
        }
        match id.base() {
            "edit.undo" => {
                self.running()?;
                s.next_undo
                    .as_ref()
                    .map(|_| ())
                    .ok_or_else(|| "Nothing to undo.".to_owned())
            }
            "edit.redo" => {
                self.running()?;
                s.next_redo
                    .as_ref()
                    .map(|_| ())
                    .ok_or_else(|| "Nothing to redo.".to_owned())
            }
            "ui.area.toggle" => match side(id) {
                Some(sd) if self.areas().any(|(a, has, _)| a == sd && has) => Ok(()),
                _ => Err("This area has no panels.".to_owned()),
            },
            "ui.area.hide_all" | "ui.area.show_all" => {
                if self.areas().any(|(_, has, _)| has) {
                    Ok(())
                } else {
                    Err("No area beside the centre has panels.".to_owned())
                }
            }
            "source.add" => {
                self.running()?;
                if id.param("kind") == Some(EXISTING) && !self.studio.has_existing_sources() {
                    return Err("No other scene has a source to add.".to_owned());
                }
                Ok(())
            }
            "canvas.studio_mode.toggle" | "scene.add" | "canvas.add_vertical" => self.running(),
            // An open panel can always be closed.
            "canvas.vertical_preview.toggle" if self.vertical_panel_open() => Ok(()),
            "canvas.vertical_preview.toggle" => {
                self.running()?;
                self.has_vertical()
            }
            "scene.rename" => {
                self.running()?;
                self.scene(id).map(|_| ())
            }
            "scene.remove" => {
                self.running()?;
                self.scene(id)?;
                if s.scene_count() > 1 {
                    Ok(())
                } else {
                    Err("The last scene cannot be removed.".to_owned())
                }
            }
            "item.visible.toggle" | "source.rename" | "source.delete" | "item.remove" => {
                self.running()?;
                self.source(id).map(|_| ())
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
                        Err("That filter type is not available for this source.".to_owned())
                    }
                    _ => Ok(()),
                }
            }
            "source.reload" => {
                self.running()?;
                if self.source(id)?.reloadable {
                    Ok(())
                } else {
                    Err("This source has nothing to open again.".to_owned())
                }
            }
            "item.flip" | "item.rotate" | "item.align" | "item.fit" | "item.reset_transform" => {
                self.running()?;
                let f = self.source(id)?;
                if !f.picture {
                    return Err("The source has no picture.".to_owned());
                }
                if f.locked {
                    return Err("Unlock the source to change its transform.".to_owned());
                }
                if matches!(id.base(), "item.fit" | "item.reset_transform") && !f.sized {
                    return Err("The source has no picture yet.".to_owned());
                }
                Ok(())
            }
            "item.locked.toggle" => {
                self.running()?;
                if self.source(id)?.picture {
                    Ok(())
                } else {
                    Err("The source has no picture.".to_owned())
                }
            }
            "source.muted.toggle" => {
                self.running()?;
                if self.source(id)?.audio {
                    Ok(())
                } else {
                    Err("The source has no sound in the Mixer.".to_owned())
                }
            }
            "source.mixer_hidden.toggle" | "source.monitor" | "source.monitor_solo.toggle" => {
                self.running()?;
                if self.source(id)?.audio {
                    Ok(())
                } else {
                    Err("The source has no sound in the Mixer.".to_owned())
                }
            }
            "mixer.hide_all" | "mixer.hide_muted" | "mixer.unhide_all" => {
                self.running()?;
                let (shown, muted, hidden) = s.mixer_counts();
                match id.base() {
                    "mixer.hide_all" if shown == 0 => Err("Every source is hidden.".to_owned()),
                    "mixer.hide_muted" if muted == 0 => Err("No shown source is muted.".to_owned()),
                    "mixer.unhide_all" if hidden == 0 => Err("No source is hidden.".to_owned()),
                    _ => Ok(()),
                }
            }
            "canvas.transition" => {
                self.running()?;
                if s.studio_mode {
                    Ok(())
                } else {
                    Err("Studio mode is off.".to_owned())
                }
            }
            "scene.select" => self.running(),
            "scene.position" => {
                self.running()?;
                scene_position(id)
                    .and_then(|n| s.scene_at(n))
                    .map(|_| ())
                    .ok_or_else(|| "No scene is at this position.".to_owned())
            }
            "scene.next" | "scene.previous" => {
                self.running()?;
                s.scene_step(1)
                    .map(|_| ())
                    .ok_or_else(|| "There is only one scene.".to_owned())
            }
            "ui.panel.maximize" => match panel_of(id) {
                Some(p) if self.workspace.is_open(&p) => Ok(()),
                _ => Err("The panel is not open.".to_owned()),
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
            }
            "ui.layout.undo" if !self.can_undo => {
                Err("There is no layout change to undo.".to_owned())
            }
            "ui.layout.redo" if !self.can_redo => {
                Err("There is no layout change to redo.".to_owned())
            }
            "ui.panel.collapse" | "ui.panel.auto_hide" | "ui.panel.move" | "ui.panel.float"
            | "ui.panel.close" => {
                let Some(c) = self.panel_caps(id) else {
                    return Err("The panel is not open.".to_owned());
                };
                match id.base() {
                    // The lock stops what rearranges; collapse and auto-hide still work.
                    "ui.panel.move" | "ui.panel.float" | "ui.panel.close"
                        if self.workspace.locked =>
                    {
                        Err("The layout is locked.".to_owned())
                    }
                    "ui.panel.collapse" if !c.can_collapse => Err(
                        "Preview and Program, and the last open stack of the centre, stay open."
                            .to_owned(),
                    ),
                    "ui.panel.auto_hide" if !c.can_hide => Err(
                        "Only a stack at the edge of the main window can hide, and not the \
                         only stack of the centre."
                            .to_owned(),
                    ),
                    "ui.panel.move" if id.param("to") == Some("next_stack") && c.stacks < 2 => {
                        Err("This window has one tab stack.".to_owned())
                    }
                    _ => Ok(()),
                }
            }
            "ui.layout.rail_size" => match (self.workspace.studio_rail(), id.param("px")) {
                (None, _) => Err("This workspace has no Preview and Program side by side in \
                                  the centre."
                    .to_owned()),
                (Some(_), Some(px)) if px.parse::<u32>().is_ok() => Ok(()),
                _ => Err("The width is a whole number of pixels.".to_owned()),
            },
            "ui.layout.studio_rail" => match self.workspace.studio_rail() {
                Some(_) => Ok(()),
                None => Err("This workspace has no Preview and Program side by side in \
                             the centre."
                    .to_owned()),
            },
            "ui.workspace.reset" => match rbs_ui::builtin::by_name(&self.workspace.name) {
                None => Err("Only a built-in workspace has a default layout.".to_owned()),
                Some(mut d) => {
                    d.name.clone_from(&self.workspace.name);
                    if d == *self.workspace {
                        Err("The workspace already has its default layout.".to_owned())
                    } else {
                        Ok(())
                    }
                }
            },
            "ui.toolbar.reset" => match self.workspace.toolbar {
                Some(_) => Ok(()),
                None => Err("The toolbar already has its default items.".to_owned()),
            },
            "stream.toggle" => match s.st_state {
                st_state::LIVE | st_state::STARTING => Ok(()),
                st_state::ENDING => Err("The stream is ending.".to_owned()),
                _ => {
                    self.running()?;
                    if s.rec_state != rec_state::OFF {
                        return Err("Stop the recording to go live.".to_owned());
                    }
                    s.cannot_record.clone().map_or(Ok(()), Err)
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
            "replay.save" => {
                self.running()?;
                if self.replay.can_save {
                    Ok(())
                } else if self.replay.armed {
                    Err("A clip is being saved.".to_owned())
                } else {
                    Err("Arm the replay buffer first.".to_owned())
                }
            }
            "stream.vertical.toggle" => match s.vertical_st {
                VerticalOut::On | VerticalOut::Starting => Ok(()),
                VerticalOut::Ending => Err("The vertical stream is ending.".to_owned()),
                VerticalOut::Off => {
                    self.running()?;
                    self.has_vertical()?;
                    s.cannot_record.clone().map_or(Ok(()), Err)
                }
            },
            "record.vertical.toggle" => match s.vertical_rec {
                VerticalOut::On => Ok(()),
                VerticalOut::Starting => Err("The vertical recording is starting.".to_owned()),
                VerticalOut::Ending => Err("The vertical recording is stopping.".to_owned()),
                VerticalOut::Off => {
                    self.running()?;
                    self.has_vertical()?;
                    s.cannot_record.clone().map_or(Ok(()), Err)
                }
            },
            "record.toggle" => match s.rec_state {
                rec_state::RECORDING => Ok(()),
                rec_state::STARTING => Err("The recording is starting.".to_owned()),
                rec_state::STOPPING => Err("The recording is stopping.".to_owned()),
                _ => {
                    self.running()?;
                    if s.st_state != st_state::OFF {
                        return Err("End the stream to record.".to_owned());
                    }
                    s.cannot_record.clone().map_or(Ok(()), Err)
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
            "item.flip" => self.source(id).is_ok_and(|f| match id.param("axis") {
                Some("horizontal") => f.flip.horizontal,
                Some("vertical") => f.flip.vertical,
                _ => false,
            }),
            "item.visible.toggle" => self.source(id).is_ok_and(|f| f.visible),
            "item.locked.toggle" => self.source(id).is_ok_and(|f| f.locked),
            "source.muted.toggle" => self.source(id).is_ok_and(|f| f.muted),
            "source.mixer_hidden.toggle" => self.source(id).is_ok_and(|f| f.mixer_hidden),
            "source.monitor_solo.toggle" => self.source(id).is_ok_and(|f| f.soloed),
            "stream.toggle" => matches!(self.studio.st_state, st_state::LIVE | st_state::STARTING),
            "vcam.toggle" => self.camera.sending,
            "replay.toggle" => self.replay.armed,
            "stream.vertical.toggle" => self.studio.vertical_st.on(),
            "record.vertical.toggle" => self.studio.vertical_rec.on(),
            "record.toggle" => matches!(
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
                    (MixerOrientation::Auto, "Auto"),
                    (MixerOrientation::Rows, "Rows"),
                    (MixerOrientation::Columns, "Columns"),
                ]
                .into_iter()
                .map(|(o, title)| (title.to_owned(), o.name().to_owned(), o == now))
                .collect()
            }
            "profile.select" => self.studio.library_choices(false),
            "collection.select" => self.studio.library_choices(true),
            "scene.select" => self.studio.scene_choices(),
            "item.rotate" => vec![
                ("Rotate 90° clockwise".into(), "90".into(), false),
                ("Rotate 90° counter-clockwise".into(), "-90".into(), false),
                ("Rotate 180°".into(), "180".into(), false),
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
                    .kind_codes()
                    .into_iter()
                    .filter_map(|(code, label)| {
                        let name = KIND_IDS.get(usize::try_from(code).ok()?)?;
                        Some((label.to_owned(), (*name).to_owned(), false))
                    })
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
        match id.base() {
            "item.locked.toggle"
            | "item.flip"
            | "item.rotate"
            | "item.fit"
            | "item.align"
            | "item.reset_transform" => self.source(id).is_ok_and(|f| f.picture),
            "source.muted.toggle"
            | "source.mixer_hidden.toggle"
            | "source.monitor"
            | "source.monitor_solo.toggle" => self.source(id).is_ok_and(|f| f.audio),
            // A vertical canvas's outputs, while it exists (or one of them still runs).
            "stream.vertical.toggle" | "record.vertical.toggle" => {
                self.studio.vertical.is_some()
                    || self.studio.vertical_st.busy()
                    || self.studio.vertical_rec.busy()
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

/// What `ui.settings.open` runs: Settings at the id's section, if it names one.
fn open_settings(id: &CommandId) -> Option<Run> {
    Some(Run::OpenSettings(match id.param("section") {
        Some(name) => Some(settings_section(name)?),
        None => None,
    }))
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
        // A canvas's own toggle shows only while that canvas exists.
        .filter(|item| match item {
            ToolbarItem::Command(c) if c.as_str() == "canvas.vertical_preview.toggle" => {
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
                let (title, live) = match (id.base(), on) {
                    ("stream.toggle", true) => ("End stream".to_owned(), true),
                    ("record.toggle", true) => ("Stop recording".to_owned(), true),
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
    /// Show the Settings window, at this section (`SettingsWindow.section`) when
    /// given (`ui.settings.open`).
    OpenSettings(Option<i32>),
    /// Open the RBS menu with its search field (`ui.app_menu.open`).
    OpenAppMenu,
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
    /// Add a scene.
    AddScene,
    /// Open the Add vertical output dialog (ADR-0093 decision 8).
    AddVertical,
    /// Rename the scene with this id; without a name, ask for one in its row.
    RenameScene(i32, Option<String>),
    /// Remove the scene with this id.
    RemoveScene(i32),
    /// Rename the source with this id; without a name, ask for one in its row.
    RenameSource(i32, Option<String>),
    /// Open a source again (ADR-0077).
    ReloadSource(i32),
    /// Delete the source with this id.
    DeleteSource(i32),
    RemoveFromScene(i32),
    /// Show (`true`) or hide the source with this id in the scene.
    SourceVisible(i32, bool),
    /// Lock (`true`) or unlock the source with this id.
    SourceLocked(i32, bool),
    /// Flip, turn, fit or centre the source with this id (ADR-0084).
    Transform(i32, TransformOp),
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
    /// The Mixer's layout (ADR-0083); view state, not layout history.
    MixerOrientation(MixerOrientation),
    GoLive,
    /// Ask first, then end the stream (U4).
    AskEndStream,
    Record,
    StopRecording,
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
                k => AddWhat::Kind(KIND_IDS.iter().position(|n| *n == k)? as i32),
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
            "scene.select" => Run::Scene(value.parse().ok()?),
            "item.rotate" => Run::Transform(
                state.source(&id).ok()?.id,
                TransformOp::Turn(value.parse::<i32>().ok()? as f32),
            ),
            "item.fit" => Run::Transform(
                state.source(&id).ok()?.id,
                TransformOp::Fit(match value.as_str() {
                    "fit" => FitMode::Fit,
                    "fill" => FitMode::Fill,
                    "stretch" => FitMode::Stretch,
                    _ => return None,
                }),
            ),
            "item.align" => Run::Transform(
                state.source(&id).ok()?.id,
                TransformOp::Center(match value.as_str() {
                    "both" => CenterAxis::Both,
                    "horizontal" => CenterAxis::Horizontal,
                    "vertical" => CenterAxis::Vertical,
                    _ => return None,
                }),
            ),
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
        "scene.position" => Run::Scene(state.studio.scene_at(scene_position(&id)?)?),
        "scene.next" => Run::Scene(state.studio.scene_step(1)?),
        "scene.previous" => Run::Scene(state.studio.scene_step(-1)?),
        "scene.add" => Run::AddScene,
        "scene.rename" => {
            Run::RenameScene(state.scene(&id).ok()?, id.param("name").map(str::to_owned))
        }
        "scene.remove" => Run::RemoveScene(state.scene(&id).ok()?),
        "source.rename" => Run::RenameSource(
            state.source(&id).ok()?.id,
            id.param("name").map(str::to_owned),
        ),
        "source.reload" => Run::ReloadSource(state.source(&id).ok()?.id),
        "source.delete" => Run::DeleteSource(state.source(&id).ok()?.id),
        "item.remove" => Run::RemoveFromScene(state.source(&id).ok()?.id),
        "item.visible.toggle" => Run::SourceVisible(state.source(&id).ok()?.id, !on),
        "item.locked.toggle" => Run::SourceLocked(state.source(&id).ok()?.id, !on),
        "item.flip" => Run::Transform(
            state.source(&id).ok()?.id,
            match id.param("axis")? {
                "horizontal" => TransformOp::FlipHorizontal,
                "vertical" => TransformOp::FlipVertical,
                _ => return None,
            },
        ),
        "item.reset_transform" => Run::Transform(state.source(&id).ok()?.id, TransformOp::Reset),
        "source.muted.toggle" => Run::SourceMuted(state.source(&id).ok()?.id, !on),
        "source.mixer_hidden.toggle" => Run::SourceMixerHidden(state.source(&id).ok()?.id, !on),
        "source.monitor_solo.toggle" => Run::SourceSolo(state.source(&id).ok()?.id, !on),
        "mixer.hide_all" => Run::MixerBulk(MixerBulk::HideAll),
        "mixer.hide_muted" => Run::MixerBulk(MixerBulk::HideMuted),
        "mixer.unhide_all" => Run::MixerBulk(MixerBulk::ShowAll),
        "stream.toggle" if on => Run::AskEndStream,
        "stream.toggle" => Run::GoLive,
        "record.toggle" if on => Run::StopRecording,
        "record.toggle" => Run::Record,
        "stream.vertical.toggle" if on => Run::AskEndVerticalStream,
        "stream.vertical.toggle" => Run::GoLiveVertical,
        "record.vertical.toggle" if on => Run::StopVerticalRecording,
        "record.vertical.toggle" => Run::RecordVertical,
        "vcam.toggle" => Run::VirtualCamera(!on),
        "replay.toggle" => Run::Replay(!on),
        "replay.save" => Run::SaveReplay,
        "canvas.add_vertical" => Run::AddVertical,
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const APP_SLINT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.slint");

    #[test]
    fn app_slint_has_the_current_menu_bar() {
        let text = std::fs::read_to_string(APP_SLINT).expect("ui/app.slint is readable");
        // A Windows checkout may have CRLF line ends; the generated text has LF.
        let text = text.replace("\r\n", "\n");
        let fresh = refreshed(&text).expect("ui/app.slint has the menu bar markers");
        if fresh != text && std::env::var_os("RBS_UPDATE_MENU_BAR").is_some() {
            std::fs::write(APP_SLINT, &fresh).expect("ui/app.slint is writable");
            return;
        }
        assert!(
            fresh == text,
            "the menu bar in ui/app.slint is out of date: run \
             `RBS_UPDATE_MENU_BAR=1 cargo test -p rbs-app --bin rbs menu_bar`"
        );
        // Both windows with a menu bar have it.
        assert_eq!(text.matches(BEGIN).count(), 2);
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
    fn every_settings_section_has_its_window_number() {
        let mut seen: Vec<i32> = rbs_ui::command::SETTINGS_SECTIONS
            .iter()
            .map(|s| settings_section(s).expect("known section"))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
        assert_eq!(settings_section("mixer"), None);
        let run = |t: &str| open_settings(&CommandId::parse(t).expect("id"));
        assert_eq!(run("ui.settings.open"), Some(Run::OpenSettings(None)));
        assert_eq!(
            run("ui.settings.open?section=shortcuts"),
            Some(Run::OpenSettings(Some(6)))
        );
        assert_eq!(run("ui.settings.open?section=mixer"), None);
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
        let text = format!("a\n    {BEGIN}\n    old\n    {END}\nb\n");
        let fresh = refreshed(&text).expect("markers");
        assert!(fresh.starts_with(&format!(
            "a\n    {BEGIN}\n    if Shell.native-menu-bar: MenuBar {{\n"
        )));
        assert!(fresh.ends_with(&format!("    }}\n    {END}\nb\n")));
        assert!(!fresh.contains("old"));
        assert_eq!(refreshed("no markers"), None);
    }
}
