//! The tray icon in the notification area (ADR-0056 decisions 6 to 9), through the
//! `tray-icon` crate. It exists only while Keep RBS running is set (P13): made when the
//! setting is turned on or at start, dropped when it is turned off. Its events arrive
//! on the UI thread's message loop through the crate's handlers; no thread, no polling.
//!
//! The menu uses `muda`, whose one event handler is process-wide and can be set only
//! once. RBS sets it at start ([`install_handlers`]); Slint's winit backend sets it only
//! for a native menu bar or context menu, which RBS has none of on Windows
//! (`tests/tray_menu_events.rs` fails when one is added, so the handler is reworked
//! to pass Slint's events on first).

use std::sync::OnceLock;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::background::{LiveState, TRAY_ICON_SIZE, TrayGlyph, tray_pixels};

const TRAY_ID: &str = "rbs.tray";
const SHOW_ID: &str = "rbs.tray.show";
const QUIT_ID: &str = "rbs.tray.quit";
const STATUS_ID: &str = "rbs.tray.status";

/// The app's show path and quit path.
type Paths = (fn(), fn());

/// The app's show and quit paths, set once by [`install_handlers`].
static PATHS: OnceLock<Paths> = OnceLock::new();

fn run(f: fn()) {
    // The paths only queue work on the UI thread; a panic stays here.
    let _ = rbs_crash::contain(f);
}

/// Routes tray clicks and the tray menu to the app: `show` for a click on the icon or
/// Show RBS, `quit` for Quit RBS. Both only queue work on the UI thread. Call once at
/// start, before any window exists.
pub fn install_handlers(show: fn(), quit: fn()) {
    if PATHS.set((show, quit)).is_err() {
        return;
    }
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
            && let Some((show, _)) = PATHS.get()
        {
            run(*show);
        }
    }));
    MenuEvent::set_event_handler(Some(|e: MenuEvent| {
        let Some((show, quit)) = PATHS.get() else {
            return;
        };
        match e.id().0.as_str() {
            SHOW_ID => run(*show),
            QUIT_ID => run(*quit),
            _ => {}
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
    status: MenuItem,
    shown: Option<(TrayGlyph, bool)>,
    tooltip: String,
}

impl Tray {
    /// Adds the icon to the notification area. `Err` says why it could not (Explorer
    /// not running, a shell error): closing the window must then quit.
    pub fn new(state: LiveState, status: &str, tooltip: &str) -> Result<Self, String> {
        let status_item = MenuItem::with_id(STATUS_ID, status, false, None);
        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        let show = MenuItem::with_id(SHOW_ID, "Show RBS", true, None);
        let quit = MenuItem::with_id(QUIT_ID, "Quit RBS", true, None);
        menu.append(&status_item)
            .and_then(|()| menu.append(&separator))
            .and_then(|()| menu.append(&show))
            .and_then(|()| menu.append(&quit))
            .map_err(|e| e.to_string())?;
        let mut builder = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(tooltip);
        if let Some(i) = icon(state) {
            builder = builder.with_icon(i);
        }
        let icon = builder.build().map_err(|e| e.to_string())?;
        Ok(Self {
            icon,
            status: status_item,
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
        if self.status.text() != status {
            self.status.set_text(status);
        }
        if self.tooltip != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
            tooltip.clone_into(&mut self.tooltip);
        }
    }
}
