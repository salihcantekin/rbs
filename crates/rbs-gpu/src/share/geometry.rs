//! Platform-neutral arithmetic of the preview layers (ADR-0034, ADR-0036), kept apart so
//! it is tested on every platform.

use std::time::Duration;

/// Where an image of `image` pixels goes when it is fitted into `rect` (x, y, width,
/// height) keeping its aspect ratio and centred: `(x, y, scale)`, in `rect`'s units.
/// A degenerate image or rectangle gives scale 0 at the rectangle's centre.
#[cfg_attr(
    target_os = "macos",
    allow(dead_code, reason = "the macOS layer lets Core Animation fit")
)]
pub(crate) fn fit(rect: (f64, f64, f64, f64), image: (u32, u32)) -> (f64, f64, f64) {
    let (x, y, w, h) = rect;
    let (iw, ih) = (f64::from(image.0), f64::from(image.1));
    let (w, h) = (w.max(0.0), h.max(0.0));
    let scale = if iw > 0.0 && ih > 0.0 {
        (w / iw).min(h / ih)
    } else {
        0.0
    };
    (
        x + (w - iw * scale) / 2.0,
        y + (h - ih * scale) / 2.0,
        scale,
    )
}

/// Surfaces an engine ring needs so that it always finds a free one while a layer shows
/// frames `frame_interval` apart and holds each replaced frame up to `hold`: the shown
/// frame, the replaced ones within `hold`, one being written and one in flight.
pub(crate) fn ring_needed(frame_interval: Duration, hold: Duration) -> usize {
    let interval = frame_interval.as_nanos().max(1);
    let retiring = hold.as_nanos().div_ceil(interval);
    usize::try_from(retiring)
        .unwrap_or(usize::MAX)
        .saturating_add(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes_and_pillarboxes() {
        // 16:9 image into a wider rectangle: pillarbox, height fills.
        let (x, y, s) = fit((10.0, 20.0, 400.0, 90.0), (160, 90));
        assert_eq!((x, y, s), (10.0 + 120.0, 20.0, 1.0));
        // Into a taller rectangle: letterbox, width fills.
        let (x, y, s) = fit((0.0, 0.0, 320.0, 400.0), (160, 90));
        assert_eq!((x, y, s), (0.0, (400.0 - 180.0) / 2.0, 2.0));
        // Exact fit.
        assert_eq!(fit((5.0, 6.0, 160.0, 90.0), (160, 90)), (5.0, 6.0, 1.0));
    }

    #[test]
    fn fit_handles_degenerate_sizes() {
        assert_eq!(fit((0.0, 0.0, 100.0, 50.0), (0, 90)).2, 0.0);
        let (x, y, s) = fit((0.0, 0.0, -5.0, 50.0), (160, 90));
        assert_eq!(s, 0.0);
        assert_eq!((x, y), (0.0, 25.0));
    }

    #[test]
    fn ring_covers_held_frames() {
        let f60 = Duration::from_nanos(16_666_667);
        // macOS: 34 ms retire time at 60 fps -> 3 retiring + 3 = 6 (ADR-0034).
        assert_eq!(ring_needed(f60, Duration::from_millis(34)), 6);
        // Windows: one refresh plus the 4 ms poll -> 2 retiring + 3 = 5 (ADR-0036).
        assert_eq!(ring_needed(f60, f60 + Duration::from_millis(4)), 5);
        // Nothing held: shown, written, in flight.
        assert_eq!(ring_needed(f60, Duration::ZERO), 3);
        // A zero interval does not divide by zero.
        assert!(ring_needed(Duration::ZERO, Duration::from_millis(1)) > 3);
    }
}
