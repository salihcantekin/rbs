//! Side areas that run the full height of the main window (ADR-0197).

use super::{Side, Window, Workspace, WorkspaceError};
use crate::layout::Edge;
use crate::panel::PanelId;

impl Workspace {
    /// Make the left or right area of the main window run the full height of the window
    /// (or the usual height beside the bottom area again): ADR-0197.
    pub fn set_area_full(&mut self, side: Side, full: bool) -> Result<(), WorkspaceError> {
        if side == Side::Bottom {
            return Err(WorkspaceError::Invalid(
                "Only the left and right areas can run the full height".into(),
            ));
        }
        let m = self
            .windows
            .first_mut()
            .and_then(Window::main_mut)
            .ok_or(WorkspaceError::NoTarget)?;
        m.area_mut(side).full_height = full;
        Ok(())
    }

    /// Move a panel to the left or right edge of the main window as an area that runs
    /// the full height of the window (ADR-0197).
    pub fn move_to_full_edge(&mut self, id: &PanelId, edge: Edge) -> Result<(), WorkspaceError> {
        let side = match edge {
            Edge::Left => Side::Left,
            Edge::Right => Side::Right,
            Edge::Top | Edge::Bottom => {
                return Err(WorkspaceError::Invalid(
                    "Only the left and right edges can run the full height".into(),
                ));
            }
        };
        self.move_to_edge(id, 0, edge)?;
        self.set_area_full(side, true)
    }
}
