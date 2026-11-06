//! The popups Slint draws inside a window, as rectangles.
//!
//! Slint 1.18's winit backend draws a `PopupWindow` inside its window, under the native
//! preview layer (ADR-0034, ADR-0036). Instead of every popup counting itself, the shell
//! asks Slint which popups are open and where it placed them (after clamping them into
//! the window), so a popup added anywhere in the UI is seen without code of its own.
//!
//! A popup's own size can be smaller than what it draws: a child wider than the popup is
//! centred on it and drawn past its edges. The rectangle is the popup joined with its
//! direct children, so the cover matches what is on screen.
//!
//! Reads `i-slint-core`, Slint's internal crate, at the exact version `slint` is pinned
//! to; `tests/popup_covers.rs` checks the result against known placements.

use i_slint_core::item_tree::ItemRc;
use i_slint_core::window::{PopupWindowLocation, WindowInner};
use rbs_ui::layout::Rect;

/// Replaces `out` with the popups open in `window`, in its logical pixels, oldest first.
/// Popups Slint shows as their own top-level windows are left out: the system draws
/// those above the window's layers.
pub fn open_in(window: &slint::Window, out: &mut Vec<Rect>) {
    out.clear();
    let inner = WindowInner::from_pub(window);
    for popup in inner.active_popups().iter() {
        if let PopupWindowLocation::ChildWindow(at) = &popup.location {
            let root = ItemRc::new_root(popup.component.clone());
            let size = root.geometry().size;
            let (mut x0, mut y0, mut x1, mut y1) = (0.0f32, 0.0f32, size.width, size.height);
            let mut child = root.first_child();
            while let Some(item) = child {
                let g = item.geometry();
                if g.size.width > 0.0 && g.size.height > 0.0 {
                    x0 = x0.min(g.origin.x);
                    y0 = y0.min(g.origin.y);
                    x1 = x1.max(g.origin.x + g.size.width);
                    y1 = y1.max(g.origin.y + g.size.height);
                }
                child = item.next_sibling();
            }
            out.push(Rect {
                x: at.x + x0,
                y: at.y + y0,
                w: x1 - x0,
                h: y1 - y0,
            });
        }
    }
}
