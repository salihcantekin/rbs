//! The menu bar, built from the command registry (ADR-0050, U20).
//!
//! Slint needs a window's `MenuBar` written out in the window itself, so the menus'
//! shape (which menus, items, separators and choice submenus, in order) is generated
//! from [`rbs_ui::command::MENU_LAYOUT`] by `slint_text` (tests only) and kept in `ui/app.slint`
//! between marker lines; a test fails when that copy is out of date (run it with
//! `RBS_UPDATE_MENU_BAR=1` to rewrite it). What changes at run time (titles, enabled
//! and checked states, a picker's choices) comes from [`Values`], indexed in the same
//! order, through the `Menus` global.

use rbs_ui::command::{CommandId, CommandState, Kind, MenuItem, menus};
use rbs_ui::workspace::{Side, Workspace};

use crate::studio_ui::{StudioUi, rec_state, st_state};

/// The run-time values of the menu bar, in the generated items' order: one entry
/// per command or choice submenu in `titles`, `enabled` and `checked`, one list per
/// choice submenu in `choices` (`(title, id to run, current)`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values {
    pub titles: Vec<String>,
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
                    ..
                } => {
                    v.titles.push(title);
                    v.enabled.push(disabled.is_none());
                    v.checked.push(checked.unwrap_or(false));
                }
                MenuItem::Choices { title, choices } => {
                    v.titles.push(title);
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
    line(0, "MenuBar {");
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
                        "MenuItem {{ title: Menus.titles[{index}]; enabled: Menus.enabled[{index}];"
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

    fn running(&self) -> Result<(), String> {
        match self.studio.state {
            1 => Ok(()),
            0 => Err("The studio is starting.".to_owned()),
            _ => Err("The studio stopped.".to_owned()),
        }
    }
}

impl CommandState for Shown<'_> {
    fn enabled(&self, id: &CommandId) -> Result<(), String> {
        let s = self.studio;
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
            "stream.toggle" => matches!(self.studio.st_state, st_state::LIVE | st_state::STARTING),
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
            "profile.select" => self.studio.library_choices(false),
            "collection.select" => self.studio.library_choices(true),
            _ => Vec::new(),
        }
    }
}

/// What running an id from the menu bar does.
#[derive(Clone, Debug, PartialEq)]
pub enum Run {
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
    /// A layout change.
    Layout(rbs_ui::LayoutCommand),
    GoLive,
    /// Ask first, then end the stream (U4).
    AskEndStream,
    Record,
    StopRecording,
}

/// What `text` runs in this state; `None` for ids that cannot run (unknown, bad
/// parameters, a bare picker, disabled).
pub fn run(text: &str, state: &Shown<'_>) -> Option<Run> {
    use rbs_ui::LayoutCommand as L;
    let id = CommandId::parse(text).ok()?;
    let entry = id.resolve().ok()??;
    state.enabled(&id).ok()?;
    if let Kind::Picker { param, .. } = entry.kind {
        let value = id.param(param)?.to_owned();
        return Some(match id.base() {
            "ui.workspace.select" => Run::Workspace(value),
            "ui.panel.select" => Run::Panel(value),
            "profile.select" => Run::Profile(value),
            "collection.select" => Run::Collection(value),
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
        "ui.toolbar.reset" => Run::Layout(L::ResetToolbar),
        "stream.toggle" if on => Run::AskEndStream,
        "stream.toggle" => Run::GoLive,
        "record.toggle" if on => Run::StopRecording,
        "record.toggle" => Run::Record,
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
    fn titles_are_quoted_for_slint() {
        assert_eq!(quoted("a \"b\" \\"), "\"a \\\"b\\\" \\\\\"");
    }

    #[test]
    fn refreshing_keeps_the_text_around_the_markers() {
        let text = format!("a\n    {BEGIN}\n    old\n    {END}\nb\n");
        let fresh = refreshed(&text).expect("markers");
        assert!(fresh.starts_with(&format!("a\n    {BEGIN}\n    MenuBar {{\n")));
        assert!(fresh.ends_with(&format!("    }}\n    {END}\nb\n")));
        assert!(!fresh.contains("old"));
        assert_eq!(refreshed("no markers"), None);
    }
}
