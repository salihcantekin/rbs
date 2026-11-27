//! The panel commands of the panel menu (ADR-0049, ADR-0197): whether each can run on the
//! tab stack that holds the panel, and what Collapse, Float and Full window height run.

use rbs_ui::LayoutCommand as L;
use rbs_ui::command::{CommandId, Disabled};
use rbs_ui::workspace::Side;

use super::registry::panel_of;
use super::{Run, Shown};

impl Shown<'_> {
    /// Whether a panel command can run now.
    pub(super) fn panel_enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        let Some(c) = self.panel_caps(id) else {
            return Err("The panel is not open.".into());
        };
        match id.base() {
            // The lock stops what rearranges; collapse and auto-hide still work.
            "ui.panel.move" | "ui.panel.float" | "ui.panel.close" | "ui.panel.full_height"
                if self.workspace.locked =>
            {
                Err("The layout is locked.".into())
            }
            "ui.panel.collapse" if !c.can_collapse => {
                Err("Preview and Program, and the last open stack of the centre, stay open.".into())
            }
            "ui.panel.full_height"
                if !matches!(self.panel_side(id), Some(Side::Left | Side::Right)) =>
            {
                Err("Only a panel in the left or right area can run the full height.".into())
            }
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

    /// The area of the main window that holds the panel, if it is docked in one.
    fn panel_side(&self, id: &CommandId) -> Option<Side> {
        let panel = panel_of(id)?;
        let (window, ..) = self.workspace.find(&panel)?;
        if window != 0 {
            return None;
        }
        let m = self.workspace.main_layout()?;
        Side::ALL.into_iter().find(|s| {
            m.area(*s)
                .layout
                .as_ref()
                .is_some_and(|l| l.panels().iter().any(|p| **p == panel))
        })
    }

    /// Does the side area that holds the panel run the full height?
    pub(super) fn panel_full_height(&self, id: &CommandId) -> bool {
        self.panel_side(id)
            .and_then(|s| self.workspace.main_layout().map(|m| m.area(s).full_height))
            .unwrap_or(false)
    }
}

/// What Collapse, Float and Full window height run; `on` is the entry's checked state.
pub(super) fn run(state: &Shown<'_>, id: &CommandId, on: bool) -> Option<Run> {
    let panel = panel_of(id)?;
    Some(Run::Layout(match id.base() {
        "ui.panel.collapse" => L::SetCollapsed {
            panel,
            collapsed: !on,
        },
        // Floating a floating panel docks its window back.
        "ui.panel.float" if on => L::Dock {
            window: state.workspace.find(&panel)?.0,
        },
        "ui.panel.float" => L::Float(panel),
        "ui.panel.full_height" => L::SetAreaFullHeight {
            side: state.panel_side(id)?,
            full: !on,
        },
        _ => return None,
    }))
}
