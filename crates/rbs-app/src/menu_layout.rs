//! Where a drawn menu goes and which row a typed letter moves to (ADR-0137): pure
//! functions, and the `ContextMenus` callbacks that call them, so the code Slint runs
//! is the code the tests check.

use rbs_app_ui::{ContextMenus, OverlayCover};

/// A rectangle in window coordinates (logical pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spot {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// Where a drawn menu of natural size `size` goes (ADR-0137 decision 5), in a window
/// of `window` size, keeping `margin` from every edge. Always flip, then shrink (the
/// menu scrolls), then clamp.
///
/// A menu (`beside` false) opens under `anchor` (a right-click's point has no height;
/// a button is its rectangle), else over it, else on the side with more room, shrunk
/// to it; it starts at the anchor's left, or ends at its right when it does not fit.
/// A submenu (`beside` true) opens beside `anchor` (its row): on the right, else on
/// the left, else on the side with more room; its top is level with the row's, moved
/// up until it fits.
pub fn place(anchor: Spot, size: [f32; 2], window: [f32; 2], margin: f32, beside: bool) -> Spot {
    let [win_w, win_h] = window;
    let room_w = (win_w - 2.0 * margin).max(0.0);
    let room_h = (win_h - 2.0 * margin).max(0.0);
    let w = size[0].min(room_w);
    let clamp_x = |x: f32| x.min(win_w - margin - w).max(margin);
    let clamp_y = |y: f32, h: f32| y.min(win_h - margin - h).max(margin);
    if beside {
        let right = anchor.x + anchor.w;
        let x = if right + w <= win_w - margin {
            right
        } else if anchor.x - w >= margin {
            anchor.x - w
        } else if win_w - margin - right >= anchor.x - margin {
            win_w - margin - w
        } else {
            margin
        };
        let h = size[1].min(room_h);
        return Spot {
            x: clamp_x(x),
            y: clamp_y(anchor.y, h),
            w,
            h,
        };
    }
    let x = if anchor.x + w <= win_w - margin {
        anchor.x
    } else {
        anchor.x + anchor.w - w
    };
    let bottom = anchor.y + anchor.h;
    let below = (win_h - margin - bottom).max(0.0);
    let above = (anchor.y - margin).max(0.0);
    let (y, h) = if size[1] <= below {
        (bottom, size[1])
    } else if size[1] <= above {
        (anchor.y - size[1], size[1])
    } else if below >= above {
        (bottom, below)
    } else {
        (anchor.y - above, above)
    };
    Spot {
        x: clamp_x(x),
        y: clamp_y(y, h),
        w,
        h,
    }
}

/// The next row after `from` (wrapping; from the start when `from` is out of range)
/// whose title starts with `letter`, ignoring case, or `None`. Typing a letter in a
/// drawn menu moves there (ADR-0137 decision 7).
pub fn find<'a>(
    titles: impl ExactSizeIterator<Item = &'a str> + Clone,
    from: i32,
    letter: &str,
) -> Option<usize> {
    let letter = letter.chars().next()?;
    if letter.is_control() || letter.is_whitespace() {
        return None;
    }
    let n = titles.len();
    if n == 0 {
        return None;
    }
    let start = usize::try_from(from)
        .ok()
        .filter(|&f| f < n)
        .map_or(0, |f| f + 1);
    let lower: String = letter.to_lowercase().collect();
    (0..n).map(|k| (start + k) % n).find(|&i| {
        titles
            .clone()
            .nth(i)
            .is_some_and(|t| t.trim_start().to_lowercase().starts_with(&lower))
    })
}

