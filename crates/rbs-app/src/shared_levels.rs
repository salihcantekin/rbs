//! Turning a scene's Own levels off from a window asks first (ADR-0091 decision 5):
//! a small dialog over the main window that says what goes, not a confirm inside the
//! scene's row. Its "Use shared levels" puts the scene back in Shared levels, one undo
//! step, and the undo toast that follows can bring the levels back.

use slint::ComponentHandle;

use super::{App, Ask, Overlays, window_place};

impl App {
    /// Opens the Shared levels confirmation for scene `id`. Cancel is focused, so
    /// Return or Escape keeps the scene's own levels.
    pub(crate) fn ask_shared_levels(&mut self, id: i32) {
        let name = self.studio.scene_name(id).unwrap_or_default();
        let o = self.main.global::<Overlays>();
        o.set_name_error("".into());
        o.set_name_value("".into());
        o.set_name_title(format!("Stop using own levels in {name}?").into());
        o.set_name_detail(
            "The volumes and mutes set only for this scene are removed. Its sources play \
             at their shared levels again. You can undo this."
                .into(),
        );
        o.set_name_confirm("Use shared levels".into());
        o.set_name_cancel("Keep own levels".into());
        o.set_name_ask(false);
        o.set_name_open(true);
        window_place::raise(self.main.window());
        self.name_for = Some(Ask::SharedLevels(id));
    }
}
