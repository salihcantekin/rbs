//! The toolbar row as the main window's title bar (ADR-0071 on Windows, ADR-0180 on
//! macOS), behind one interface. `Shell.caption(n)` and the regions callback call the
//! frame of this system; only [`current`] and the frame files know which it is.
//! Linux keeps the OS title bar: its frame does nothing and the window buttons are
//! never shown.

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
    fn press(&self, _main: &MainWindow, _button: Button) {}
    fn set_regions(&self, _r: Regions) {}
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

/// The window buttons and the regions the UI sends.
pub fn install_handlers(main: &MainWindow) {
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
        Some(main) => current().window_event(&main, w, e),
        None => EventResult::Propagate,
    }
}
