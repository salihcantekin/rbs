//! Windows-only parts of the app: keeping RBS running without a window (ADR-0056: the
//! tray icon and the named event a second launch signals) and the toolbar row as the
//! title bar (ADR-0071, [`caption`]), and windows that stay drawn while a native
//! context menu is built ([`redraw`]). Marked low-level module (P11): calls into Win32,
//! each `unsafe` block with its invariant.

pub mod caption;
pub mod redraw;
mod show_event;
mod tray;

use std::num::NonZeroIsize;

use windows_sys::Win32::Foundation::HWND;

pub use show_event::{ShowEvent, signal_running};
pub use tray::{Tray, install_handlers};

/// The Videos known folder (it follows the user's move of the folder); `None` if the
/// shell cannot say.
pub fn videos_folder() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `FOLDERID_Videos` is a static GUID, a null token means the current user,
    // and `raw` is a writable pointer the call fills.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Videos,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a null-terminated UTF-16 string the shell owns
        // until it is freed below; the slice ends before the terminator.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::slice::from_raw_parts(raw, len)
        };
        std::path::PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: the shell allocated `raw` with CoTaskMemAlloc (it must be freed even when
    // the call fails); it is freed once, after its last read. Null is allowed.
    unsafe { CoTaskMemFree(raw.cast()) };
    path.filter(|p| !p.as_os_str().is_empty())
}

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

/// Remembers `on_change` for the process. The main window's procedure calls it when
/// Windows sends `WM_SETTINGCHANGE` for the app theme (`ImmersiveColorSet`) or high
/// contrast. No extra thread: the message arrives on the UI thread.
pub fn watch_os_scheme(on_change: impl Fn() + Send + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    match SCHEME.lock() {
        Ok(mut slot) => *slot = Some(Box::new(on_change)),
        Err(poisoned) => *poisoned.into_inner() = Some(Box::new(on_change)),
    }
}

static SCHEME: std::sync::Mutex<Option<Box<dyn Fn() + Send>>> = std::sync::Mutex::new(None);
/// The slot last given to the callback. Applying it asks Windows to refresh menus,
/// and that sends `WM_SETTINGCHANGE` again for the same slot. Handling that repeat
/// never returns to the close button.
static SCHEME_SLOT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Runs the theme callback registered by [`watch_os_scheme`] when the slot changed.
/// The window procedure calls this on the UI thread; the callback queues the UI update.
pub(crate) fn notify_os_scheme() {
    use std::sync::atomic::Ordering;
    let slot = os_scheme_slot();
    if SCHEME_SLOT.swap(slot, Ordering::Relaxed) == slot {
        return;
    }
    let guard = SCHEME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(on_change) = guard.as_ref() {
        on_change();
    }
}

/// Whether this `WM_SETTINGCHANGE` is an app-theme or high-contrast change.
/// `lparam` is null or a NUL-terminated wide string Windows owns for the call.
pub(crate) fn theme_setting(wparam: usize, lparam: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::SPI_SETHIGHCONTRAST;
    if wparam == SPI_SETHIGHCONTRAST as usize {
        return true;
    }
    if lparam == 0 {
        return false;
    }
    const NAME: *const u16 = windows_sys::w!("ImmersiveColorSet");
    let p = lparam as *const u16;
    // SAFETY: `NAME` is a NUL-terminated literal. A non-null WM_SETTINGCHANGE
    // lParam is a NUL-terminated wide string for this call. The read stops at
    // 64 units.
    unsafe {
        let mut i = 0;
        loop {
            if i == 64 {
                return false;
            }
            let want = *NAME.add(i);
            if *p.add(i) != want {
                return false;
            }
            if want == 0 {
                return true;
            }
            i += 1;
        }
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

/// Leaves this window out of a screen capture, or puts it back. `false` when the
/// system refused the call (the caller tries again once the window is ready).
pub fn exclude_from_capture(hwnd: NonZeroIsize, exclude: bool) -> bool {
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
    unsafe { SetWindowDisplayAffinity(hwnd.get() as HWND, affinity) != 0 }
}

/// Brings an already-shown window of this process in front of the others, without
/// moving it. Restores it when minimised. Used when the user asks to open a window
/// that is already open (Settings, Source settings, a floating panel, the main window).
pub fn bring_forward(hwnd: NonZeroIsize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    let hwnd = hwnd.get() as HWND;
    // SAFETY: `hwnd` is a window this process created and still owns. ShowWindow /
    // BringWindowToTop / SetForegroundWindow only change z-order and activation; a
    // refused SetForegroundWindow leaves the window where it was.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
    }
}
