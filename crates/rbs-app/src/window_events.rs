//! What each RBS window needs from its own window events: tooltips that close when
//! Slint would not tell their control (ADR-0094), and on Windows a window that stays
//! drawn while Slint builds a native context menu (`win::redraw`).
//!
//! ## Tooltips
//!
//! A tooltip is a layer of its window (`OverlayHost`), opened and closed by the pointer
//! entering and leaving its control. Some ways out never reach the control, and the
//! tooltip stayed on screen:
//!
//! - another window opens over the pointer or takes the focus: the old window may
//!   never hear the pointer leave;
//! - the control goes away under the pointer (a page change, a list rebuilt);
//! - the pointer moves onto a control without a tooltip.
//!
//! Each window therefore reports to its own `Overlays`: its activation (losing it
//! closes the tooltip, and an inactive window shows none, as the system tooltips of
//! Windows and macOS do), where the pointer moves (`pointer-at` closes the tooltip off
//! its control), and a press, a scroll or the pointer leaving the window (closes it).
//!
//! The activation is Slint's own window state, read through `i-slint-core` at the exact
//! version `slint` is pinned to, and watched with a change tracker: nothing runs until
//! it changes (P13). The pointer events come from winit before Slint handles them; with
//! no tooltip open, `pointer-at` returns at once.

use std::cell::RefCell;

use i_slint_core::properties::ChangeTracker;
use i_slint_core::window::WindowInner;
use rbs_app_ui::Overlays;
use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Global};

/// One watched window: its tracker lives while the window does.
struct Watch {
    alive: Box<dyn Fn() -> bool>,
    _tracker: ChangeTracker,
}

thread_local! {
    static WATCHES: RefCell<Vec<Watch>> = const { RefCell::new(Vec::new()) };
}

/// Makes `component`'s tooltips close when Slint would not tell their control, and on
/// Windows keeps its window drawn while a native menu is built. Call once per window,
/// on the UI thread, right after the window is made. Watches of windows dropped since
/// are cleared here.
pub fn install<C>(component: &C)
where
    C: ComponentHandle + 'static,
    for<'a> Overlays<'a>: Global<'a, C>,
{
    follow_activation(component);
    let weak = component.as_weak();
    #[cfg(windows)]
    let mut subclassed = false;
    component
        .window()
        .on_winit_window_event(move |window, event| {
            // The Win32 window exists once it sends events; its handle is read on the
            // next turn, outside Slint's event handling. Again on each activation, in
            // case the window was made again (subclassing twice only replaces it).
            #[cfg(windows)]
            if !subclassed || matches!(event, WindowEvent::Focused(true)) {
                subclassed = true;
                keep_visible_soon(weak.clone());
            }
            let close = match event {
                WindowEvent::CursorMoved { position, .. } => {
                    let scale = f64::from(window.scale_factor());
                    if let Some(c) = weak.upgrade() {
                        c.global::<Overlays>().invoke_pointer_at(
                            (position.x / scale) as f32,
                            (position.y / scale) as f32,
                        );
                    }
                    false
                }
                WindowEvent::CursorLeft { .. } | WindowEvent::MouseWheel { .. } => true,
                WindowEvent::MouseInput { state, .. } => *state == ElementState::Pressed,
                _ => false,
            };
            if close && let Some(c) = weak.upgrade() {
                c.global::<Overlays>().invoke_tip_close();
            }
            EventResult::Propagate
        });
}

/// Reports `component`'s window activation to its `Overlays` whenever it changes.
pub fn follow_activation<C>(component: &C)
where
    C: ComponentHandle + 'static,
    for<'a> Overlays<'a>: Global<'a, C>,
{
    let tracker = ChangeTracker::default();
    tracker.init(
        component.as_weak(),
        |weak| {
            weak.upgrade()
                .is_some_and(|c| WindowInner::from_pub(c.window()).active())
        },
        |weak, active| {
            if let Some(c) = weak.upgrade() {
                c.global::<Overlays>().invoke_window_activation(*active);
            }
        },
    );
    let weak = component.as_weak();
    WATCHES.with(|watches| {
        let mut watches = watches.borrow_mut();
        watches.retain(|w| (w.alive)());
        watches.push(Watch {
            alive: Box::new(move || weak.upgrade().is_some()),
            _tracker: tracker,
        });
    });
}

/// Subclasses `component`'s Win32 window (see `win::redraw`) on the next event loop
/// turn.
#[cfg(windows)]
fn keep_visible_soon<C: ComponentHandle + 'static>(weak: slint::Weak<C>) {
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(c) = weak.upgrade() else {
            return;
        };
        let handle = c.window().window_handle();
        let Ok(handle) = handle.window_handle() else {
            return;
        };
        if let RawWindowHandle::Win32(win32) = handle.as_raw() {
            crate::win::redraw::keep_visible(win32.hwnd);
        }
    });
}
