//! Toolkit content drawn over the native preview layer.
//!
//! The preview picture is a native layer above everything the toolkit draws in the
//! window (ADR-0034, ADR-0036), so an in-window popup or toast that overlaps it would be
//! drawn under the picture. The shell hides the layer only while such a cover really
//! overlaps what the layer draws: the picture itself (not the letterbox bars, which
//! the layer leaves transparent) or a selection mark (ADR-0048). A popup beside the
//! picture leaves it alone (ADR-0034 decision 4, UI principle U21 "The picture stays").
//!
//! Pure and toolkit-neutral; all rectangles in the same window coordinates.

use crate::layout::Rect;

/// Whether `a` and `b` share an area larger than zero. A rectangle that is not finite
/// overlaps everything, so a bad cover hides the picture instead of being drawn under it.
pub fn overlap(a: Rect, b: Rect) -> bool {
    let finite = |r: Rect| r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite();
    if !finite(a) || !finite(b) {
        return true;
    }
    a.w > 0.0
        && a.h > 0.0
        && b.w > 0.0
        && b.h > 0.0
        && a.x < b.x + b.w
        && b.x < a.x + a.w
        && a.y < b.y + b.h
        && b.y < a.y + a.h
}

/// Whether any of `covers` overlaps any of `drawn` (what the layer draws: the picture
/// and the marks).
pub fn covered(drawn: &[Rect], covers: &[Rect]) -> bool {
    covers.iter().any(|&c| drawn.iter().any(|&d| overlap(c, d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn beside_is_not_covered() {
        let picture = r(100.0, 100.0, 640.0, 360.0);
        // A colour picker right of the picture, touching its edge.
        assert!(!covered(&[picture], &[r(740.0, 300.0, 280.0, 300.0)]));
        // Above, touching.
        assert!(!covered(&[picture], &[r(100.0, 0.0, 200.0, 100.0)]));
        assert!(!covered(&[picture], &[]));
    }

    #[test]
    fn any_overlap_is_covered() {
        let picture = r(100.0, 100.0, 640.0, 360.0);
        // One pixel into the picture.
        assert!(covered(&[picture], &[r(739.0, 300.0, 280.0, 300.0)]));
        // Inside.
        assert!(covered(&[picture], &[r(200.0, 200.0, 10.0, 10.0)]));
        // Larger than the picture.
        assert!(covered(&[picture], &[r(0.0, 0.0, 2000.0, 2000.0)]));
        // The second of several covers.
        assert!(covered(
            &[picture],
            &[r(0.0, 0.0, 50.0, 50.0), r(300.0, 450.0, 50.0, 50.0)]
        ));
    }

    #[test]
    fn marks_count_as_drawn() {
        let picture = r(100.0, 100.0, 640.0, 360.0);
        // A handle in the letterbox bar left of the picture.
        let handle = r(92.0, 200.0, 8.0, 8.0);
        let popup = r(0.0, 190.0, 95.0, 100.0);
        assert!(!covered(&[picture], &[popup]));
        assert!(covered(&[picture, handle], &[popup]));
    }

    #[test]
    fn empty_rectangles_cover_nothing_and_non_finite_covers_everything() {
        let picture = r(0.0, 0.0, 100.0, 100.0);
        assert!(!covered(&[picture], &[r(10.0, 10.0, 0.0, 50.0)]));
        assert!(!covered(&[r(10.0, 10.0, 0.0, 0.0)], &[picture]));
        assert!(covered(&[picture], &[r(f32::NAN, 0.0, 1.0, 1.0)]));
        assert!(covered(&[picture], &[r(0.0, 0.0, f32::INFINITY, 1.0)]));
    }
}
