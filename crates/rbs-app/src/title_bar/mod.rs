//! The toolbar row as the main window's title bar (ADR-0071 on Windows, ADR-0180 on
//! macOS), behind one interface. `Shell.caption(n)` and the regions callback call the
//! frame of this system; only [`current`] and the frame files know which it is.
//! Linux keeps the OS title bar: its frame does nothing and the window buttons are
//! never shown.

use std::cell::{Cell, RefCell};

use rbs_app::caption::Regions;
use slint::ComponentHandle;
use slint::Window;
use slint::winit_030::EventResult;
use slint::winit_030::winit::event::WindowEvent;

use crate::entry;
use crate::ui::{MainWindow, Shell};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;

/// A window button of the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Minimize.
    Minimize,
    /// Maximize, or restore when maximized (zoom on macOS).
    Maximize,
    /// Close: a close request, ADR-0056's close path.
    Close,
}

/// A main window command of the registry (`ui.window.*`, ADR-0180 decision 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    /// `ui.window.minimize`.
    Minimize,
    /// `ui.window.zoom`: maximize, or restore.
    Zoom,
    /// `ui.window.full_screen`: into full screen, or out of it.
    FullScreen,
}

impl WindowCommand {
    /// The command of a registry id (`ui.window.minimize`, ...).
    pub fn from_id(id: &str) -> Option<WindowCommand> {
        Some(match id {
            "ui.window.minimize" => WindowCommand::Minimize,
            "ui.window.zoom" => WindowCommand::Zoom,
            "ui.window.full_screen" => WindowCommand::FullScreen,
            _ => return None,
        })
    }
}

/// Whether `command` acts now (ADR-0180 decision 5): nothing while the main window
/// is hidden (ADR-0056) or minimized (the system cannot restyle a minimized window
/// reliably), and minimize and zoom do nothing in full screen, where their buttons are
/// hidden. A call that does not act still succeeds.
pub fn acts(command: WindowCommand, shown: bool, minimized: bool, full_screen: bool) -> bool {
    shown && !minimized && (command == WindowCommand::FullScreen || !full_screen)
}

