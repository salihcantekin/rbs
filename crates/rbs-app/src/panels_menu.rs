//! The Panels menu (View › Panels, Window › Panels): each item toggles its panel
//! (ADR-0172 decision 1). A checked item is an open panel; choosing it closes the
//! panel, which remembers where it was, and choosing an unchecked one opens it
//! there again.

use rbs_ui::{LayoutCommand, Origin};

use super::{App, panel};

impl App {
    /// Runs a Panels menu item for panel `id`.
    pub(crate) fn toggle_from_panels_menu(&mut self, id: &str) {
        let Some(id) = panel(id) else {
            return;
        };
        if self.session.workspace().is_open(&id) {
            self.command(LayoutCommand::ClosePanel(id), Origin::Command);
            return;
        }
        self.command(LayoutCommand::OpenPanel(id.clone()), Origin::Command);
        if self
            .session
            .workspace()
            .main_layout()
            .is_some_and(|m| m.group_of(&id).is_some())
        {
            self.reveal = Some(id.clone());
            self.dismiss_flyout = false;
        }
        self.raise_host_of(&id);
    }
}
