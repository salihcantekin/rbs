//! The toolbar row as the title bar on Windows (ADR-0071): connects the main window's
//! `Shell` to `rbs_app::win::caption`. Elsewhere the OS title bar stays and nothing
//! here runs (the window buttons are never shown).

use crate::entry;
use crate::ui::{MainWindow, Shell};
use slint::ComponentHandle;

/// The window buttons and the regions the UI sends.
pub fn install_handlers(main: &MainWindow) {
    main.global::<Shell>().on_caption(move |which| {
        entry::guard(|| {
            #[cfg(windows)]
            {
                use rbs_app::win::caption::{Button, press};
                press(match which {
                    0 => Button::Minimize,
                    1 => Button::Maximize,
                    _ => Button::Close,
                });
            }
            #[cfg(not(windows))]
            let _ = which;
        });
    });
    main.global::<Shell>()
        .on_caption_regions(move |lead_end, end_start, max_x, max_w, row_h| {
            entry::guard(|| {
                #[cfg(windows)]
                rbs_app::win::caption::set_regions(rbs_app::caption::Regions {
                    lead_end,
                    end_start,
                    max_x,
                    max_w,
                    row_h,
                });
                #[cfg(not(windows))]
                let _ = (lead_end, end_start, max_x, max_w, row_h);
            });
        });
}

/// Before each redraw of the main window: once its Win32 window exists (and again
/// whenever it is a new one), removes the caption, after this redraw.
#[cfg(windows)]
pub fn attach(main: &MainWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let window_handle = main.window().window_handle();
    let Ok(handle) = window_handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd;
    if rbs_app::win::caption::window() == Some(hwnd.get() as windows_sys::Win32::Foundation::HWND) {
        return;
    }
    let weak = main.as_weak();
    // Not inside the redraw: removing the caption resizes the client area at once.
    entry::single_shot(std::time::Duration::ZERO, move || {
        if let Some(main) = weak.upgrade() {
            install(&main, hwnd);
        }
    });
}

#[cfg(windows)]
fn install(main: &MainWindow, hwnd: std::num::NonZeroIsize) {
    use std::cell::RefCell;
    use std::rc::Rc;

    use rbs_app::win::caption::{self, Event};

    thread_local! {
        // The open popups, refilled for each hit test without allocating again.
        static POPUPS: RefCell<Vec<rbs_ui::layout::Rect>> = const { RefCell::new(Vec::new()) };
    }
    let weak = main.as_weak();
    let sink = Rc::new(move |e: Event| {
        if let Some(main) = weak.upgrade() {
            let g = main.global::<Shell>();
            match e {
                Event::MaxHover(on) => g.set_caption_max_hover(on),
                Event::MaxPressed(on) => g.set_caption_max_pressed(on),
                Event::Maximized(on) => g.set_window_maximized(on),
            }
        }
    });
    let weak = main.as_weak();
    let popup_open = Rc::new(move || {
        let Some(main) = weak.upgrade() else {
            return false;
        };
        POPUPS.with(|p| {
            let Ok(mut p) = p.try_borrow_mut() else {
                return true;
            };
            p.clear();
            rbs_app::popups::open_in(main.window(), &mut p);
            !p.is_empty()
        })
    });
    if caption::install(hwnd, sink, popup_open) {
        let g = main.global::<Shell>();
        g.set_window_maximized(caption::maximized());
        g.set_custom_caption(true);
    }
}