thread_local! {
    /// The main window is full screen, as its frame last reported.
    static FULL_SCREEN: Cell<bool> = const { Cell::new(false) };
    /// Called when full screen changes, so the menus' checkmark follows a change the
    /// system made later (macOS) or alone (Esc, the green button).
    static ON_CHANGE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Whether the main window is full screen (the Window menu's checkmark).
pub fn is_full_screen() -> bool {
    FULL_SCREEN.with(Cell::get)
}

/// The frame reports full screen on or off: the menu's checkmark and the row's
/// buttons follow it.
fn note_full_screen(main: &MainWindow, on: bool) {
    main.global::<Shell>().set_window_full_screen(on);
    if FULL_SCREEN.with(|f| f.replace(on)) != on {
        ON_CHANGE.with(|c| {
            if let Some(changed) = c.borrow().as_ref() {
                changed();
            }
        });
    }
}

/// Runs a window command on the main window.
pub fn run(main: &MainWindow, command: WindowCommand) {
    let w = main.window();
    if !acts(command, w.is_visible(), w.is_minimized(), is_full_screen()) {
        return;
    }
    let frame = current();
    match command {
        WindowCommand::Minimize => frame.press(main, Button::Minimize),
        WindowCommand::Zoom => frame.press(main, Button::Maximize),
        WindowCommand::FullScreen => frame.toggle_full_screen(main),
    }
}

/// What the main window's frame does on this system (ADR-0180 decision 7). Used on the
/// UI thread only. Moving the window and the double-click in the row's flexible space
/// are the OS's own on Windows (its hit test), so [`WindowFrame::window_event`] does
/// nothing there.
pub trait WindowFrame {
    /// After every show of the main window: takes the OS title bar away once the
    /// window exists, or leaves it and logs why. Sets `Shell.custom-caption`.
    fn attach_after_show(&self, main: &MainWindow);
    /// Runs a window button.
    fn press(&self, main: &MainWindow, button: Button);
    /// Where the row's parts are now.
    fn set_regions(&self, r: Regions);
    /// Into full screen, or out of it: borderless on the window's display (winit),
    /// unless the system has its own.
    fn toggle_full_screen(&self, main: &MainWindow) {
        let on = !is_full_screen();
        main.window().set_fullscreen(on);
        note_full_screen(main, on);
    }
    /// A window event before the toolkit sees it: a press in the flexible space moves
    /// the window or is a double-click.
    fn window_event(&self, _main: &MainWindow, _w: &Window, _e: &WindowEvent) -> EventResult {
        EventResult::Propagate
    }
}

/// The frame of a system without one of RBS's own: the OS title bar stays.
#[cfg(not(any(windows, target_os = "macos")))]
struct OsFrame;

#[cfg(not(any(windows, target_os = "macos")))]
impl WindowFrame for OsFrame {
    fn attach_after_show(&self, _main: &MainWindow) {}
    /// The OS title bar has the buttons; the window commands use the toolkit's calls.
    fn press(&self, main: &MainWindow, button: Button) {
        toolkit_press(main, button);
    }
    fn set_regions(&self, _r: Regions) {}
}

/// Minimize and maximize through the toolkit: the frame of a system without RBS's own,
/// and the fallback while RBS's frame is not attached (the OS title bar stayed). Close
/// is the OS title bar's own there.
fn toolkit_press(main: &MainWindow, button: Button) {
    let w = main.window();
    match button {
        Button::Minimize => w.set_minimized(true),
        Button::Maximize => w.set_maximized(!w.is_maximized()),
        Button::Close => {}
    }
}

/// The frame of this system.
pub fn current() -> &'static dyn WindowFrame {
    #[cfg(windows)]
    {
        &win::Frame
    }
    #[cfg(target_os = "macos")]
    {
        &mac::Frame
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        &OsFrame
    }
}

/// The window buttons and the regions the UI sends; `changed` runs when full screen
/// begins or ends. Runs before the event loop.
pub fn install_handlers(main: &MainWindow, changed: impl Fn() + 'static) {
    ON_CHANGE.with(|c| *c.borrow_mut() = Some(Box::new(changed)));
    #[cfg(target_os = "macos")]
    rbs_app::native::titlebar::hide_system_full_screen_item();
    let weak = main.as_weak();
    main.global::<Shell>().on_caption(move |which| {
        entry::guard(|| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            let button = match which {
                0 => Button::Minimize,
                1 => Button::Maximize,
                _ => Button::Close,
            };
            current().press(&main, button);
        });
    });
    main.global::<Shell>()
        .on_caption_regions(move |lead_end, end_start, max_x, max_w, row_h| {
            entry::guard(|| {
                current().set_regions(Regions {
                    lead_end,
                    end_start,
                    max_x,
                    max_w,
                    row_h,
                });
            });
        });
}

/// After every show of the main window (see [`WindowFrame::attach_after_show`]).
pub fn attach_after_show(main: &MainWindow) {
    current().attach_after_show(main);
}

/// The main window's events for its frame, for `window_events::install_with`.
pub fn events(main: &MainWindow) -> impl FnMut(&Window, &WindowEvent) -> EventResult + 'static {
    let weak = main.as_weak();
    move |w, e| match weak.upgrade() {
        Some(main) => {
            // Without a frame of RBS's own the window manager may end full screen
            // itself; the toolkit's flag follows winit on every resize.
            #[cfg(not(any(windows, target_os = "macos")))]
            if matches!(e, WindowEvent::Resized(_)) {
                note_full_screen(&main, w.is_fullscreen());
            }
            current().window_event(&main, w, e)
        }
        None => EventResult::Propagate,
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowCommand as C, acts};

    #[test]
    fn a_window_command_acts_only_when_it_can() {
        // acts(command, shown, minimized, full_screen)
        for c in [C::Minimize, C::Zoom, C::FullScreen] {
            assert!(acts(c, true, false, false), "{c:?} on a shown window");
            assert!(!acts(c, false, false, false), "{c:?} on a hidden window");
            assert!(
                !acts(c, false, false, true),
                "{c:?} on a hidden full-screen window"
            );
            assert!(!acts(c, true, true, false), "{c:?} on a minimized window");
        }
        assert!(
            acts(C::FullScreen, true, false, true),
            "leaving full screen"
        );
        assert!(!acts(C::Minimize, true, false, true));
        assert!(!acts(C::Zoom, true, false, true));
    }
}
