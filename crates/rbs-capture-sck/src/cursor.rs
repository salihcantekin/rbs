//! Cursor of window captures: drawn only while the captured window is the one the
//! pointer is over (capture-port.md, ScreenCaptureKit adapter notes).
//!
//! ScreenCaptureKit draws the cursor into a window capture whenever the pointer is
//! inside the window's frame, even when another window covers that spot, so the
//! picture shows a second cursor over content the user cannot see. While a window
//! capture with the cursor is Active, a watcher thread of the session polls the
//! pointer and the windows stacked above the captured one, and turns the stream's
//! cursor off while one of them is under the pointer.
//!
//! The decision is platform independent so it is tested everywhere.

use std::time::Duration;

/// How often the watcher looks at the pointer. A covered cursor shows for at most
/// about this long plus ScreenCaptureKit's reconfiguration time.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const POLL: Duration = Duration::from_millis(50);

/// Polls in a row that may skip the window list while the pointer stands still. A
/// window raised over a still pointer (Cmd-Tab) is noticed within this many polls.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const STILL_POLLS: u32 = 6;

/// Window level of the Dock (`kCGDockWindowLevel`). The Dock's window spans the whole
/// display, opaque by its alpha, though only the Dock itself is drawn, so it would
/// cover every pointer position. It is ignored: over the visible Dock the captured
/// cursor can still show.
pub(crate) const DOCK_LAYER: i64 = 20;

/// A rectangle in global display points, origin top left (Quartz window bounds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// An on-screen window stacked above the captured one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Above {
    pub bounds: Rect,
    /// Window alpha; fully transparent windows hide nothing.
    pub alpha: f64,
    /// Window level (`kCGWindowLayer`).
    pub layer: i64,
}

/// Whether a window above the captured one is under the pointer at `(x, y)`, so the
/// cursor ScreenCaptureKit would draw into the capture is not over visible content.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn covered(x: f64, y: f64, above: impl IntoIterator<Item = Above>) -> bool {
    above
        .into_iter()
        .any(|w| w.alpha > 0.0 && w.layer != DOCK_LAYER && w.bounds.contains(x, y))
}

/// When the watcher reads the window list: whenever the pointer moved, and at least
/// every [`STILL_POLLS`] polls while it stands still.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn should_query(moved: bool, still_polls: u32) -> bool {
    moved || still_polls >= STILL_POLLS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(x: f64, y: f64, width: f64, height: f64) -> Above {
        Above {
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            alpha: 1.0,
            layer: 0,
        }
    }

    #[test]
    fn nothing_above_never_covers() {
        assert!(!covered(10.0, 10.0, []));
    }

    #[test]
    fn a_window_under_the_pointer_covers() {
        let above = [
            window(0.0, 0.0, 100.0, 100.0),
            window(500.0, 0.0, 50.0, 50.0),
        ];
        assert!(covered(50.0, 50.0, above));
        assert!(covered(510.0, 49.5, above));
        assert!(!covered(200.0, 50.0, above));
    }

    #[test]
    fn edges_are_half_open() {
        let above = [window(0.0, 0.0, 100.0, 100.0)];
        assert!(covered(0.0, 0.0, above));
        assert!(!covered(100.0, 50.0, above));
        assert!(!covered(50.0, 100.0, above));
        assert!(!covered(-0.5, 50.0, above));
    }

    #[test]
    fn transparent_windows_do_not_cover() {
        let mut clear = window(0.0, 0.0, 100.0, 100.0);
        clear.alpha = 0.0;
        assert!(!covered(50.0, 50.0, [clear]));
    }

    #[test]
    fn the_full_screen_dock_window_does_not_cover() {
        // As seen on macOS 26: the Dock's window spans the display above all normal
        // windows, alpha 1.
        let mut dock = window(0.0, 0.0, 1800.0, 1169.0);
        dock.layer = DOCK_LAYER;
        let mut menu_bar = window(0.0, 0.0, 1800.0, 39.0);
        menu_bar.layer = 24;
        assert!(!covered(986.0, 1032.0, [menu_bar, dock]));
        assert!(covered(900.0, 10.0, [menu_bar, dock]));
    }

    #[test]
    fn negative_coordinates_of_left_displays_work() {
        let above = [window(-1920.0, 0.0, 800.0, 600.0)];
        assert!(covered(-1500.0, 300.0, above));
        assert!(!covered(-100.0, 300.0, above));
    }

    #[test]
    fn still_pointer_queries_periodically() {
        assert!(should_query(true, 0));
        assert!(!should_query(false, 0));
        assert!(!should_query(false, STILL_POLLS - 1));
        assert!(should_query(false, STILL_POLLS));
    }
}
