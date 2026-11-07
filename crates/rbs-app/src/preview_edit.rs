//! Selecting, moving, resizing and cropping sources on the picture panel (ADR-0048,
//! ADR-0058): the pointer and keys become source boxes or crops, sent to the studio one
//! at a time, and the selection becomes marks. The geometry is `rbs_ui::transform`; this keeps the state of one drag
//! and of the boxes on their way to the studio.
//!
//! UI thread only. Coordinates are the panel's, in logical pixels.

use rbs_ui::layout::Rect;
use rbs_ui::transform::{
    self, Drag, DragKeys, Grip, MAX_MARKS, Mark, SNAP_DISTANCE, SourceBox, Viewport,
};

/// A source with a picture as the panel sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// Its id.
    pub id: i32,
    /// Its box on the canvas.
    pub at: SourceBox,
    /// Source pixels cut from the left, top, right and bottom.
    pub crop: [u32; 4],
    /// Its picture's size in pixels, once known (a crop drag needs it).
    pub picture: Option<(u32, u32)>,
}

/// What goes to the studio for one source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edit {
    /// A new box.
    Box(SourceBox),
    /// A new crop (left, top, right, bottom), keeping the scale.
    Crop([u32; 4]),
}

/// A crop drag's start: the crop and the picture's size.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cropping {
    crop: [u32; 4],
    picture: (u32, u32),
}

/// What a pointer or key event asks of the studio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    /// Nothing for the studio.
    None,
    /// Select this source (-1: none).
    Select(i32),
    /// Send this box for this source, as request `seq`; the studio answers it with the
    /// same `seq` whether or not anything changed. The last number is the gesture: the
    /// boxes of one drag share one, so they are one undo step; 0 for a box of its own
    /// (arrow keys).
    Send(u64, i32, SourceBox, u64),
    /// Send this crop for this source, as request `seq`, like [`Effect::Send`].
    SendCrop(u64, i32, [u32; 4], u64),
}

/// The editing state of the picture panel.
#[derive(Debug, Default)]
pub struct PreviewEdit {
    panel: [f32; 2],
    drag: Option<(i32, Drag, Option<Cropping>)>,
    keys: DragKeys,
    /// Boxes shown for sources while the studio has not caught up with them (one per
    /// source).
    live: Vec<(i32, SourceBox)>,
    /// The request with the studio, if any.
    in_flight: Option<(u64, i32)>,
    /// The newest edit per source waiting for the one in flight, oldest source first,
    /// with its gesture.
    waiting: Vec<(i32, Edit, u64)>,
    next_seq: u64,
    /// The current drag's gesture number (see [`Effect::Send`]).
    gesture: u64,
    next_gesture: u64,
}

impl PreviewEdit {
    /// The panel's size (from the layout, or from the last press).
    pub fn set_panel(&mut self, w: f32, h: f32) {
        self.panel = [w, h];
    }

    /// Forgets every drag and every box on its way (the studio stopped or started: no
    /// answer will come for what was sent).
    pub fn reset(&mut self) {
        self.drag = None;
        self.live.clear();
        self.in_flight = None;
        self.waiting.clear();
    }

    fn view(&self, canvas: [f32; 2]) -> Viewport {
        Viewport {
            area: Rect::sized(self.panel[0], self.panel[1]),
            canvas,
        }
    }

    fn live_of(&self, id: i32) -> Option<SourceBox> {
        self.live.iter().find(|(i, _)| *i == id).map(|(_, b)| *b)
    }

    fn set_live(&mut self, id: i32, b: SourceBox) {
        match self.live.iter_mut().find(|(i, _)| *i == id) {
            Some(l) => l.1 = b,
            None => self.live.push((id, b)),
        }
    }

    /// The box to show for `p`: the one being dragged or sent, else the studio's.
    fn current(&self, p: &Placed) -> SourceBox {
        self.live_of(p.id).unwrap_or(p.at)
    }

