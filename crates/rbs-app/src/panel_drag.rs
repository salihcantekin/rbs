//! Where a dragged panel would go, shown while the pointer is over a dock area
//! (ADR-0049 decision 5, ADR-0197).

use rbs_ui::frame::{DropTarget, drop_target};
use rbs_ui::panel::PanelId;

use crate::{App, view};

impl App {
    /// Where panel `panel` dragged to `(x, y)` in window `window`'s dock area would go.
    pub(crate) fn drop_at(
        &self,
        window: usize,
        panel: &PanelId,
        x: f32,
        y: f32,
    ) -> Option<DropTarget> {
        let area = self.area(window);
        let w = self.session.workspace().windows.get(window)?;
        let flat = view::shown_flat(w, area, self.maximized.as_ref(), self.studio.studio_mode);
        drop_target(&flat, area, panel, x, y)
    }

    /// A tab is dragged over window `window`'s dock area: show where it would go.
    pub(crate) fn drag_over(&mut self, window: usize, panel: Option<PanelId>, x: f32, y: f32) {
        let guide = panel
            .and_then(|p| self.drop_at(window, &p, x, y))
            .map(|t| (window, t.guide));
        self.show_drop_guide(guide);
    }
}
