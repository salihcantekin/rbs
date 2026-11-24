//! The toolbar row as the title bar (ADR-0071 on Windows, ADR-0180 on macOS): what a
//! point of the window is and what a double-click does, as pure functions. `win::caption`
//! asks them for every `WM_NCHITTEST` and `WM_NCCALCSIZE`, the macOS frame for every
//! press in the row; they hold no platform code, so their tests run everywhere.

/// Where the toolbar row's parts are, in logical pixels from the client area's
/// top-left corner (the UI sends them whenever one moves).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Regions {
    /// Where the leading group ends.
    pub lead_end: f32,
    /// Where the end group starts (the window buttons follow it).
    pub end_start: f32,
    /// The maximize button's left edge.
    pub max_x: f32,
    /// The maximize button's width.
    pub max_w: f32,
    /// The row's height; 0 until the UI has sent the regions.
    pub row_h: f32,
}

/// What a point is, as the OS is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The UI's: it gets the mouse.
    Client,
    /// Moves the window; a double-click maximizes it; a right-click opens the system
    /// menu.
    Caption,
    /// The maximize button: Windows 11 shows its snap layouts there.
    MaxButton,
    /// The top resize edge.
    Top,
    /// The top-left resize corner.
    TopLeft,
    /// The top-right resize corner.
    TopRight,
}

/// One point to test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Probe {
    /// The point's x, in logical pixels from the client area's left edge.
    pub x: f32,
    /// The point's y, in logical pixels from the client area's top edge.
    pub y: f32,
    /// The client area's width, in logical pixels.
    pub width: f32,
    /// The top resize band's height, in logical pixels (the OS's frame thickness).
    pub frame: f32,
    /// The window is maximized: no top resize band.
    pub maximized: bool,
    /// A popup is open in the window: a click must reach it (and close it), never start
    /// a drag.
    pub popup_open: bool,
}

/// Whether a press at `(x, y)` is in the row's flexible space, which moves the window
/// (macOS, ADR-0180 decision 3; on Windows `hit` answers `Caption` there). Nothing is
/// until the UI has sent the regions.
pub fn in_drag_space(x: f32, y: f32, r: Regions) -> bool {
    r.row_h > 0.0 && y >= 0.0 && y < r.row_h && x >= r.lead_end && x < r.end_start
}

/// What a double-click in the row's flexible space does on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoubleClick {
    /// Zoom (macOS's maximize).
    Zoom,
    /// Minimize to the Dock.
    Minimize,
    /// Nothing.
    Nothing,
}

/// The system setting "Double-click a window's title bar to" (ADR-0180 decision 3):
/// `action` is `AppleActionOnDoubleClick` ("Maximize", "Minimize", "None", "Fill"),
/// `miniaturize` the older `AppleMiniaturizeOnDoubleClick`, used when `action` is
/// missing. Anything else zooms, as macOS does by default.
pub fn double_click(action: Option<&str>, miniaturize: Option<bool>) -> DoubleClick {
    match action {
        Some("Minimize") => DoubleClick::Minimize,
        Some("None") => DoubleClick::Nothing,
        Some(_) => DoubleClick::Zoom,
        None if miniaturize == Some(true) => DoubleClick::Minimize,
        None => DoubleClick::Zoom,
    }
}

/// What `p` is (ADR-0071 decision 2, in its order).
pub fn hit(p: Probe, r: Regions) -> Hit {
    if !p.maximized && p.y >= 0.0 && p.y < p.frame {
        return if p.x < p.frame {
            Hit::TopLeft
        } else if p.x >= p.width - p.frame {
            Hit::TopRight
        } else {
            Hit::Top
        };
    }
    if p.popup_open || r.row_h <= 0.0 || p.y < 0.0 || p.y >= r.row_h {
        return Hit::Client;
    }
    if p.x >= r.max_x && p.x < r.max_x + r.max_w {
        return Hit::MaxButton;
    }
    if p.x < r.lead_end || p.x >= r.end_start {
        return Hit::Client;
    }
    Hit::Caption
}

