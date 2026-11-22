//! A video source's crop, sent as `item.set_crop` (ADR-0164 step 6): the Properties
//! fields and the picture panel's crop drags. The answer comes back as a ticket, so
//! a drag sends its next crop only after the last one was placed, as the window's
//! own crop command's answer did. A box (`item.set_box`, ADR-0168 step 2) is answered
//! the same way.

use rbs_api::Command as C;
use rbs_api::request as rq;
use rbs_api::{ApiError, Response};
use rbs_app::studio_host::{ApiReply, OutputCanvas};

use super::{StudioUi, VERTICAL_SEQ};

/// Marks a crop's or a box's ticket; the rest is the edit's `seq` (with
/// [`VERTICAL_SEQ`] for the vertical picture). Other tickets count up from 1 and never
/// reach it.
pub(super) const EDIT_TICKET: u64 = 1 << 60;

impl StudioUi {
    /// Crops source `id`'s item on `canvas`; `seq` names the answer.
    pub(super) fn send_crop(
        &mut self,
        seq: u64,
        id: i32,
        crop: [u32; 4],
        gesture: u64,
        canvas: OutputCanvas,
    ) {
        let Some(item) = self.row_item(id) else {
            // The row went: nothing to crop.
            self.crop_placed(seq, false);
            return;
        };
        self.call_as(
            C::ItemSetCrop(rq::SetItemCrop {
                item,
                crop,
                keep_scale: true,
                canvas,
                gesture,
            }),
            ApiReply::Ticket(EDIT_TICKET | seq),
        );
    }

    /// Places a box from the picture panel or the fields; its answer, by `seq`, lets
    /// a drag send the next one, also after a refusal (ADR-0048 decision 5).
    pub(super) fn call_box(&self, call: rq::SetItemBox) {
        let ticket = EDIT_TICKET | call.seq;
        self.call_as(C::ItemSetBox(call), ApiReply::Ticket(ticket));
    }

    /// The answer to [`StudioUi::send_crop`] or [`StudioUi::call_box`] number `seq`:
    /// the next drag step.
    pub(super) fn crop_answer(
        &mut self,
        seq: u64,
        result: Result<Response, ApiError>,
        out: &mut super::Applied,
    ) {
        // A refusal is on the error bar already (`ApiReply::Ticket` shows it).
        let _ = out;
        self.crop_placed(seq, result.is_ok());
    }

    fn crop_placed(&mut self, seq: u64, ok: bool) {
        if seq & VERTICAL_SEQ != 0 {
            let next = self.vedit.answered(seq & !VERTICAL_SEQ, ok);
            self.run_vertical(next);
        } else {
            // Crops from the Properties fields are numbered 0, which the picture
            // panel never uses.
            let next = self.edit.answered(seq, ok);
            self.run(next);
        }
    }
}
