//! The merged title bar on Windows (ADR-0071): what a point of the window is, as pure
//! functions. `win::caption` asks them for every `WM_NCHITTEST` and `WM_NCCALCSIZE`;
//! they hold no platform code, so their tests run everywhere.

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
}
