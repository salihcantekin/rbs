//! Windows-only parts of the app: keeping RBS running without a window (ADR-0056: the
//! tray icon and the named event a second launch signals) and the toolbar row as the
//! title bar (ADR-0071, [`caption`]). Marked low-level module (P11): calls into Win32,
//! each `unsafe` block with its invariant.

pub mod caption;
mod show_event;
mod tray;

use std::num::NonZeroIsize;

use windows_sys::Win32::Foundation::HWND;

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

/// Which theme slot the real Windows setting asks for: 0 while apps are dark, 1
/// while they are light or high contrast is on. This is the system setting, not the
/// menu request (`native_menu` changes what the toolkit reports).
pub fn os_scheme_slot() -> i32 {
    if high_contrast() || apps_use_light_theme() != Some(false) {
        1
    } else {
        0
    }
}

fn high_contrast() -> bool {
    use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW};
    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        dwFlags: 0,
        lpszDefaultScheme: std::ptr::null_mut(),
    };
    // SAFETY: `hc` is a writable HIGHCONTRASTW for the duration of the call. The
    // scheme pointer stays null, so the system does not write a string.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            (&mut hc as *mut HIGHCONTRASTW).cast(),
            0,
        )
    };
    ok != 0 && hc.dwFlags & HCF_HIGHCONTRASTON != 0
}

/// `AppsUseLightTheme`: `Some(false)` is dark, `Some(true)` is light, `None` when the
/// value cannot be read (Windows then defaults to light).
fn apps_use_light_theme() -> Option<bool> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key = windows_sys::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = windows_sys::w!("AppsUseLightTheme");
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` is a writable DWORD and `size` is its byte length. The key and
    // name are null-terminated literals that outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            name,
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(value != 0)
}

/// Calls `on_change` on this thread when the user changes the Windows app theme.
/// One watcher for the process; it waits on the registry, it does not poll.
pub fn watch_os_scheme(on_change: impl Fn() + Send + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rbs-os-scheme".into())
        .spawn(move || watch_personalize(on_change))
        .is_ok();
    if !spawned {
        STARTED.store(false, Ordering::Relaxed);
    }
}

fn watch_personalize(on_change: impl Fn()) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, RegCloseKey,
        RegNotifyChangeKeyValue, RegOpenKeyExW,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let sub = windows_sys::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    // SAFETY: the key path is a literal. The opened handle is closed before return.
    // The event is a manual-reset kernel object this thread owns.
    unsafe {
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub, 0, KEY_NOTIFY, &mut key) != 0 {
            return;
        }
        let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if event.is_null() {
            RegCloseKey(key);
            return;
        }
        loop {
            if RegNotifyChangeKeyValue(key, 0, REG_NOTIFY_CHANGE_LAST_SET, event, 1) != 0 {
                break;
            }
            if WaitForSingleObject(event, INFINITE) != WAIT_OBJECT_0 {
                break;
            }
            on_change();
            // Manual-reset: arm it again before the next wait.
            windows_sys::Win32::System::Threading::ResetEvent(event);
        }
        CloseHandle(event);
        RegCloseKey(key);
    }
}

/// Asks Windows to draw its own menus dark or light. The menus stay the system's;
/// a dark RBS theme asks for the dark menu, a light theme for the light one.
/// Custom theme colours are not painted onto the menu. This also changes what the
/// toolkit reports as the system scheme, so the theme slot is chosen from
/// [`os_scheme_slot`] instead.
pub fn native_menu(dark: bool) {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    // uxtheme ordinals. 135 is SetPreferredAppMode (2 force dark, 3 force light).
    // 104 refreshes menus that are already built.
    const SET_PREFERRED_APP_MODE: u16 = 135;
    const REFRESH: u16 = 104;
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;
    type SetMode = unsafe extern "system" fn(i32) -> i32;
    type Refresh = unsafe extern "system" fn();
    // SAFETY: the names are the system library and ordinals it has exported since
    // Windows 10 1903. A missing export is a null pointer and is not called.
    unsafe {
        let lib = LoadLibraryA(c"uxtheme.dll".as_ptr().cast());
        if lib.is_null() {
            return;
        }
        if let Some(set) = GetProcAddress(lib, SET_PREFERRED_APP_MODE as usize as *const _) {
            let set: SetMode = std::mem::transmute(set);
            set(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        if let Some(refresh) = GetProcAddress(lib, REFRESH as usize as *const _) {
            let refresh: Refresh = std::mem::transmute(refresh);
            refresh();
        }
    }
}

/// Leaves this window out of a screen capture, or puts it back.
pub fn exclude_from_capture(hwnd: NonZeroIsize, exclude: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };
    let affinity = if exclude {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    // SAFETY: `hwnd` is a window this process created. The call only sets a flag
    // the capture APIs read; a rejected call leaves the window as it was.
    unsafe {
        SetWindowDisplayAffinity(hwnd.get() as HWND, affinity);
    }
}