/// Connects `ContextMenus.place` and `ContextMenus.find` of a window to [`place`] and
/// [`find`].
pub fn install(g: &ContextMenus<'_>) {
    g.on_find(|rows, from, letter| {
        let titles: Vec<slint::SharedString> = slint::Model::iter(&rows)
            .map(|r| {
                if r.kind == 2 {
                    slint::SharedString::new()
                } else {
                    r.title
                }
            })
            .collect();
        find(titles.iter().map(|t| t.as_str()), from, &letter)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1)
    });
    g.on_place(|anchor, w, h, win_w, win_h, margin, beside| {
        let spot = place(
            Spot {
                x: anchor.x,
                y: anchor.y,
                w: anchor.width,
                h: anchor.height,
            },
            [w, h],
            [win_w, win_h],
            margin,
            beside,
        );
        OverlayCover {
            x: spot.x,
            y: spot.y,
            width: spot.w,
            height: spot.h,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: [f32; 2] = [800.0, 600.0];
    const M: f32 = 4.0;

    fn point(x: f32, y: f32) -> Spot {
        Spot {
            x,
            y,
            w: 0.0,
            h: 0.0,
        }
    }

    #[test]
    fn a_menu_opens_below_and_right_of_the_point_when_it_fits() {
        let s = place(point(100.0, 100.0), [200.0, 300.0], WIN, M, false);
        assert_eq!(
            s,
            Spot {
                x: 100.0,
                y: 100.0,
                w: 200.0,
                h: 300.0
            }
        );
    }

    #[test]
    fn a_menu_near_the_bottom_opens_upward() {
        let s = place(point(100.0, 550.0), [200.0, 300.0], WIN, M, false);
        assert_eq!((s.y, s.h), (250.0, 300.0), "its bottom is at the point");
    }

    #[test]
    fn a_menu_near_the_right_edge_ends_at_the_point() {
        let s = place(point(700.0, 100.0), [200.0, 100.0], WIN, M, false);
        assert_eq!(s.x, 500.0);
    }

    #[test]
    fn a_button_menu_flips_over_the_button_not_over_its_bottom() {
        // A 30 px button whose bottom is at y 590, near the window's bottom.
        let button = Spot {
            x: 10.0,
            y: 560.0,
            w: 0.0,
            h: 30.0,
        };
        let s = place(button, [200.0, 200.0], WIN, M, false);
        assert_eq!((s.y, s.h), (360.0, 200.0), "it ends at the button's top");
    }

    #[test]
    fn a_menu_that_fits_neither_way_takes_the_larger_side_and_scrolls() {
        let s = place(point(100.0, 200.0), [200.0, 500.0], WIN, M, false);
        assert_eq!((s.y, s.h), (200.0, 396.0), "below has 396 px, above 196");
        let s = place(point(100.0, 400.0), [200.0, 500.0], WIN, M, false);
        assert_eq!((s.y, s.h), (4.0, 396.0), "above has 396 px");
    }

    #[test]
    fn a_menu_stays_inside_a_small_window() {
        let s = place(point(5.0, 5.0), [300.0, 100.0], [200.0, 80.0], M, false);
        assert_eq!(s.w, 192.0, "never wider than the window's room");
        assert!(s.x >= M && s.x + s.w <= 200.0 - M);
        assert!(s.y >= M && s.y + s.h <= 80.0 - M);
    }

    #[test]
    fn a_submenu_opens_right_then_left_then_where_there_is_more_room() {
        let row = Spot {
            x: 100.0,
            y: 100.0,
            w: 200.0,
            h: 28.0,
        };
        assert_eq!(place(row, [200.0, 100.0], WIN, M, true).x, 300.0);
        let row = Spot {
            x: 500.0,
            y: 100.0,
            w: 200.0,
            h: 28.0,
        };
        assert_eq!(place(row, [200.0, 100.0], WIN, M, true).x, 300.0, "left");
        let row = Spot {
            x: 100.0,
            y: 100.0,
            w: 500.0,
            h: 28.0,
        };
        let s = place(row, [250.0, 100.0], WIN, M, true);
        assert_eq!(s.x, 546.0, "neither side fits; the right has more room");
        let s = place(
            Spot {
                x: 100.0,
                y: 500.0,
                w: 200.0,
                h: 28.0,
            },
            [200.0, 300.0],
            WIN,
            M,
            true,
        );
        assert_eq!((s.y, s.h), (296.0, 300.0), "moved up until it fits");
    }

    #[test]
    fn a_letter_moves_to_the_next_title_that_starts_with_it() {
        let titles = ["Rename", "", "Remove", "Duplicate"];
        let it = || titles.iter().copied();
        assert_eq!(find(it(), -1, "r"), Some(0));
        assert_eq!(find(it(), 0, "R"), Some(2));
        assert_eq!(find(it(), 2, "r"), Some(0), "wraps");
        assert_eq!(find(it(), 0, "d"), Some(3));
        assert_eq!(find(it(), 0, "x"), None);
        assert_eq!(find(it(), 0, " "), None);
        assert_eq!(find(it(), 0, ""), None);
        assert_eq!(find(std::iter::empty::<&str>(), 0, "a"), None);
    }
}
