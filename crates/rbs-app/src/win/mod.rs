//! Windows-only parts of the app: keeping RBS running without a window (ADR-0056: the
//! tray icon and the named event a second launch signals) and the toolbar row as the
//! title bar (ADR-0071, [`caption`]), and windows that stay drawn while a native
//! context menu is built ([`redraw`]). Marked low-level module (P11): calls into Win32,
//! each `unsafe` block with its invariant.

pub mod caption;
pub mod redraw;
mod show_event;
mod tray;

pub use show_event::{ShowEvent, signal_running};
pub use tray::{Tray, install_handlers};

/// The system cursor in screen pixels; `None` if the system cannot say. The press that
/// closes a system context menu reaches the window at the place the menu opened, so the
/// app compares with this.
pub fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: `p` is a valid, writable POINT for the duration of the call.
    let ok = unsafe { GetCursorPos(&mut p) } != 0;
    ok.then_some((p.x, p.y))
}

/// Whether the primary mouse button is up now. The click that closes a system context
/// menu can reach the window without its release (the menu takes it), so a drag asks
/// the system instead of waiting for a release that never comes.
pub fn primary_button_up() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
    // SAFETY: plain reads of the system's mouse state; no pointers, no invariants.
    let state = unsafe {
        // GetAsyncKeyState reads physical buttons: with the buttons swapped the
        // primary one is the right.
        let key = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
            VK_RBUTTON
        } else {
            VK_LBUTTON
        };
        GetAsyncKeyState(i32::from(key))
    };
    // The high bit (a negative value) means down.
    state >= 0
}
