//! Selecting, moving and resizing sources on the picture panel (ADR-0048): the pointer
//! and keys become source boxes, sent to the studio one at a time, and the selection
//! becomes marks. The geometry is `rbs_ui::transform`; this keeps the state of one drag
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
}

/// What a pointer or key event asks of the studio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    /// Nothing for the studio.
    None,
    /// Select this source (-1: none).
    Select(i32),
    /// Send this box for this source, as request `seq`; the studio answers it with the
    /// same `seq` whether or not anything changed.
    Send(u64, i32, SourceBox),
}

/// The editing state of the picture panel.
#[derive(Debug, Default)]
pub struct PreviewEdit {
    panel: [f32; 2],
    drag: Option<(i32, Drag)>,
    keys: DragKeys,
    /// Boxes shown for sources while the studio has not caught up with them (one per
    /// source).
    live: Vec<(i32, SourceBox)>,
    /// The request with the studio, if any.
    in_flight: Option<(u64, i32)>,
    /// The newest box per source waiting for the one in flight, oldest source first.
    waiting: Vec<(i32, SourceBox)>,
    next_seq: u64,
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
                self.drag = Some((p.id, Drag::new(grip, at, b)));
                return Effect::None;
            }
        }
        let boxes: Vec<SourceBox> = placed.iter().map(|p| self.current(p)).collect();
        match transform::topmost_at(&view, &boxes, point).and_then(|i| placed.get(i)) {
            Some(p) => {
                self.drag = Some((p.id, Drag::new(Grip::Move, at, self.current(p))));
                Effect::Select(p.id)
            }
            None => {
                self.drag = None;
                Effect::Select(-1)
            }
        }
    }

    /// The pointer moved to `point` with the button down.
    pub fn drag_to(&mut self, point: [f32; 2], canvas: [f32; 2]) -> Effect {
        let view = self.view(canvas);
        let (Some((id, drag)), Some(at)) = (self.drag, view.to_canvas(point)) else {
            return Effect::None;
        };
        let scale = view.scale();
        let snap = if scale > 0.0 {
            SNAP_DISTANCE / scale
        } else {
            0.0
        };
        let b = drag.update(at, canvas, snap, self.keys);
        if self.live_of(id).unwrap_or(drag.from()) == b {
            return Effect::None;
        }
        self.set_live(id, b);
        self.send(id, b)
    }

    /// The button went up or the press was cancelled: the drag ends (its last box is
    /// already on its way).
    pub fn release(&mut self) {
        self.drag = None;
    }

    /// Arrow keys: the selected source moves by `dx`, `dy` canvas pixels.
    pub fn nudge(&mut self, placed: &[Placed], selected: i32, dx: f32, dy: f32) -> Effect {
        let Some(p) = placed.iter().find(|p| p.id == selected) else {
            return Effect::None;
        };
        let b = self.current(p).nudged(dx, dy);
        self.set_live(p.id, b);
        self.send(p.id, b)
    }

    /// At most one box is with the studio; newer ones wait, the newest per source.
    fn send(&mut self, id: i32, b: SourceBox) -> Effect {
        if self.in_flight.is_some() {
            match self.waiting.iter_mut().find(|(i, _)| *i == id) {
                Some(w) => w.1 = b,
                None => self.waiting.push((id, b)),
            }
            return Effect::None;
        }
        self.next_seq += 1;
        self.in_flight = Some((self.next_seq, id));
        Effect::Send(self.next_seq, id, b)
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
            self.waiting.retain(|(i, _)| *i != id);
        }
        let dragged = self.drag.map(|(d, _)| d);
        let waiting = &self.waiting;
        self.live
            .retain(|(i, _)| Some(*i) == dragged || waiting.iter().any(|(w, _)| w == i));
        if self.waiting.is_empty() {
            return Effect::None;
        }
        let (id, b) = self.waiting.remove(0);
        self.send(id, b)
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
            },
            Placed {
                id: 2,
                at: SourceBox::new([1400.0, 700.0], [480.0, 270.0]),
            },
        ]
    }

    fn sent(e: Effect) -> (u64, i32, SourceBox) {
        match e {
            Effect::Send(s, i, b) => (s, i, b),
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
}
