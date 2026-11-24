//! The macOS window frame (ADR-0180): the window is changed after every show
//! (`rbs_app::native::titlebar`), a press in the row's flexible space moves it, and a
//! double-click there does what the system setting says.
//!
//! The `NSWindow` exists only once the event loop has made it, which can be after a
//! show and after timers run, so the change is also made from the window's events:
//! whenever its view is not the one last changed (a first window, or one Slint made
//! again), on a resize or a focus change. No polling.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;

use rbs_app::caption::{self, DoubleClick, Regions};
use rbs_app::native::titlebar;
use slint::ComponentHandle;
use slint::Window;
use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor};

use super::{Button, WindowFrame};
use crate::entry;
use crate::ui::{MainWindow, Shell};

thread_local! {
    /// Where the row's parts are, as the UI last sent them.
    static REGIONS: Cell<Regions> = Cell::new(Regions::default());
    /// The pointer's last position in the window, in logical pixels.
    static POINTER: Cell<(f32, f32)> = const { Cell::new((0.0, 0.0)) };
    /// The main window, for the full-screen notifications.
    static MAIN: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
    /// The open popups, refilled for each press without allocating again.
    static POPUPS: RefCell<Vec<rbs_ui::layout::Rect>> = const { RefCell::new(Vec::new()) };
    /// The view last changed, so a new window is changed once; null before any.
    static ATTACHED: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
}

/// The macOS frame.
pub struct Frame;

/// The main window's `NSView`, once the window exists.
fn view(main: &MainWindow) -> Option<NonNull<c_void>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = main.window().window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(v) => Some(v.ns_view),
        _ => None,
    }
}

/// Full screen began or ended (an AppKit notification, on the main thread).
fn full_screen_changed(on: bool) {
    let _ = entry::invoke(move || {
        let main = MAIN.with(|m| m.borrow().as_ref().and_then(slint::Weak::upgrade));
        if let Some(main) = main {
            super::note_full_screen(&main, on);
            let g = main.global::<Shell>();
            if let Some(v) = view(&main) {
                g.set_caption_min_height(titlebar::band_height(v) as f32);
                g.set_window_maximized(titlebar::zoomed(v));
            }
        }
    });
}

impl WindowFrame for Frame {
    fn attach_after_show(&self, main: &MainWindow) {
        MAIN.with(|m| *m.borrow_mut() = Some(main.as_weak()));
        // A window made again by this show is changed now, or from its first events.
        ATTACHED.with(|a| a.set(std::ptr::null_mut()));
        attach_if_new(main);
    }

    fn press(&self, main: &MainWindow, button: Button) {
        let Some(v) = view(main) else {
            return super::toolkit_press(main, button);
        };
        titlebar::press(
            v,
            match button {
                Button::Minimize => titlebar::Button::Minimize,
                Button::Maximize => titlebar::Button::Zoom,
                Button::Close => titlebar::Button::Close,
            },
        );
    }

    fn set_regions(&self, r: Regions) {
        REGIONS.with(|c| c.set(r));
    }

    /// The system's full screen (its own Space); the notifications report the change.
    fn toggle_full_screen(&self, main: &MainWindow) {
        if let Some(v) = view(main) {
            titlebar::toggle_full_screen(v);
        }
    }

    fn window_event(&self, main: &MainWindow, w: &Window, e: &WindowEvent) -> EventResult {
        // Objective-C calls run here: a panic stays inside (ADR-0055).
        entry::guard_or(|| EventResult::Propagate, || self.frame_event(main, w, e))
    }
}

impl Frame {
    fn frame_event(&self, main: &MainWindow, w: &Window, e: &WindowEvent) -> EventResult {
        if matches!(e, WindowEvent::Resized(_) | WindowEvent::Focused(_)) {
            attach_if_new(main);
        }
        let g = main.global::<Shell>();
        if !g.get_custom_caption() {
            return EventResult::Propagate;
        }
        match e {
            WindowEvent::CursorMoved { position, .. } => {
                let scale = f64::from(w.scale_factor());
                POINTER.with(|p| p.set(((position.x / scale) as f32, (position.y / scale) as f32)));
                EventResult::Propagate
            }
            WindowEvent::Resized(_) => {
                // The title band can change with the display or its scale too, and a
                // new style mask can bring the standard buttons back.
                if let Some(v) = view(main) {
                    titlebar::hide_buttons_again(v);
                    g.set_window_maximized(titlebar::zoomed(v));
                    g.set_caption_min_height(titlebar::band_height(v) as f32);
                }
                EventResult::Propagate
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = POINTER.with(Cell::get);
                // Full screen does not move or zoom (decision 4); a press that closes a
                // popup never moves the window (ADR-0071).
                if g.get_window_full_screen()
                    || !caption::in_drag_space(x, y, REGIONS.with(Cell::get))
                    || popup_open(main)
                {
                    return EventResult::Propagate;
                }
                if titlebar::is_double_click() {
                    let (action, miniaturize) = titlebar::double_click_setting();
                    let button = match caption::double_click(action.as_deref(), miniaturize) {
                        DoubleClick::Zoom => Some(Button::Maximize),
                        DoubleClick::Minimize => Some(Button::Minimize),
                        DoubleClick::Nothing => None,
                    };
                    if let Some(b) = button {
                        self.press(main, b);
                    }
                } else if let Some(Err(err)) = w.with_winit_window(|w| w.drag_window()) {
                    rbs_core::debug!(rbs_core::log::Area::Ui, "title bar: no drag: {err}");
                }
                EventResult::PreventDefault
            }
            _ => EventResult::Propagate,
        }
    }
}

fn popup_open(main: &MainWindow) -> bool {
    POPUPS.with(|p| {
        let Ok(mut p) = p.try_borrow_mut() else {
            return true;
        };
        p.clear();
        rbs_app::popups::open_in(main.window(), &mut p);
        !p.is_empty()
    })
}

/// Changes the main window when its view is not the one last changed, on the next
/// event loop turn (outside the toolkit's event handling).
fn attach_if_new(main: &MainWindow) {
    let Some(v) = view(main) else {
        return;
    };
    if ATTACHED.with(Cell::get) == v.as_ptr() {
        return;
    }
    ATTACHED.with(|a| a.set(v.as_ptr()));
    let weak = main.as_weak();
    entry::single_shot(std::time::Duration::ZERO, move || {
        let Some(main) = weak.upgrade() else {
            return;
        };
        // Not in a window yet: a later event tries again.
        let Some(v) = view(&main).filter(|v| titlebar::in_window(*v)) else {
            ATTACHED.with(|a| a.set(std::ptr::null_mut()));
            return;
        };
        let g = main.global::<Shell>();
        match titlebar::attach(v, full_screen_changed) {
            Ok(band) => {
                g.set_caption_min_height(band as f32);
                g.set_window_maximized(titlebar::zoomed(v));
                super::note_full_screen(&main, titlebar::full_screen(v));
                g.set_custom_caption(true);
            }
            Err(why) => {
                g.set_custom_caption(false);
                rbs_core::warn!(
                    rbs_core::log::Area::Ui,
                    "title bar: the OS title bar stays: {why}"
                );
            }
        }
    });
}