/// A point of the screen, in physical pixels, as a point of the client area in logical
/// pixels: `origin` is the client area's top-left corner on the screen and `scale` the
/// window's scale factor (physical pixels per logical pixel).
pub fn to_client(screen: (i32, i32), origin: (i32, i32), scale: f32) -> (f32, f32) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    (
        (screen.0 - origin.0) as f32 / scale,
        (screen.1 - origin.1) as f32 / scale,
    )
}

/// The client area's top edge for the window rectangle's `top`, in physical pixels: the
/// caption is gone, so the client area starts at the window's top; a maximized window
/// reaches `frame` pixels past the screen, so its client area starts that far in.
pub fn client_top(top: i32, maximized: bool, frame: i32) -> i32 {
    if maximized { top + frame } else { top }
}

/// One `WM_WINDOWPOSCHANGING` of the main window, as far as its size is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeChange {
    /// The window keeps its position (`SWP_NOMOVE`).
    pub keeps_position: bool,
    /// The window keeps its size (`SWP_NOSIZE`).
    pub keeps_size: bool,
    /// The user is moving or sizing the window (between `WM_ENTERSIZEMOVE` and
    /// `WM_EXITSIZEMOVE`): the OS computes that size from the real frame.
    pub user_sizing: bool,
    /// The window is maximized or minimized: the OS sets that size.
    pub zoomed_or_iconic: bool,
}

/// How many physical pixels a size change asked for is too tall.
///
/// winit turns a requested client size into a window size with `AdjustWindowRectEx`
/// for the window's style, which still has the caption, so it adds the caption and the
/// top frame (`top_inset`). With the caption removed the client area starts at the
/// window's top (`client_top`), so that window would be `top_inset` taller inside than
/// asked. winit asks for the current client size again whenever the minimum or maximum
/// size changes (Slint sends them on every layout change, a density change among them),
/// and without this correction the window grew by `top_inset` each time.
///
/// Only a program's resize of a window in its normal state is corrected: a pure size
/// change, outside the user's own move or size loop. Maximize, restore, snap, a DPI
/// change and the user's drags all move the window or run inside that loop.
pub fn excess_height(c: SizeChange, top_inset: i32) -> i32 {
    if c.keeps_size || !c.keeps_position || c.user_sizing || c.zoomed_or_iconic {
        0
    } else {
        top_inset.max(0)
    }
}

