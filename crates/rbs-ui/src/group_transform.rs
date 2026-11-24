//! Pure geometry for editing several sources at once (multi-selection).
//!
//! Everything here works on [`SourceBox`] values in canvas pixels and allocates only
//! the result vector. It runs on selection and pointer events, never per frame.

use crate::layout::Rect;
use crate::transform::{MIN_SIZE, SourceBox};

/// An edge or centre line the boxes are aligned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignTo {
    /// Left edges meet.
    Left,
    /// Horizontal centres meet.
    CentreX,
    /// Right edges meet.
    Right,
    /// Top edges meet.
    Top,
    /// Vertical centres meet.
    CentreY,
    /// Bottom edges meet.
    Bottom,
}

/// The direction boxes are spread along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

/// The smallest rectangle that covers every box, or `None` for no boxes.
pub fn union(boxes: &[SourceBox]) -> Option<Rect> {
    let mut it = boxes.iter().map(SourceBox::visual);
    let first = it.next()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x + first.w, first.y + first.h);
    for r in it {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.w);
        y1 = y1.max(r.y + r.h);
    }
    Some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Every box moved by `dx`, `dy`.
pub fn moved(boxes: &[SourceBox], dx: f32, dy: f32) -> Vec<SourceBox> {
    boxes.iter().map(|b| b.nudged(dx, dy)).collect()
}

/// Boxes mapped from the union `from` into the rectangle `to`: positions and sizes
/// scale with it, so the relative layout is kept. Flips are kept (the sign of a size
/// does not change). A `to` smaller than [`MIN_SIZE`] on a side is raised to it.
pub fn scaled(boxes: &[SourceBox], from: Rect, to: Rect) -> Vec<SourceBox> {
    let to_w = to.w.max(MIN_SIZE);
    let to_h = to.h.max(MIN_SIZE);
    let sx = if from.w > 0.0 { to_w / from.w } else { 1.0 };
    let sy = if from.h > 0.0 { to_h / from.h } else { 1.0 };
    boxes
        .iter()
        .map(|b| SourceBox {
            position: [
                to.x + (b.position[0] - from.x) * sx,
                to.y + (b.position[1] - from.y) * sy,
            ],
            size: [b.size[0] * sx, b.size[1] * sy],
        })
        .collect()
}

/// The boxes aligned to the union of all of them (one box stays where it is).
pub fn align(boxes: &[SourceBox], to: AlignTo) -> Vec<SourceBox> {
    let Some(u) = union(boxes) else {
        return Vec::new();
    };
    boxes
        .iter()
        .map(|b| {
            let v = b.visual();
            let (dx, dy) = match to {
                AlignTo::Left => (u.x - v.x, 0.0),
                AlignTo::CentreX => (u.x + u.w / 2.0 - (v.x + v.w / 2.0), 0.0),
                AlignTo::Right => (u.x + u.w - (v.x + v.w), 0.0),
                AlignTo::Top => (0.0, u.y - v.y),
                AlignTo::CentreY => (0.0, u.y + u.h / 2.0 - (v.y + v.h / 2.0)),
                AlignTo::Bottom => (0.0, u.y + u.h - (v.y + v.h)),
            };
            b.nudged(dx, dy)
        })
        .collect()
}

