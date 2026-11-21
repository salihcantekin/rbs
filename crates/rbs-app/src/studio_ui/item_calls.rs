//! Item operations of the window, sent as control API commands (ADR-0163 step 2,
//! ADR-0164 step 2): the Sources row's eye, lock and layer buttons and drags, Add
//! existing source, the timed show of an item, Undo and Redo, the Show buttons, Fit
//! box to text and a shape's fill.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::types::OutputCanvas;

use rbs_app::studio_host::{ApiReply, TransformOp};

use super::StudioUi;

impl StudioUi {
    /// The item of source `id` in the scene being edited: the Sources row's.
    fn row_item(&self, id: i32) -> Option<u32> {
        self.rows
            .iter()
            .find(|r| r.id.get() == id as u32)
            .map(|r| r.item.get())
    }

    /// Pause (0), Resume (1) or Restart (2) of a source's scroll on the canvas
    /// Properties edits: live, not an undo step (`item.scroll_*`).
    pub fn scroll_live(&mut self, id: i32, action: i32) {
        let Some(item) = self.row_item(id) else {
            return;
        };
        let at = rq::ScrollItem {
            item,
            canvas: self.props_target(id),
        };
        self.call(match action {
            0 => C::ItemScrollPause(at),
            1 => C::ItemScrollResume(at),
            _ => C::ItemScrollRestart(at),
        });
    }

