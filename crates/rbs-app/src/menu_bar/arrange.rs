//! The Transform entries and Group / Ungroup (ADR-0084, ADR-0182): whether they can run
//! on the selected source or sources, and what they run.

use rbs_ui::command::{CommandId, Disabled};

use super::{Run, Shown, canvas_of};
use crate::studio_host::TransformOp;

impl Shown<'_> {
    /// Whether a transform, Group or Ungroup entry can run now.
    pub(super) fn arrange_enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        self.running()?;
        match id.base() {
            "item.group" => return self.studio.group_items().map(|_| ()).map_err(Into::into),
            "item.ungroup" => return self.studio.ungroup_target().map(|_| ()).map_err(Into::into),
            _ => {}
        }
        if id.param("canvas") == Some("vertical") {
            self.vertical_edited()?;
        }
        let f = self.source(id)?;
        if !f.picture {
            return Err("The source has no picture.".into());
        }
        if f.locked {
            return Err("Unlock the source to change its transform.".into());
        }
        if matches!(id.base(), "item.fit" | "item.reset_transform") && !f.sized {
            return Err("The source has no picture yet.".into());
        }
        Ok(())
    }
}

/// What a flip, reset, Group or Ungroup entry runs.
pub(super) fn run(id: &CommandId, state: &Shown<'_>) -> Option<Run> {
    Some(match id.base() {
        "item.group" => Run::Group(true),
        "item.ungroup" => Run::Group(false),
        "item.flip" => Run::Transform(
            state.source(id).ok()?.id,
            match id.param("axis")? {
                "horizontal" => TransformOp::FlipHorizontal,
                "vertical" => TransformOp::FlipVertical,
                _ => return None,
            },
            canvas_of(id)?,
        ),
        _ => Run::Transform(
            state.source(id).ok()?.id,
            TransformOp::Reset,
            canvas_of(id)?,
        ),
    })
}
