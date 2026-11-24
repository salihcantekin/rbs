//! The asks of the Sources rows (rename, remove confirm) and what a group row's menu runs (ADR-0182): Visible and Locked go to every member in one
//! undo step; Rename and Remove ask the group's row in the Sources list, which shows the
//! name field and the confirm, as for a source's row.

use rbs_ui::{LayoutCommand, Origin};
use slint::ComponentHandle as _;

use crate::menu_bar::GroupOp;
use crate::ui::Studio;
use crate::{App, panel};

impl App {
    /// Runs `op` on group `group`. Rename and Remove need someone at the window, so a
    /// control surface or a global shortcut (`confirm` false) does nothing.
    pub(crate) fn run_group(&mut self, group: i32, op: GroupOp, confirm: bool) {
        match op {
            GroupOp::Visible(on) => self.studio.set_group_visible(group, on),
            GroupOp::Locked(on) => self.studio.set_group_locked(group, on),
            GroupOp::Rename | GroupOp::Remove if !confirm => {}
            GroupOp::Rename | GroupOp::Remove => self.ask_group(group, op == GroupOp::Remove),
        }
    }

    /// Opens the Sources panel and asks the group's row for its name field (`remove`
    /// false) or its confirm (`remove` true). The count makes each ask a change.
    fn ask_group(&mut self, group: i32, remove: bool) {
        if let Some(p) = panel("rbs.sources") {
            self.command(LayoutCommand::OpenPanel(p.clone()), Origin::Command);
            self.raise_host_of(&p);
        }
        let ask = |g: Studio<'_>| {
            if remove {
                g.set_confirming_group(group);
                g.set_confirm_group_asked(g.get_confirm_group_asked() + 1);
            } else {
                g.set_renaming_group(group);
                g.set_rename_group_asked(g.get_rename_group_asked() + 1);
            }
        };
        ask(self.main.global::<Studio>());
        for f in &self.floats {
            ask(f.window.global::<Studio>());
        }
    }

    /// Starts the name field of a scene's (`scene`) or source's row: Rename from a menu
    /// or a shortcut. The row's panel is opened first.
    pub(crate) fn ask_rename(&mut self, id: i32, scene: bool) {
        let name = if scene { "rbs.scenes" } else { "rbs.sources" };
        if let Some(p) = panel(name) {
            self.command(LayoutCommand::OpenPanel(p.clone()), Origin::Command);
            self.raise_host_of(&p);
        }
        // The id says which row; the count makes each ask a change, also for the same
        // row twice. A row made later (its panel was closed) checks the id when made.
        let ask = |g: Studio<'_>| {
            if scene {
                g.set_renaming_scene(id);
                g.set_rename_scene_asked(g.get_rename_scene_asked() + 1);
            } else {
                g.set_renaming_source(id);
                g.set_rename_source_asked(g.get_rename_source_asked() + 1);
            }
        };
        ask(self.main.global::<Studio>());
        for f in &self.floats {
            ask(f.window.global::<Studio>());
        }
    }

    /// Shows the source row's confirm before Remove from scene (`delete` false) or
    /// Delete source (`delete` true). The Sources panel opens, and the source is
    /// selected so a folded global row is in the list.
    pub(crate) fn ask_remove_source(&mut self, id: i32, delete: bool) {
        if let Some(p) = panel("rbs.sources") {
            self.command(LayoutCommand::OpenPanel(p.clone()), Origin::Command);
            self.raise_host_of(&p);
        }
        self.studio.selected = id;
        let ask = |g: Studio<'_>| {
            g.set_selected_source(id);
            g.set_confirm_source_kind(if delete { 2 } else { 1 });
            g.set_confirming_source(id);
            g.set_confirm_source_asked(g.get_confirm_source_asked() + 1);
        };
        ask(self.main.global::<Studio>());
        for f in &self.floats {
            ask(f.window.global::<Studio>());
        }
    }
}