/// The window height for `height` asked by winit, without the caption it counted
/// (see [`excess_height`]); never below one pixel.
pub fn corrected_height(height: i32, excess: i32) -> i32 {
    (height - excess).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: Regions = Regions {
        lead_end: 400.0,
        end_start: 900.0,
        max_x: 1254.0,
        max_w: 46.0,
        row_h: 37.0,
    };

    fn at(x: f32, y: f32) -> Probe {
        Probe {
            x,
            y,
            width: 1346.0,
            frame: 8.0,
            maximized: false,
            popup_open: false,
        }
    }

    #[test]
    fn the_top_band_resizes_unless_maximized() {
        assert_eq!(hit(at(3.0, 2.0), ROW), Hit::TopLeft);
        assert_eq!(hit(at(600.0, 2.0), ROW), Hit::Top);
        assert_eq!(hit(at(1340.0, 2.0), ROW), Hit::TopRight);
        let p = Probe {
            maximized: true,
            ..at(600.0, 2.0)
        };
        assert_eq!(hit(p, ROW), Hit::Caption);
    }

    #[test]
    fn the_row_is_caption_only_between_its_groups() {
        assert_eq!(hit(at(100.0, 20.0), ROW), Hit::Client);
        assert_eq!(hit(at(400.0, 20.0), ROW), Hit::Caption);
        assert_eq!(hit(at(899.0, 20.0), ROW), Hit::Caption);
        assert_eq!(hit(at(900.0, 20.0), ROW), Hit::Client);
        assert_eq!(hit(at(1260.0, 20.0), ROW), Hit::MaxButton);
        assert_eq!(hit(at(1310.0, 20.0), ROW), Hit::Client, "close");
        assert_eq!(hit(at(600.0, 37.0), ROW), Hit::Client, "below the row");
    }

    #[test]
    fn popups_and_unsent_regions_keep_the_mouse_in_the_ui() {
        let p = Probe {
            popup_open: true,
            ..at(600.0, 20.0)
        };
        assert_eq!(hit(p, ROW), Hit::Client);
        assert_eq!(hit(at(3.0, 2.0), ROW), Hit::TopLeft, "resizing still works");
        assert_eq!(hit(at(600.0, 20.0), Regions::default()), Hit::Client);
    }

    #[test]
    fn screen_points_become_logical_client_points() {
        assert_eq!(to_client((300, 140), (100, 40), 2.0), (100.0, 50.0));
        assert_eq!(to_client((99, 40), (100, 40), 1.0), (-1.0, 0.0));
        assert_eq!(to_client((10, 10), (0, 0), 0.0), (10.0, 10.0));
    }

    #[test]
    fn a_maximized_client_area_starts_inside_the_screen() {
        assert_eq!(client_top(-8, true, 8), 0);
        assert_eq!(client_top(120, false, 8), 120);
    }

    const PROGRAM: SizeChange = SizeChange {
        keeps_position: true,
        keeps_size: false,
        user_sizing: false,
        zoomed_or_iconic: false,
    };

    #[test]
    fn a_program_resize_loses_the_caption_winit_added() {
        assert_eq!(excess_height(PROGRAM, 39), 39);
        assert_eq!(corrected_height(800 + 39, excess_height(PROGRAM, 39)), 800);
    }

    #[test]
    fn asking_for_the_same_client_size_again_keeps_the_window_height() {
        // winit asks for the current client size whenever the size limits change.
        let mut client = 900;
        for _ in 0..10 {
            let outer = client + 39;
            client = corrected_height(outer, excess_height(PROGRAM, 39));
        }
        assert_eq!(client, 900);
    }

    #[test]
    fn moves_user_drags_and_maximize_are_left_alone() {
        let moved = SizeChange {
            keeps_position: false,
            ..PROGRAM
        };
        let dragged = SizeChange {
            user_sizing: true,
            ..PROGRAM
        };
        let zoomed = SizeChange {
            zoomed_or_iconic: true,
            ..PROGRAM
        };
        let only_moved = SizeChange {
            keeps_size: true,
            ..PROGRAM
        };
        for c in [moved, dragged, zoomed, only_moved] {
            assert_eq!(excess_height(c, 39), 0, "{c:?}");
        }
        assert_eq!(excess_height(PROGRAM, -3), 0);
        assert_eq!(corrected_height(10, 39), 1);
    }

    #[test]
    fn only_the_flexible_space_moves_the_window() {
        let r = Regions {
            lead_end: 200.0,
            end_start: 600.0,
            max_x: 700.0,
            max_w: 46.0,
            row_h: 37.0,
        };
        assert!(in_drag_space(200.0, 10.0, r));
        assert!(in_drag_space(599.0, 36.0, r));
        assert!(!in_drag_space(199.0, 10.0, r), "leading group");
        assert!(!in_drag_space(600.0, 10.0, r), "end group");
        assert!(!in_drag_space(300.0, 37.0, r), "below the row");
        assert!(
            !in_drag_space(300.0, 10.0, Regions::default()),
            "no regions yet"
        );
    }

    #[test]
    fn a_double_click_follows_the_system_setting() {
        use DoubleClick::*;
        assert_eq!(double_click(Some("Maximize"), None), Zoom);
        assert_eq!(double_click(Some("Fill"), None), Zoom);
        assert_eq!(double_click(Some("Minimize"), Some(false)), Minimize);
        assert_eq!(double_click(Some("None"), Some(true)), Nothing);
        assert_eq!(double_click(Some("Something new"), None), Zoom);
        assert_eq!(double_click(None, Some(true)), Minimize);
        assert_eq!(double_click(None, Some(false)), Zoom);
        assert_eq!(double_click(None, None), Zoom);
    }
}