    /// A press at `point`: a handle or the inside of the selected source starts a drag
    /// of it; elsewhere the topmost source under the pointer is selected and dragged;
    /// outside every source the selection clears. `placed` is bottom first.
    pub fn press(
        &mut self,
        point: [f32; 2],
        canvas: [f32; 2],
        placed: &[Placed],
        selected: i32,
        keys: DragKeys,
    ) -> Effect {
        let view = self.view(canvas);
        let Some(at) = view.to_canvas(point) else {
            return Effect::None;
        };
        self.keys = keys;
        if let Some(p) = placed.iter().find(|p| p.id == selected) {
            let b = self.current(p);
            if let Some(grip) = transform::grip_at(&view, &b, point) {
                // Alt on a handle crops, once the picture's size is known.
                let cropping = match (grip, p.picture) {
                    (Grip::Handle(_), Some(picture)) if keys.crop => Some(Cropping {
                        crop: p.crop,
                        picture,
                    }),
                    _ => None,
                };
                if cropping.is_some() {
                    // Each edge on its own: corners keep no aspect ratio.
                    self.keys.free = true;
                }
                self.drag = Some((p.id, Drag::new(grip, at, b), cropping));
                self.new_gesture();
                return Effect::None;
            }
        }
        let boxes: Vec<SourceBox> = placed.iter().map(|p| self.current(p)).collect();
        match transform::topmost_at(&view, &boxes, point).and_then(|i| placed.get(i)) {
            Some(p) => {
                self.drag = Some((p.id, Drag::new(Grip::Move, at, self.current(p)), None));
                self.new_gesture();
                Effect::Select(p.id)
            }
            None => {
                self.drag = None;
                Effect::Select(-1)
            }
        }
    }

    /// The topmost source whose picture is under `point` (a right-click: nothing is
    /// pressed or dragged).
    pub fn source_at(&self, point: [f32; 2], canvas: [f32; 2], placed: &[Placed]) -> Option<i32> {
        let view = self.view(canvas);
        view.to_canvas(point)?;
        let boxes: Vec<SourceBox> = placed.iter().map(|p| self.current(p)).collect();
        transform::topmost_at(&view, &boxes, point)
            .and_then(|i| placed.get(i))
            .map(|p| p.id)
    }

    /// The pointer moved to `point` with the button down.
    pub fn drag_to(&mut self, point: [f32; 2], canvas: [f32; 2]) -> Effect {
        let view = self.view(canvas);
        let (Some((id, drag, cropping)), Some(at)) = (self.drag, view.to_canvas(point)) else {
            return Effect::None;
        };
        let scale = view.scale();
        let snap = if scale > 0.0 {
            SNAP_DISTANCE / scale
        } else {
            0.0
        };
        let b = drag.update(at, canvas, snap, self.keys);
        let (b, edit) = match cropping {
            None => (b, Edit::Box(b)),
            Some(c) => match transform::crop_for(drag.from(), c.crop, c.picture, b) {
                Some((crop, shown)) => (shown, Edit::Crop(crop)),
                None => return Effect::None,
            },
        };
        if self.live_of(id).unwrap_or(drag.from()) == b {
            return Effect::None;
        }
        self.set_live(id, b);
        self.send(id, edit, self.gesture)
    }

    fn new_gesture(&mut self) {
        self.next_gesture += 1;
        self.gesture = self.next_gesture;
    }

    /// The button went up or the press was cancelled: the drag ends (its last box is
    /// already on its way).
    pub fn release(&mut self) {
        if let Some((id, ..)) = self.drag.take() {
            // Nothing more on its way for it: the studio's box shows again (it is the
            // drag's last box, or what an undo made of it since).
            let busy = self.in_flight.is_some_and(|(_, i)| i == id)
                || self.waiting.iter().any(|(w, ..)| *w == id);
            if !busy {
                self.live.retain(|(i, _)| *i != id);
            }
        }
    }

