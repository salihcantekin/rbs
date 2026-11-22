//! Desktop to picture mapping maths of the ScreenCaptureKit adapter (ADR-0165
//! decision 5, capture-port.md "Picture mapping").
//!
//! The target's frame on the desktop (a display's `CGDisplayBounds`, a window's
//! `kCGWindowBounds`, both in global points with the origin top left of the main
//! display) and where ScreenCaptureKit put the content in the output buffer (the
//! frame's `contentRect` in output points times its `scaleFactor`) give an affine map
//! `picture = (desktop - origin) * scale`. Platform independent so it is tested
//! everywhere.

use rbs_capture::Cover;

use crate::cursor::{self, Above, Rect};

/// Where ScreenCaptureKit placed the content in one output buffer, from the frame's
/// `SCStreamFrameInfo`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Content {
    /// `contentRect`: the content's rectangle in the output, in output points.
    pub rect: Rect,
    /// `scaleFactor`: output pixels per output point.
    pub scale_factor: f64,
}

fn positive(v: f64) -> bool {
    v.is_finite() && v > 0.0
}

/// The mapping of a target whose desktop frame is `target` (points) into a picture of
/// `picture` pixels, `(origin, scale)`. Without `content` the content is taken to fill
/// the whole buffer. `None` when a size is empty or not finite.
pub(crate) fn picture_mapping(
    target: Rect,
    content: Option<Content>,
    picture: (u32, u32),
) -> Option<([f64; 2], [f64; 2])> {
    if !positive(target.width) || !positive(target.height) || picture.0 == 0 || picture.1 == 0 {
        return None;
    }
    // The content's rectangle in buffer pixels.
    let (px, py, pw, ph) = match content {
        Some(c)
            if positive(c.scale_factor) && positive(c.rect.width) && positive(c.rect.height) =>
        {
            (
                c.rect.x * c.scale_factor,
                c.rect.y * c.scale_factor,
                c.rect.width * c.scale_factor,
                c.rect.height * c.scale_factor,
            )
        }
        _ => (0.0, 0.0, f64::from(picture.0), f64::from(picture.1)),
    };
    let scale = [pw / target.width, ph / target.height];
    let origin = [target.x - px / scale[0], target.y - py / scale[1]];
    (origin.iter().chain(&scale).all(|v| v.is_finite())).then_some((origin, scale))
}

/// The covered test of a window capture (capture-port.md rule 4): `Covered` while the
/// pointer is outside the window's frame or a window stacked above it is under the
/// pointer, `Clear` otherwise, `Unknown` without a pointer.
pub(crate) fn window_cover(
    point: Option<(f64, f64)>,
    target: Rect,
    above: impl IntoIterator<Item = Above>,
) -> Cover {
    match point {
        None => Cover::Unknown,
        Some((x, y)) if !target.contains(x, y) || cursor::covered(x, y, above) => Cover::Covered,
        Some(_) => Cover::Clear,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn map(m: ([f64; 2], [f64; 2]), p: [f64; 2]) -> [f64; 2] {
        let (origin, scale) = m;
        [(p[0] - origin[0]) * scale[0], (p[1] - origin[1]) * scale[1]]
    }

    #[test]
    fn retina_display_at_native_size() {
        // A 1512 x 982 point display at 2x, captured at 3024 x 1964.
        let display = rect(0.0, 0.0, 1512.0, 982.0);
        let content = Content {
            rect: rect(0.0, 0.0, 1512.0, 982.0),
            scale_factor: 2.0,
        };
        let m = picture_mapping(display, Some(content), (3024, 1964)).expect("mapping");
        assert_eq!(m, ([0.0, 0.0], [2.0, 2.0]));
        assert_eq!(map(m, [756.0, 491.0]), [1512.0, 982.0]);
    }

    #[test]
    fn display_left_of_the_main_one() {
        // Global points are negative left of the main display.
        let display = rect(-1920.0, 0.0, 1920.0, 1080.0);
        let m = picture_mapping(display, None, (1920, 1080)).expect("mapping");
        assert_eq!(map(m, [-1920.0, 0.0]), [0.0, 0.0]);
        assert_eq!(map(m, [-960.0, 540.0]), [960.0, 540.0]);
    }

    #[test]
    fn output_scaled_to_fit_uses_the_buffer() {
        // A 2x display delivered into a half-size buffer: the content rect shrinks.
        let display = rect(0.0, 0.0, 1512.0, 982.0);
        let content = Content {
            rect: rect(0.0, 0.0, 756.0, 491.0),
            scale_factor: 2.0,
        };
        let m = picture_mapping(display, Some(content), (1512, 982)).expect("mapping");
        assert_eq!(m.1, [1.0, 1.0]);
        assert_eq!(map(m, [1512.0, 982.0]), [1512.0, 982.0]);
    }

    #[test]
    fn letterboxed_window_content_is_offset() {
        // A 400 x 300 point window at 2x, placed 20 points in from the left of a wider
        // buffer (a resize not yet followed).
        let window = rect(100.0, 50.0, 400.0, 300.0);
        let content = Content {
            rect: rect(20.0, 0.0, 400.0, 300.0),
            scale_factor: 2.0,
        };
        let m = picture_mapping(window, Some(content), (880, 600)).expect("mapping");
        assert_eq!(map(m, [100.0, 50.0]), [40.0, 0.0]);
        assert_eq!(map(m, [500.0, 350.0]), [840.0, 600.0]);
    }

    #[test]
    fn empty_sizes_map_nothing() {
        let some = rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            picture_mapping(rect(0.0, 0.0, 0.0, 10.0), None, (10, 10)),
            None
        );
        assert_eq!(picture_mapping(some, None, (0, 10)), None);
        assert_eq!(
            picture_mapping(rect(0.0, 0.0, f64::NAN, 10.0), None, (10, 10)),
            None
        );
        // A broken content rect falls back to the whole buffer.
        let broken = Content {
            rect: rect(0.0, 0.0, 0.0, 0.0),
            scale_factor: 2.0,
        };
        assert_eq!(
            picture_mapping(some, Some(broken), (20, 20)),
            Some(([0.0, 0.0], [2.0, 2.0]))
        );
    }

    #[test]
    fn cover_of_a_window() {
        let window = rect(0.0, 0.0, 100.0, 100.0);
        let above = Above {
            bounds: rect(50.0, 50.0, 100.0, 100.0),
            alpha: 1.0,
            layer: 0,
        };
        assert_eq!(window_cover(None, window, [above]), Cover::Unknown);
        assert_eq!(
            window_cover(Some((10.0, 10.0)), window, [above]),
            Cover::Clear
        );
        assert_eq!(
            window_cover(Some((60.0, 60.0)), window, [above]),
            Cover::Covered
        );
        assert_eq!(
            window_cover(Some((200.0, 10.0)), window, [above]),
            Cover::Covered
        );
    }
}
