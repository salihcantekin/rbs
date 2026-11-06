//! The popups Slint draws inside a window, as rectangles.
//!
//! Slint 1.18's winit backend draws a `PopupWindow` inside its window, under the native
//! preview layer (ADR-0034, ADR-0036). Instead of every popup counting itself, the shell
//! asks Slint which popups are open and where it placed them (after clamping them into
//! the window), so a popup added anywhere in the UI is seen without code of its own.
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
            let size = ItemRc::new_root(popup.component.clone()).geometry().size;
            out.push(Rect {
                x: at.x,
                y: at.y,
                w: size.width,
                h: size.height,
            });
        }
    }
}
