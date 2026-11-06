//! The tray icon in the notification area (ADR-0056 decisions 6 to 9), through the
//! `tray-icon` crate. It exists only while Keep RBS running is set (P13): made when the
//! setting is turned on or at start, dropped when it is turned off. Its events arrive
//! on the UI thread's message loop through the crate's handler; no thread, no polling.
//!
//! The menu is a plain Win32 popup menu shown by RBS on a right click, not a `muda`
//! menu: `muda`'s event handler is process-wide, can be set only once, and belongs to
//! Slint's native menu bar. `TrackPopupMenu` runs its own message loop, as every
//! Windows context menu does; it runs from the tray window's message handler, never
//! inside a Slint callback or timer (ADR-0055 decision 6), and the chosen item only
//! queues work on the UI thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
    SetForegroundWindow, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_NULL,
};

use crate::background::{LiveState, TRAY_ICON_SIZE, TrayGlyph, tray_pixels};

const TRAY_ID: &str = "rbs.tray";
/// Menu command ids (0 means nothing was chosen).
const CMD_SHOW: usize = 1;
const CMD_QUIT: usize = 2;

/// The app's show path and quit path.
type Paths = (fn(), fn());

/// The app's show and quit paths, set once by [`install_handlers`].
static PATHS: OnceLock<Paths> = OnceLock::new();
/// The menu's status line, kept by [`Tray::set_state`].
static STATUS: Mutex<String> = Mutex::new(String::new());
/// The tray icon's message window, for the menu (0: none).
static WINDOW: AtomicUsize = AtomicUsize::new(0);

fn run(f: fn()) {
    // The paths only queue work on the UI thread; a panic stays here.
    let _ = rbs_crash::contain(f);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Shows the tray menu at the pointer and returns the chosen command (0: none).
fn show_menu(x: i32, y: i32) -> usize {
    let hwnd = WINDOW.load(Ordering::Relaxed) as HWND;
    if hwnd.is_null() {
        return 0;
    }
    let status = wide(&STATUS.lock().map(|s| s.clone()).unwrap_or_default());
    let show = wide("Show RBS");
    let quit = wide("Quit RBS");
    // SAFETY: the menu is created, filled, shown and destroyed here, on the thread
    // that owns `hwnd` (the tray icon's window, alive while the icon exists: this runs
    // from its own message handler). The strings are NUL-terminated and outlive the
    // calls (AppendMenuW copies them). The foreground call and the WM_NULL post are
    // what Windows requires for a tray menu to close when the user clicks elsewhere.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, status.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_SHOW, show.as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, quit.as_ptr());
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            x,
            y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        usize::try_from(chosen).unwrap_or(0)
    }
}

/// Routes the tray icon to the app: a left click shows the main window, a right click
/// (or Shift+F10 on the focused icon) opens the menu with Show RBS and Quit RBS. Both
/// paths only queue work on the UI thread. Call once at start.
pub fn install_handlers(show: fn(), quit: fn()) {
    if PATHS.set((show, quit)).is_err() {
        return;
    }
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        let Some((show, quit)) = PATHS.get() else {
            return;
        };
        let TrayIconEvent::Click {
            button,
            button_state: MouseButtonState::Up,
            position,
            ..
        } = e
        else {
            return;
        };
        match button {
            MouseButton::Left => run(*show),
            MouseButton::Right => {
                // Pixel positions fit in i32.
                let (x, y) = (position.x as i32, position.y as i32);
                match rbs_crash::contain(|| show_menu(x, y)) {
                    Ok(CMD_SHOW) => run(*show),
                    Ok(CMD_QUIT) => run(*quit),
                    _ => {}
                }
            }
            MouseButton::Middle => {}
        }
    }));
}

fn icon(state: LiveState) -> Option<Icon> {
    let (glyph, warning) = crate::background::tray_state(state);
    Icon::from_rgba(tray_pixels(glyph, warning), TRAY_ICON_SIZE, TRAY_ICON_SIZE).ok()
}

/// The tray icon while RBS keeps running.
pub struct Tray {
    icon: TrayIcon,
    shown: Option<(TrayGlyph, bool)>,
    tooltip: String,
}

impl Tray {
    /// Adds the icon to the notification area. `Err` says why it could not (Explorer
    /// not running, a shell error): closing the window must then quit.
    pub fn new(state: LiveState, status: &str, tooltip: &str) -> Result<Self, String> {
        let mut builder = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_tooltip(tooltip);
        if let Some(i) = icon(state) {
            builder = builder.with_icon(i);
        }
        let icon = builder.build().map_err(|e| e.to_string())?;
        WINDOW.store(icon.window_handle() as usize, Ordering::Relaxed);
        if let Ok(mut s) = STATUS.lock() {
            status.clone_into(&mut s);
        }
        Ok(Self {
            icon,
            shown: Some(crate::background::tray_state(state)),
            tooltip: tooltip.to_owned(),
        })
    }

    /// Shows `state`: the icon's variant, the menu's status line and the tooltip.
    /// Only what differs is changed.
    pub fn set_state(&mut self, state: LiveState, status: &str, tooltip: &str) {
        let variant = crate::background::tray_state(state);
        if self.shown != Some(variant) {
            if let Some(i) = icon(state) {
                let _ = self.icon.set_icon(Some(i));
            }
            self.shown = Some(variant);
        }
        if let Ok(mut s) = STATUS.lock()
            && *s != status
        {
            status.clone_into(&mut s);
        }
        if self.tooltip != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
            tooltip.clone_into(&mut self.tooltip);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // The icon's window goes with it.
        WINDOW.store(0, Ordering::Relaxed);
    }
}