    /// Arrow keys: the selected source moves by `dx`, `dy` canvas pixels.
    pub fn nudge(&mut self, placed: &[Placed], selected: i32, dx: f32, dy: f32) -> Effect {
        let Some(p) = placed.iter().find(|p| p.id == selected) else {
            return Effect::None;
        };
        let b = self.current(p).nudged(dx, dy);
        self.set_live(p.id, b);
        self.send(p.id, Edit::Box(b), 0)
    }

    /// At most one edit is with the studio; newer ones wait, the newest per source.
    fn send(&mut self, id: i32, edit: Edit, gesture: u64) -> Effect {
        if self.in_flight.is_some() {
            match self.waiting.iter_mut().find(|(i, ..)| *i == id) {
                Some(w) => {
                    w.1 = edit;
                    w.2 = gesture;
                }
                None => self.waiting.push((id, edit, gesture)),
            }
            return Effect::None;
        }
        self.next_seq += 1;
        self.in_flight = Some((self.next_seq, id));
        match edit {
            Edit::Box(b) => Effect::Send(self.next_seq, id, b, gesture),
            Edit::Crop(c) => Effect::SendCrop(self.next_seq, id, c, gesture),
        }
    }

    /// The studio answered request `seq` (`ok`: it took the box or found it unchanged;
    /// else it refused it). Answers to other requests are ignored. The next waiting box
    /// goes; a source with nothing more on its way shows the studio's box again, unless
    /// it is being dragged. A refused box drops what waits for that source.
    pub fn answered(&mut self, seq: u64, ok: bool) -> Effect {
        let Some((sent, id)) = self.in_flight else {
            return Effect::None;
        };
        if sent != seq {
            return Effect::None;
        }
        self.in_flight = None;
        if !ok {
            self.waiting.retain(|(i, ..)| *i != id);
        }
        let dragged = self.drag.map(|(d, ..)| d);
        let waiting = &self.waiting;
        self.live
            .retain(|(i, _)| Some(*i) == dragged || waiting.iter().any(|(w, ..)| w == i));
        if self.waiting.is_empty() {
            return Effect::None;
        }
        let (id, edit, gesture) = self.waiting.remove(0);
        self.send(id, edit, gesture)
    }

