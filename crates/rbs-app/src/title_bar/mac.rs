//! The macOS window frame (ADR-0180): the window is changed after every show
//! (`rbs_app::native::titlebar`), a press in the row's flexible space moves it, and a
//! double-click there does what the system setting says.

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
}

/// The macOS frame.
pub struct Frame;

/// The main window's `NSView`, once the window exists.
fn view(main: &MainWindow) -> Option<NonNull<c_void>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = main.window().window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(v) => NonNull::new(v.ns_view.as_ptr()),
        _ => None,
    }
}

/// Full screen began or ended (an AppKit notification, on the main thread).
fn full_screen_changed(on: bool) {
    let _ = entry::invoke(move || {
        let main = MAIN.with(|m| m.borrow().as_ref().and_then(slint::Weak::upgrade));
        if let Some(main) = main {
            let g = main.global::<Shell>();
            g.set_window_full_screen(on);
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
        // The NSWindow exists once the event loop has run the show; look on the next turn.
        let weak = main.as_weak();
        entry::single_shot(std::time::Duration::ZERO, move || {
            let Some(main) = weak.upgrade() else {
                return;
            };
            let g = main.global::<Shell>();
            let Some(v) = view(&main) else {
                rbs_core::warn!(
                    rbs_core::log::Area::Ui,
                    "title bar: the main window has no view"
                );
                return;
            };
            match titlebar::attach(v, full_screen_changed) {
                Ok(band) => {
                    g.set_caption_min_height(band as f32);
                    g.set_window_maximized(titlebar::zoomed(v));
                    g.set_window_full_screen(titlebar::full_screen(v));
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

    fn press(&self, main: &MainWindow, button: Button) {
        let Some(v) = view(main) else {
            return;
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

    fn window_event(&self, main: &MainWindow, w: &Window, e: &WindowEvent) -> EventResult {
        if !main.global::<Shell>().get_custom_caption() {
            return EventResult::Propagate;
        }
        match e {
            WindowEvent::CursorMoved { position, .. } => {
                let scale = f64::from(w.scale_factor());
                POINTER.with(|p| p.set(((position.x / scale) as f32, (position.y / scale) as f32)));
                EventResult::Propagate
            }
            WindowEvent::Resized(_) => {
                // The title band can change with the display or its scale too.
                if let Some(v) = view(main) {
                    let g = main.global::<Shell>();
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
                if !caption::in_drag_space(x, y, REGIONS.with(Cell::get)) || popup_open(main) {
                    // A press that closes a popup never moves the window (ADR-0071).
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