    pub fn set_source_locked(&mut self, id: i32, locked: bool) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetLocked(rq::SetItemLocked { item, locked }));
        }
    }

    /// Show or hide a source in the program scene (the Sources row's eye button).
    pub fn set_source_visible(&mut self, id: i32, visible: bool) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetVisible(rq::SetItemVisible { item, visible }));
        }
    }

    /// One layer up (drawn over the source above it in the list) or down.
    pub fn move_source(&mut self, id: i32, direction: rq::LayerDirection) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemMove(rq::MoveItem {
                item,
                direction: Some(direction),
                to_item: None,
            }));
        }
    }

    /// Puts source `id` where source `to` is in the layer order (a row dragged onto
    /// another). A place in another scene (a global source) changes nothing.
    pub fn move_source_to(&mut self, id: i32, to: i32) {
        self.deleted.clear();
        if let (Some(item), Some(to)) = (self.row_item(id), self.row_item(to)) {
            self.call_as(
                C::ItemMove(rq::MoveItem {
                    item,
                    direction: None,
                    to_item: Some(to),
                }),
                ApiReply::QuietIf(rbs_api::ErrorKind::InvalidParams),
            );
        }
    }

    /// Shows a part of a video source's picture (Properties' Show buttons): 0 the whole
    /// picture, 1 canvas size centred, 2 left half, 3 right half. Main canvas only.
    pub fn set_source_region(&mut self, id: i32, region: i32) {
        let region = match region {
            0 => rq::ShownRegion::Full,
            1 => rq::ShownRegion::CanvasCentred,
            2 => rq::ShownRegion::LeftHalf,
            3 => rq::ShownRegion::RightHalf,
            _ => return,
        };
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetShownRegion(rq::SetItemShownRegion {
                item,
                region,
                canvas: OutputCanvas::Main,
            }));
        }
    }

    /// Fit box to text.
    pub fn fit_text_box(&mut self, id: i32) {
        if !self.text_settings.contains_key(&id) {
            return;
        }
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemFitTextBox(rq::FitTextBox {
                item,
                canvas: OutputCanvas::Main,
            }));
        }
    }

    /// Turns a colour source's fill on or off. Off leaves the inside empty.
    pub fn set_source_filled(&mut self, id: i32, on: bool) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetFilled(rq::SetItemFilled {
                item,
                on,
                canvas: OutputCanvas::Main,
            }));
        }
    }

    /// Flip, turn, fit or centre a source's picture (ADR-0084), on `canvas`, or
    /// without one on the canvas Properties edits: the item method of `op`.
    pub fn transform_source(&mut self, id: i32, op: TransformOp, canvas: Option<OutputCanvas>) {
        use rbs_scene::{CenterAxis, FitMode};
        self.deleted.clear();
        let Some(item) = self.row_item(id) else {
            return;
        };
        let canvas = canvas.unwrap_or_else(|| self.props_target(id));
        let gesture = 0;
        let flip = |axis| {
            C::ItemFlip(rq::FlipItem {
                item,
                axis,
                canvas,
                gesture,
            })
        };
        self.call(match op {
            TransformOp::FlipHorizontal => flip(rq::Axis::Horizontal),
            TransformOp::FlipVertical => flip(rq::Axis::Vertical),
            TransformOp::Turn(degrees) => C::ItemRotate(rq::RotateItem {
                item,
                degrees: degrees.round() as i32,
                canvas,
                gesture,
            }),
            TransformOp::SetAngle(degrees) => {
                return self.send_rotation(item, degrees, canvas, gesture);
            }
            TransformOp::Fit(mode) => C::ItemFit(rq::FitItem {
                item,
                mode: match mode {
                    FitMode::Fill => rq::FitKind::Fill,
                    FitMode::Stretch => rq::FitKind::Stretch,
                    _ => rq::FitKind::Fit,
                },
                canvas,
                gesture,
            }),
            TransformOp::Center(axis) => C::ItemAlign(rq::AlignItem {
                item,
                axis: Some(match axis {
                    CenterAxis::Horizontal => rq::AlignAxis::Horizontal,
                    CenterAxis::Vertical => rq::AlignAxis::Vertical,
                    _ => rq::AlignAxis::Both,
                }),
                canvas,
                gesture,
            }),
            TransformOp::Reset => C::ItemResetTransform(rq::ResetItemTransform {
                item,
                canvas,
                gesture,
            }),
        });
    }

    /// Sets the picture's rotation in degrees clockwise (Properties' rotation field
    /// and its drag, ADR-0166). Moves that share `gesture` are one undo step.
    pub fn set_source_rotation(&mut self, id: i32, degrees: f32, gesture: i32) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            let canvas = self.props_target(id);
            self.send_rotation(item, degrees, canvas, super::ui_gesture(gesture));
        }
    }

    /// `item.set_rotation`, wrapped into `0..360` first as the field's value always
    /// was; a value that is not finite changes nothing.
    fn send_rotation(&self, item: u32, degrees: f32, canvas: OutputCanvas, gesture: u64) {
        if degrees.is_finite() {
            self.call(C::ItemSetRotation(rq::SetItemRotation {
                item,
                degrees: f64::from(degrees.rem_euclid(360.0)),
                canvas,
                gesture,
            }));
        }
    }

    /// Remove from scene (ADR-0065): the Sources row's remove button and Delete key.
    pub fn remove_from_scene(&mut self, id: i32) {
        self.deleted.clear();
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemRemove(rq::ItemRef { item }));
        }
    }

    /// Places a source of another scene into the scene on air (Add source, Existing
    /// source); it is selected once it is there.
    pub fn add_existing_source(&mut self, id: i32) {
        self.deleted.clear();
        self.adding = true;
        self.call(C::ItemAdd(rq::AddItem {
            source: id as u32,
            scene: None,
        }));
    }

    /// Stored timed-show duration on the item (ADR-0105). `show_for_ms` 0 clears.
    pub fn set_item_show_for(&mut self, item: i32, show_for_ms: i32) {
        let show_for_ms = if show_for_ms <= 0 {
            None
        } else {
            Some((show_for_ms as u32).min(rbs_scene::MAX_SHOW_FOR_MS))
        };
        self.call(C::ItemSetShowFor(rq::SetItemShowFor {
            item: item as u32,
            show_for_ms,
        }));
    }

    /// Live timed show for a hidden item (ADR-0105).
    pub fn show_item(&mut self, item: i32) {
        self.call(C::ItemShow(rq::ShowItem { item: item as u32 }));
    }

    /// Undo (`back`) or redo the newest scene edit (Ctrl/Cmd+Z, the Undo and Redo
    /// buttons). A box on its way is still applied first: the studio takes commands
    /// in order.
    pub fn undo_edit(&mut self, back: bool) {
        self.deleted.clear();
        let call = if back { C::EditUndo } else { C::EditRedo };
        // Nothing to undo says nothing (the buttons are disabled then); a refused
        // step is shown.
        self.call_as(call, ApiReply::QuietIf(rbs_api::ErrorKind::Conflict));
    }
}

impl StudioUi {
    /// `item.set_style` for the row's item on the canvas Properties edits.
    pub(super) fn send_style(&self, id: i32, style: &rbs_scene::model::ItemStyle, gesture: u64) {
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetStyle(rq::SetItemStyle {
                item,
                style: rbs_app::studio_host::style_for_api(style),
                canvas: self.props_target(id),
                gesture,
            }));
        }
    }

    /// `item.set_scroll` for the row's item on the canvas Properties edits; `None`
    /// stops it.
    pub(super) fn send_scroll(
        &self,
        id: i32,
        scroll: Option<&rbs_scene::model::Scroll>,
        gesture: u64,
    ) {
        if let Some(item) = self.row_item(id) {
            self.call(C::ItemSetScroll(rq::SetItemScroll {
                item,
                scroll: scroll.map(rbs_app::studio_host::scroll_for_api),
                canvas: self.props_target(id),
                gesture,
            }));
        }
    }
}