    /// The selection of `selected` as marks in panel coordinates, written into `out`.
    pub fn marks(&self, canvas: [f32; 2], placed: &[Placed], selected: i32, out: &mut Vec<Mark>) {
        out.clear();
        out.reserve(MAX_MARKS);
        if let Some(p) = placed.iter().find(|p| p.id == selected) {
            transform::marks(&self.view(canvas), &self.current(p), true, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANVAS: [f32; 2] = [1920.0, 1080.0];

    fn edit() -> PreviewEdit {
        let mut e = PreviewEdit::default();
        // Scale 0.5: the 1920 x 1080 canvas fills a 960 x 540 panel.
        e.set_panel(960.0, 540.0);
        e
    }

    fn scene() -> Vec<Placed> {
        vec![
            Placed {
                id: 1,
                at: SourceBox::new([0.0, 0.0], [1920.0, 1080.0]),
                crop: [0; 4],
                picture: Some((3840, 2160)),
            },
            Placed {
                id: 2,
                at: SourceBox::new([1400.0, 700.0], [480.0, 270.0]),
                crop: [0; 4],
                picture: None,
            },
        ]
    }

    fn sent(e: Effect) -> (u64, i32, SourceBox) {
        match e {
            Effect::Send(s, i, b, _) => (s, i, b),
            other => panic!("expected a send, got {other:?}"),
        }
    }

    #[test]
    fn a_press_selects_the_topmost_source_and_a_drag_moves_it() {
        let mut e = edit();
        let s = scene();
        // On the camera (top).
        let pick = e.press([800.0, 400.0], CANVAS, &s, -1, DragKeys::default());
        assert_eq!(pick, Effect::Select(2));
        let (_, id, b) = sent(e.drag_to([750.0, 400.0], CANVAS));
        assert_eq!(
            (id, b),
            (2, SourceBox::new([1300.0, 700.0], [480.0, 270.0]))
        );
        // Outside every source: nothing is selected.
        let mut e = edit();
        let only = [s[1]];
        assert_eq!(
            e.press([10.0, 10.0], CANVAS, &only, 2, DragKeys::default()),
            Effect::Select(-1)
        );
    }

    fn gesture_of(e: Effect) -> u64 {
        match e {
            Effect::Send(.., g) => g,
            other => panic!("expected a send, got {other:?}"),
        }
    }

    #[test]
    fn the_boxes_of_one_drag_share_a_gesture_and_keys_have_none() {
        let mut e = edit();
        let s = scene();
        e.press([800.0, 400.0], CANVAS, &s, 2, DragKeys::default());
        let first = e.drag_to([790.0, 400.0], CANVAS);
        let g = gesture_of(first);
        assert_ne!(g, 0);
        let (seq, ..) = sent(first);
        e.drag_to([780.0, 400.0], CANVAS);
        // The waiting box keeps the drag's gesture.
        let next = e.answered(seq, true);
        assert_eq!(gesture_of(next), g);
        let (seq, ..) = sent(next);
        e.release();
        e.answered(seq, true);
        // A new press is a new gesture.
        e.press([700.0, 400.0], CANVAS, &s, 2, DragKeys::default());
        let other = e.drag_to([690.0, 400.0], CANVAS);
        let g2 = gesture_of(other);
        assert!(g2 != 0 && g2 != g);
        let (seq, ..) = sent(other);
        e.release();
        e.answered(seq, true);
        // Arrow keys: each box is its own step.
        assert_eq!(gesture_of(e.nudge(&s, 2, 1.0, 0.0)), 0);
    }

    #[test]
    fn a_release_after_the_last_answer_shows_the_studios_box() {
        let mut e = edit();
        let s = scene();
        e.press([800.0, 400.0], CANVAS, &s, 2, DragKeys::default());
        let (seq, ..) = sent(e.drag_to([790.0, 400.0], CANVAS));
        // Answered while the button is still down: the dragged box stays shown.
        assert_eq!(e.answered(seq, true), Effect::None);
        assert_ne!(e.current(&s[1]), s[1].at);
        // After the release the studio's box shows, so an undo is seen at once.
        e.release();
        assert_eq!(e.current(&s[1]), s[1].at);
    }

    #[test]
    fn one_box_at_a_time_goes_to_the_studio_and_the_newest_waits() {
        let mut e = edit();
        let s = scene();
        e.press([800.0, 400.0], CANVAS, &s, 2, DragKeys::default());
        let (first, _, _) = sent(e.drag_to([790.0, 400.0], CANVAS));
        assert_eq!(e.drag_to([780.0, 400.0], CANVAS), Effect::None);
        assert_eq!(e.drag_to([770.0, 400.0], CANVAS), Effect::None);
        // An answer to something else changes nothing.
        assert_eq!(e.answered(first + 100, true), Effect::None);
        // Only the newest box goes next.
        let (second, id, b) = sent(e.answered(first, true));
        assert_eq!(
            (id, b),
            (2, SourceBox::new([1340.0, 700.0], [480.0, 270.0]))
        );
        e.release();
        assert_eq!(e.answered(second, true), Effect::None);
        // Caught up: the studio's box shows again.
        assert_eq!(e.current(&s[1]), s[1].at);
    }

    #[test]
    fn a_box_the_studio_already_has_is_answered_and_nothing_stays_in_flight() {
        let mut e = edit();
        let s = scene();
        e.press([800.0, 400.0], CANVAS, &s, 2, DragKeys::default());
        let (a, _, box_a) = sent(e.drag_to([790.0, 400.0], CANVAS));
        // Away and back while A is with the studio: A waits again.
        e.drag_to([700.0, 400.0], CANVAS);
        e.drag_to([790.0, 400.0], CANVAS);
        let (again, _, b) = sent(e.answered(a, true));
        assert_eq!(b, box_a);
        // The studio answers the unchanged box too, so the next one goes at once.
        assert_eq!(e.answered(again, true), Effect::None);
        assert!(matches!(
            e.drag_to([600.0, 400.0], CANVAS),
            Effect::Send(..)
        ));
    }

    #[test]
    fn switching_source_keeps_the_first_sources_last_box() {
        let mut e = edit();
        let s = scene();
        let (first, _, _) = sent(e.nudge(&s, 2, 10.0, 0.0));
        e.nudge(&s, 2, 10.0, 0.0);
        e.nudge(&s, 1, 0.0, 5.0);
        let (second, id, b) = sent(e.answered(first, true));
        assert_eq!((id, b.position), (2, [1420.0, 700.0]));
        let (_, id, b) = sent(e.answered(second, true));
        assert_eq!((id, b.position), (1, [0.0, 5.0]));
    }

    #[test]
    fn a_refused_box_drops_what_waits_and_a_reset_forgets_everything() {
        let mut e = edit();
        let s = scene();
        let (seq, _, _) = sent(e.nudge(&s, 2, 10.0, 0.0));
        assert_eq!(e.nudge(&s, 2, 10.0, 0.0), Effect::None);
        assert_eq!(e.answered(seq, false), Effect::None);
        assert_eq!(e.current(&s[1]), s[1].at);
        // A send with no answer (the studio stopped): after a reset the next one goes.
        sent(e.nudge(&s, 2, 10.0, 0.0));
        e.reset();
        assert!(matches!(e.nudge(&s, 2, 10.0, 0.0), Effect::Send(..)));
        assert_eq!(e.nudge(&s, -1, 1.0, 0.0), Effect::None);
    }

    #[test]
    fn marks_follow_the_shown_box() {
        let mut e = edit();
        let s = scene();
        let mut out = Vec::new();
        e.marks(CANVAS, &s, 2, &mut out);
        assert_eq!(out.len(), MAX_MARKS);
        assert_eq!(out[0].rect.x, 700.0);
        e.nudge(&s, 2, 20.0, 0.0);
        e.marks(CANVAS, &s, 2, &mut out);
        assert_eq!(out[0].rect.x, 710.0);
        e.marks(CANVAS, &s, -1, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn alt_on_a_handle_crops_at_the_picture_scale_and_answers_share_the_flow() {
        let mut e = edit();
        let s = scene();
        let alt = DragKeys {
            crop: true,
            ..DragKeys::default()
        };
        // The display (3840 x 2160 at half size on the canvas): its left edge handle is
        // at panel (0, 270); 100 panel px is 200 canvas px, 400 source px.
        assert_eq!(e.press([0.0, 270.0], CANVAS, &s, 1, alt), Effect::None);
        let Effect::SendCrop(seq, 1, crop, gesture) = e.drag_to([100.0, 270.0], CANVAS) else {
            panic!("expected a crop");
        };
        assert_eq!(crop, [400, 0, 0, 0]);
        assert_ne!(gesture, 0);
        // The marks show the cropped box at once.
        let mut out = Vec::new();
        e.marks(CANVAS, &s, 1, &mut out);
        assert_eq!(out[0].rect.x, 100.0);
        // The next one waits for the answer, then goes with the same gesture.
        assert_eq!(e.drag_to([150.0, 270.0], CANVAS), Effect::None);
        let Effect::SendCrop(_, 1, crop, g) = e.answered(seq, true) else {
            panic!("expected the waiting crop");
        };
        assert_eq!((crop, g), ([600, 0, 0, 0], gesture));
        e.release();

        // Without a known picture size, Alt resizes as before.
        e.reset();
        e.press([700.0, 350.0], CANVAS, &s, 2, alt);
        assert!(matches!(
            e.drag_to([650.0, 350.0], CANVAS),
            Effect::Send(..)
        ));
    }
}
