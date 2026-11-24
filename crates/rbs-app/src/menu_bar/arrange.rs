//! The Transform entries and Group / Ungroup (ADR-0084, ADR-0182): whether they can run
//! on the selected source or sources, and what they run.

use rbs_api::request::{AlignEdge, Axis};
use rbs_ui::command::{CommandId, Disabled};

use crate::studio_ui::multi_select::ArrangeOp;

use super::{Run, Shown, canvas_of};
use crate::studio_host::TransformOp;

impl Shown<'_> {
    /// Whether a transform, Group or Ungroup entry can run now.
    pub(super) fn arrange_enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        self.running()?;
        match id.base() {
            "item.group" => return self.studio.group_items().map(|_| ()).map_err(Into::into),
            "item.ungroup" => return self.studio.ungroup_target().map(|_| ()).map_err(Into::into),
            "item.align_items" => {
                return self
                    .studio
                    .arrange_items(ArrangeOp::Align(AlignEdge::Left))
                    .map(|_| ())
                    .map_err(Into::into);
            }
            "item.distribute" => {
                return self
                    .studio
                    .arrange_items(ArrangeOp::Distribute(Axis::Horizontal))
                    .map(|_| ())
                    .map_err(Into::into);
            }
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
        "item.group" => Run::Arrange(ArrangeOp::Group(true)),
        "item.ungroup" => Run::Arrange(ArrangeOp::Group(false)),
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

/// Whether `base` is an Arrange command this file answers.
pub(super) fn is_arrange(base: &str) -> bool {
    matches!(
        base,
        "item.flip"
            | "item.rotate"
            | "item.align"
            | "item.fit"
            | "item.reset_transform"
            | "item.group"
            | "item.ungroup"
            | "item.align_items"
            | "item.distribute"
    )
}

/// What a picker entry of Arrange runs for the chosen `value`.
pub(super) fn picked(id: &CommandId, value: &str, state: &Shown<'_>) -> Option<Run> {
    use rbs_scene::{CenterAxis, FitMode};
    Some(match id.base() {
        "item.rotate" => Run::Transform(
            state.source(id).ok()?.id,
            TransformOp::Turn(value.parse::<i32>().ok()? as f32),
            canvas_of(id)?,
        ),
        "item.fit" => Run::Transform(
            state.source(id).ok()?.id,
            TransformOp::Fit(match value {
                "fit" => FitMode::Fit,
                "fill" => FitMode::Fill,
                "stretch" => FitMode::Stretch,
                _ => return None,
            }),
            canvas_of(id)?,
        ),
        "item.align_items" => Run::Arrange(ArrangeOp::Align(match value {
            "left" => AlignEdge::Left,
            "center" => AlignEdge::Center,
            "right" => AlignEdge::Right,
            "top" => AlignEdge::Top,
            "middle" => AlignEdge::Middle,
            "bottom" => AlignEdge::Bottom,
            _ => return None,
        })),
        "item.distribute" => Run::Arrange(ArrangeOp::Distribute(match value {
            "horizontal" => rbs_api::request::Axis::Horizontal,
            "vertical" => rbs_api::request::Axis::Vertical,
            _ => return None,
        })),
        "item.align" => Run::Transform(
            state.source(id).ok()?.id,
            TransformOp::Center(match value {
                "both" => CenterAxis::Both,
                "horizontal" => CenterAxis::Horizontal,
                "vertical" => CenterAxis::Vertical,
                _ => return None,
            }),
            canvas_of(id)?,
        ),
        _ => return None,
    })
}