/// The boxes spread so the gaps between neighbours along `axis` are equal. The two
/// outermost boxes stay; fewer than three boxes come back unchanged. The result is in
/// the order of the input.
pub fn distribute(boxes: &[SourceBox], axis: Axis) -> Vec<SourceBox> {
    let n = boxes.len();
    if n < 3 {
        return boxes.to_vec();
    }
    let (start, len): (fn(&Rect) -> f32, fn(&Rect) -> f32) = match axis {
        Axis::Horizontal => (|r| r.x, |r| r.w),
        Axis::Vertical => (|r| r.y, |r| r.h),
    };
    let rects: Vec<Rect> = boxes.iter().map(SourceBox::visual).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| start(&rects[a]).total_cmp(&start(&rects[b])));
    let first = start(&rects[order[0]]);
    let last = &rects[order[n - 1]];
    let end = start(last) + len(last);
    let total: f32 = rects.iter().map(len).sum();
    let gap = (end - first - total) / (n - 1) as f32;
    let mut out = boxes.to_vec();
    let mut at = first;
    for &i in &order {
        let d = at - start(&rects[i]);
        out[i] = match axis {
            Axis::Horizontal => boxes[i].nudged(d, 0.0),
            Axis::Vertical => boxes[i].nudged(0.0, d),
        };
        at += len(&rects[i]) + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x: f32, y: f32, w: f32, h: f32) -> SourceBox {
        SourceBox::new([x, y], [w, h])
    }

    #[test]
    fn union_covers_flipped_boxes() {
        let u = union(&[b(0.0, 0.0, 10.0, 10.0), b(50.0, 40.0, -20.0, -10.0)]).unwrap();
        assert_eq!((u.x, u.y, u.w, u.h), (0.0, 0.0, 50.0, 40.0));
        assert!(union(&[]).is_none());
    }

    #[test]
    fn scaling_keeps_relative_layout_and_flips() {
        let boxes = [b(0.0, 0.0, 10.0, 10.0), b(10.0, 0.0, -10.0, 10.0)];
        let from = union(&boxes).unwrap();
        let out = scaled(
            &boxes,
            from,
            Rect {
                x: 100.0,
                y: 0.0,
                w: 40.0,
                h: 20.0,
            },
        );
        assert_eq!(out[0], b(100.0, 0.0, 40.0, 20.0));
        assert_eq!(out[1].size, [-40.0, 20.0]);
        assert_eq!(out[1].position, [140.0, 0.0]);
    }

    #[test]
    fn scaling_never_goes_below_the_minimum() {
        let boxes = [b(0.0, 0.0, 100.0, 100.0)];
        let out = scaled(
            &boxes,
            union(&boxes).unwrap(),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        );
        assert!(out[0].size[0] >= MIN_SIZE && out[0].size[1] >= MIN_SIZE);
    }

    #[test]
    fn align_moves_to_the_union_edge_or_centre() {
        let boxes = [b(0.0, 0.0, 10.0, 10.0), b(30.0, 20.0, 20.0, 10.0)];
        assert_eq!(align(&boxes, AlignTo::Left)[1].position, [0.0, 20.0]);
        assert_eq!(align(&boxes, AlignTo::Right)[0].position, [40.0, 0.0]);
        assert_eq!(align(&boxes, AlignTo::CentreX)[0].position, [20.0, 0.0]);
        assert_eq!(align(&boxes, AlignTo::Bottom)[0].position, [0.0, 20.0]);
        assert_eq!(align(&boxes, AlignTo::CentreY)[0].position, [0.0, 10.0]);
        assert_eq!(align(&boxes, AlignTo::Top)[1].position, [30.0, 0.0]);
    }

    #[test]
    fn distribute_makes_equal_gaps_and_keeps_input_order() {
        // Given out of order: the box at 90 is listed second.
        let boxes = [
            b(0.0, 0.0, 10.0, 10.0),
            b(90.0, 0.0, 10.0, 10.0),
            b(20.0, 0.0, 30.0, 10.0),
        ];
        let out = distribute(&boxes, Axis::Horizontal);
        assert_eq!(out[0].position[0], 0.0);
        assert_eq!(out[1].position[0], 90.0);
        // Gap = (100 - 50) / 2 = 25, so the middle box starts at 10 + 25.
        assert_eq!(out[2].position[0], 35.0);
    }

    #[test]
    fn distribute_needs_three_boxes() {
        let boxes = [b(0.0, 0.0, 10.0, 10.0), b(50.0, 0.0, 10.0, 10.0)];
        assert_eq!(distribute(&boxes, Axis::Vertical), boxes.to_vec());
    }
}
