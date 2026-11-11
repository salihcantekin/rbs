//! Keeps RBS windows on screen while Slint builds a native context menu.
//!
//! Slint 1.18.1 (`i-slint-backend-winit` `muda.rs`, `rebuild_menu`) sends a visible
//! window `WM_SETREDRAW FALSE` before it builds a menu and `TRUE` after. The default
//! window procedure handles `FALSE` by clearing `WS_VISIBLE`, so for those 2-3 ms the
//! compositor leaves the window out; a frame shown then has no RBS window in it (the
//! flash seen on a right-click, measured on SC-PC: 12 of 12 menus hid the window, none
//! once the message is ignored). RBS windows have no Win32 menu bar to protect while
//! one is built, so this subclass drops `WM_SETREDRAW FALSE` (and its `TRUE` pair is
//! then harmless). Nothing runs for any other message beyond the subclass chain.

use std::num::NonZeroIsize;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{WM_NCDESTROY, WM_SETREDRAW};

/// The subclass's id among the window's subclasses.
const SUBCLASS_ID: usize = 0x5242_5344; // "RBSD"

/// Makes `window` (a top-level RBS window, as `raw-window-handle` gives it) ignore
/// `WM_SETREDRAW FALSE` until it is destroyed. Again for the same window does
/// nothing new. Returns whether the subclass is in place; a handle that is not a
/// window of this thread is refused by Windows.
pub fn keep_visible(window: NonZeroIsize) -> bool {
    let hwnd = window.get() as HWND;
    // SAFETY: Windows checks the handle (a window of another thread or none is
    // refused); `procedure` matches SUBCLASSPROC and lives for the program. Setting
    // the same procedure and id again only replaces its data.
    unsafe { SetWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID, 0) != 0 }
}

/// The window procedure in front of the others. Panics cannot happen here (no
/// allocation, no indexing), so nothing is contained.
unsafe extern "system" fn procedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_SETREDRAW if wparam == 0 => 0,
        WM_NCDESTROY => {
            // SAFETY: removes this subclass from the window being destroyed.
            unsafe { RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID) };
            // SAFETY: Windows' own arguments for this window, passed on unchanged.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: Windows' own arguments for this window, passed on unchanged.
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}
