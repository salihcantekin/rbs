//! Align and distribute several items at once (protocol 1.106, ADR-0182): one undo
//! step made of the members' layout edits, written on the canvas the call names.

use std::collections::BTreeSet;

use rbs_api::request::{AlignEdge, AlignItems, AlignTo, Axis, DistributeItems};
use rbs_api::types::OutputCanvas;
use rbs_scene::model::{ItemContent, ItemId};
use rbs_ui::group_transform::{self, AlignTo as To, Axis as Along};
use rbs_ui::layout::Rect;
use rbs_ui::transform::SourceBox;

use super::{CropRefusal, EditKind, Empty, Host, Refusal, Response, Update};

/// The most items one call names.
const MAX_ITEMS: usize = 1000;

/// The box that covers `layout` as drawn: its centre, with the width and height of the
/// rotated rectangle's axis-aligned bounds. Flips do not change it.
fn drawn_box(position: [f32; 2], size: [f32; 2], rotation: f32) -> SourceBox {
    let (w, h) = (size[0].abs(), size[1].abs());
    let (sin, cos) = rotation.to_radians().sin_cos();
    let (dw, dh) = (w * cos.abs() + h * sin.abs(), w * sin.abs() + h * cos.abs());
    let (cx, cy) = (position[0] + size[0] / 2.0, position[1] + size[1] / 2.0);
    SourceBox::new([cx - dw / 2.0, cy - dh / 2.0], [dw, dh])
}

impl Host {
    /// `item.align_items`.
    pub(super) fn api_align_items(
        &mut self,
        p: AlignItems,
        notify: &dyn Fn(Update),
    ) -> Result<Response, super::ApiError> {
        let edge = match p.edge {
            AlignEdge::Left => To::Left,
            AlignEdge::Center => To::CentreX,
            AlignEdge::Right => To::Right,
            AlignEdge::Top => To::Top,
            AlignEdge::Middle => To::CentreY,
            AlignEdge::Bottom => To::Bottom,
        };
        let to_canvas = p.to == AlignTo::Canvas;
        if p.items.is_empty() || (p.items.len() < 2 && !to_canvas) {
            return self.arrange_refused(Refusal::Invalid(
                "Aligning to the selection needs at least two items, and to the canvas one.".into(),
            ));
        }
        let moved = self.arrange(&p.items, p.canvas, p.gesture, |boxes, canvas| {
            if to_canvas {
                group_transform::align_within(boxes, edge, canvas)
            } else {
                group_transform::align(boxes, edge)
            }
        });
        self.arrange_answer(moved, notify)
    }

    /// `item.distribute`.
    pub(super) fn api_distribute(
        &mut self,
        p: DistributeItems,
        notify: &dyn Fn(Update),
    ) -> Result<Response, super::ApiError> {
        if p.items.len() < 3 {
            return self.arrange_refused(Refusal::Invalid(
                "Distributing needs at least three items.".into(),
            ));
        }
        let along = match p.axis {
            Axis::Horizontal => Along::Horizontal,
            Axis::Vertical => Along::Vertical,
        };
        let moved = self.arrange(&p.items, p.canvas, p.gesture, |boxes, _| {
            group_transform::distribute(boxes, along)
        });
        self.arrange_answer(moved, notify)
    }

    fn arrange_refused(&self, e: Refusal) -> Result<Response, super::ApiError> {
        Err(super::refused(&e))
    }

    fn arrange_answer(
        &mut self,
        moved: Result<bool, Refusal>,
        notify: &dyn Fn(Update),
    ) -> Result<Response, super::ApiError> {
        if moved.map_err(|e| super::refused(&e))? {
            self.apply(notify);
        }
        Ok(Response::Done(Empty {}))
    }

    /// Moves `items` to the boxes `place` returns for their current ones (in canvas
    /// pixels, with the canvas rectangle) as one undo step; all or nothing. `Ok(false)`
    /// when nothing moves.
    fn arrange(
        &mut self,
        items: &[u32],
        on: OutputCanvas,
        gesture: u64,
        place: impl FnOnce(&[SourceBox], Rect) -> Vec<SourceBox>,
    ) -> Result<bool, Refusal> {
        if items.len() > MAX_ITEMS {
            return Err(Refusal::Invalid(format!(
                "At most {MAX_ITEMS} items can be arranged at once."
            )));
        }
        let distinct: BTreeSet<u32> = items.iter().copied().collect();
        if distinct.len() != items.len() {
            return Err(Refusal::Invalid("An item is named twice.".into()));
        }
        let target = self.edit_canvas(on)?;
        let (w, h) = self
            .canvas_px(target)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let shown = match on {
            OutputCanvas::Main => Some(self.edited()),
            OutputCanvas::Vertical => self.vertical_shown(),
        };
        let mut found = Vec::with_capacity(items.len());
        for id in items {
            let item = ItemId::from_raw(*id);
            let (scene, entry) = self
                .collection
                .item(item)
                .ok_or_else(|| Refusal::NotFound(format!("There is no item {id}.")))?;
            let global = on == OutputCanvas::Main && self.globals() == Some(scene);
            if Some(scene) != shown && !global {
                return Err(Refusal::Invalid(format!(
                    "Item {id} is not in the scene this canvas edits."
                )));
            }
            if !matches!(entry.content, ItemContent::Source(_)) {
                return Err(Refusal::Invalid(
                    "Align and distribute are for source items, not nested scenes.".into(),
                ));
            }
            if !self.has_picture(entry) {
                return Err(Refusal::Invalid(format!("Item {id} has no picture.")));
            }
            if entry.locked {
                return Err(Refusal::Conflict(
                    "An item is locked. Unlock it to move it.".into(),
                ));
            }
            let (source, layout, own) = self.crop_target_on(item, on).map_err(|e| match e {
                CropRefusal::NotFound(t) => Refusal::NotFound(t),
                CropRefusal::Invalid(t) => Refusal::Invalid(t),
                CropRefusal::Conflict(t) => Refusal::Conflict(t),
                CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
            })?;
            found.push((item, source, layout, own));
        }
        let boxes: Vec<SourceBox> = found
            .iter()
            .map(|(_, _, l, _)| drawn_box(l.position, l.size, l.rotation))
            .collect();
        let canvas = Rect {
            x: 0.0,
            y: 0.0,
            w: w as f32,
            h: h as f32,
        };
        let placed = place(&boxes, canvas);
        let mut undos = Vec::new();
        for (((item, _, old, own), before), b) in found.iter().zip(&boxes).zip(&placed) {
            let delta = [
                b.position[0] - before.position[0],
                b.position[1] - before.position[1],
            ];
            if delta[0].abs() < 1e-3 && delta[1].abs() < 1e-3 {
                continue;
            }
            let mut layout = *old;
            layout.position = [old.position[0] + delta[0], old.position[1] + delta[1]];
            match self.save_live_layout(*item, *own, layout) {
                Ok(u) => undos.push(u),
                Err(e) => {
                    // Put back what was written: the step is all or nothing.
                    while let Some(u) = undos.pop() {
                        if self.collection.undo(u).is_err() {
                            rbs_core::warn!(
                                rbs_core::log::Area::Studio,
                                "an arrange step could not be put back"
                            );
                        }
                    }
                    return Err(Refusal::of(&e));
                }
            }
        }
        if undos.is_empty() {
            return Ok(false);
        }
        let name = self.name_of(found[0].1);
        self.edits
            .history
            .record(EditKind::Place, &name, gesture, undos);
        Ok(true)
    }
}
