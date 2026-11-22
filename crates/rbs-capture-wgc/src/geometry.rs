//! Desktop to picture mapping maths of the Windows Graphics Capture adapter (ADR-0165
//! decision 5, capture-port.md "Picture mapping").
//!
//! The target's rectangle on the desktop (a monitor's `rcMonitor`, a window's DWM
//! extended frame bounds, both read per-monitor DPI aware v2, so physical pixels of
//! the virtual screen) and the size of the frame WGC delivered give an affine map
//! `picture = (desktop - origin) * scale`. WGC puts the content at the buffer's top
//! left at its own size, so there is no offset to fold in. Platform independent so it
//! is tested everywhere.

use rbs_capture::Cover;

/// A desktop rectangle in physical pixels, right and bottom exclusive (a `RECT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    fn width(self) -> i64 {
        i64::from(self.right) - i64::from(self.left)
    }

    fn height(self) -> i64 {
        i64::from(self.bottom) - i64::from(self.top)
    }

    pub(crate) fn contains(self, (x, y): (i32, i32)) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// The mapping of a target at `target` on the desktop into a frame of `picture`
/// pixels, `(origin, scale)`. `None` when either is empty.
pub(crate) fn picture_mapping(target: Rect, picture: (u32, u32)) -> Option<([f64; 2], [f64; 2])> {
    let (w, h) = (target.width(), target.height());
    if w <= 0 || h <= 0 || picture.0 == 0 || picture.1 == 0 {
        return None;
    }
    let scale = [
        f64::from(picture.0) / w as f64,
        f64::from(picture.1) / h as f64,
    ];
    Some(([f64::from(target.left), f64::from(target.top)], scale))
}

/// What the watcher saw of a window under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Under {
    /// The top-level window at the pointer (`WindowFromPoint`, then
    /// `GetAncestor(GA_ROOT)`) is the target.
    Target,
    /// Another window is there.
    Other,
}

/// The covered test of a window capture (capture-port.md rule 4): `Covered` while the
/// pointer is outside the window's frame, the window is minimised or another window is
/// on top at the pointer; `Clear` otherwise; `Unknown` without a pointer.
pub(crate) fn window_cover(
    point: Option<(i32, i32)>,
    frame: Rect,
    minimised: bool,
    under: Under,
) -> Cover {
    match point {
        None => Cover::Unknown,
        Some(p) if minimised || !frame.contains(p) || under == Under::Other => Cover::Covered,
        Some(_) => Cover::Clear,
    }
}

/// [`Cover`] as one byte, for the watcher's atomic: the newest covered test reaches
/// `latest_mapping` without a new frame (capture-port.md rule 4).
pub(crate) fn cover_bits(cover: Cover) -> u8 {
    match cover {
        Cover::Clear => 1,
        Cover::Covered => 2,
        _ => 0,
    }
}

/// The inverse of [`cover_bits`].
pub(crate) fn cover_of(bits: u8) -> Cover {
    match bits {
        1 => Cover::Clear,
        2 => Cover::Covered,
        _ => Cover::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn map(m: ([f64; 2], [f64; 2]), p: [f64; 2]) -> [f64; 2] {
        let (origin, scale) = m;
        [(p[0] - origin[0]) * scale[0], (p[1] - origin[1]) * scale[1]]
    }

    #[test]
    fn primary_monitor_maps_one_to_one() {
        let m = picture_mapping(rect(0, 0, 3840, 2160), (3840, 2160)).unwrap();
        assert_eq!(m, ([0.0, 0.0], [1.0, 1.0]));
        assert_eq!(map(m, [1920.0, 1080.0]), [1920.0, 1080.0]);
    }

    #[test]
    fn monitor_left_of_the_primary_has_a_negative_origin() {
        let m = picture_mapping(rect(-2560, -200, 0, 1240), (2560, 1440)).unwrap();
        assert_eq!(map(m, [-2560.0, -200.0]), [0.0, 0.0]);
        assert_eq!(map(m, [-1280.0, 520.0]), [1280.0, 720.0]);
    }

    #[test]
    fn a_window_frame_maps_its_corners() {
        let frame = rect(100, 50, 900, 650);
        let m = picture_mapping(frame, (800, 600)).unwrap();
        assert_eq!(map(m, [100.0, 50.0]), [0.0, 0.0]);
        assert_eq!(map(m, [900.0, 650.0]), [800.0, 600.0]);
    }

    #[test]
    fn a_frame_that_lags_a_resize_scales() {
        // The frame is from before the window grew to 1600 wide.
        let m = picture_mapping(rect(0, 0, 800, 600), (1600, 600)).unwrap();
        assert_eq!(m.1, [2.0, 1.0]);
    }

    #[test]
    fn empty_sizes_give_no_mapping() {
        assert!(picture_mapping(rect(0, 0, 0, 10), (10, 10)).is_none());
        assert!(picture_mapping(rect(10, 0, 0, 10), (10, 10)).is_none());
        assert!(picture_mapping(rect(0, 0, 10, 10), (0, 10)).is_none());
    }

    #[test]
    fn covered_test() {
        let frame = rect(0, 0, 100, 100);
        assert_eq!(
            window_cover(None, frame, false, Under::Target),
            Cover::Unknown
        );
        assert_eq!(
            window_cover(Some((50, 50)), frame, false, Under::Target),
            Cover::Clear
        );
        assert_eq!(
            window_cover(Some((50, 50)), frame, false, Under::Other),
            Cover::Covered
        );
        assert_eq!(
            window_cover(Some((50, 50)), frame, true, Under::Target),
            Cover::Covered
        );
        // Right and bottom are exclusive.
        assert_eq!(
            window_cover(Some((100, 50)), frame, false, Under::Target),
            Cover::Covered
        );
    }

    #[test]
    fn cover_bits_round_trip() {
        for cover in [Cover::Unknown, Cover::Clear, Cover::Covered] {
            assert_eq!(cover_of(cover_bits(cover)), cover);
        }
    }
}
