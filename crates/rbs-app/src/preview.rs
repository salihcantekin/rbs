//! The Program panel's picture: the studio's preview surfaces as a native layer over the
//! panel (a Core Animation layer on macOS, ADR-0034; a DirectComposition visual on
//! Windows, ADR-0036). A new frame changes the layer only; Slint does not redraw.
//!
//! UI thread only (the layer is not `Send`); the subscriber receives frames on its own
//! thread and hands each to the UI thread.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rbs_core::time::{Clock, MonotonicClock};
use rbs_gpu::share::{LayerRect, PreviewEndpoint, PreviewFrame, PreviewLayer, PreviewSubscriber};
use slint::ComponentHandle;

use crate::studio_ui::log;
use crate::ui::{MainWindow, Shell, Studio};

/// How long connecting to the in-process publisher may take before it is given up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

struct Preview {
    layer: PreviewLayer,
    attached: bool,
    clock: MonotonicClock,
    timer: slint::Timer,
}

thread_local! {
    static PREVIEW: RefCell<Option<Preview>> = const { RefCell::new(None) };
    /// The Program panel's content in the main window (logical pixels), if shown there.
    static AREA: Cell<Option<[f32; 4]>> = const { Cell::new(None) };
}

/// Connects to `endpoint` off the UI thread, then makes the layer on the UI thread.
/// `done(true)` once the layer exists, `done(false)` if this session cannot show it.
pub fn connect(endpoint: PreviewEndpoint, done: impl Fn(bool) + Send + Clone + 'static) {
    let spawned = std::thread::Builder::new()
        .name("rbs-preview-connect".into())
        .spawn(
            move || match PreviewSubscriber::connect(&endpoint, CONNECT_TIMEOUT) {
                Ok(sub) => {
                    let d = done.clone();
                    let _ = slint::invoke_from_event_loop(move || setup(sub, d));
                }
                Err(e) => {
                    log(&format!("preview: cannot connect: {e}"));
                    let _ = slint::invoke_from_event_loop(move || done(false));
                }
            },
        );
    if let Err(e) = spawned {
        log(&format!("preview: no thread: {e}"));
    }
}

fn setup(mut sub: PreviewSubscriber, done: impl Fn(bool)) {
    #[cfg(target_os = "macos")]
    let made: Result<PreviewLayer, rbs_gpu::GpuError> = Ok(PreviewLayer::new(&sub));
    #[cfg(windows)]
    let made = PreviewLayer::new(&sub);
    let layer = match made {
        Ok(l) => l,
        Err(e) => {
            log(&format!("preview: no layer in this session: {e}"));
            done(false);
            return;
        }
    };
    PREVIEW.with(|p| {
        *p.borrow_mut() = Some(Preview {
            layer,
            attached: false,
            clock: MonotonicClock::new(),
            timer: slint::Timer::default(),
        });
    });
    // Frames arrive on their own thread; only the newest is shown.
    let spawned = std::thread::Builder::new()
        .name("rbs-preview".into())
        .spawn(move || {
            loop {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        if slint::invoke_from_event_loop(move || show(frame)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log(&format!("preview: the studio's preview ended: {e}"));
                        break;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log(&format!("preview: no thread: {e}"));
    }
    done(true);
}

/// Sets where the Program panel's content is in the main window (`None`: not shown
/// there).
pub fn set_area(area: Option<[f32; 4]>) {
    AREA.with(|a| a.set(area));
}

/// Attaches the layer once the window exists, places it over the Program panel and
/// hides it while toolkit content would be covered by it (menus, toasts, the error bar).
/// Called after every view sync and whenever a popup opens or closes.
pub fn place(window: &MainWindow) {
    PREVIEW.with(|p| {
        let mut p = p.borrow_mut();
        let Some(st) = p.as_mut() else { return };
        if !st.attached {
            let window_handle = window.window().window_handle();
            let Ok(handle) = window_handle.window_handle() else {
                return;
            };
            #[cfg(target_os = "macos")]
            let attach = {
                let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                    return;
                };
                // SAFETY: the handle is the live NSView of this window, which the renderer
                // made layer-backed; this runs on the main thread that owns it.
                unsafe { st.layer.attach_to_ns_view(appkit.ns_view) }
            };
            #[cfg(windows)]
            let attach = {
                let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                    return;
                };
                // SAFETY: the handle is the live top-level window of this process; this
                // runs on the UI thread that owns it.
                unsafe { st.layer.attach_to_hwnd(win32.hwnd) }
            };
            if let Err(e) = attach {
                log(&format!("preview: cannot attach the layer: {e}"));
                return;
            }
            st.attached = true;
        }
        let shell = window.global::<Shell>();
        let studio = window.global::<Studio>();
        let covered = studio.get_overlays() > 0
            || shell.get_toast_kind() != 0
            || shell.get_error_kind() != 0
            || !studio.get_deleted().is_empty();
        match AREA.with(Cell::get) {
            Some([x, y, w, h]) if !covered && w > 0.0 && h > 0.0 => {
                // Core Animation places in points (logical pixels); DirectComposition in
                // physical pixels.
                #[cfg(target_os = "macos")]
                let scale = 1.0;
                #[cfg(windows)]
                let scale = f64::from(window.window().scale_factor());
                st.layer.set_rect(LayerRect {
                    x: f64::from(x) * scale,
                    y: f64::from(y) * scale,
                    width: f64::from(w) * scale,
                    height: f64::from(h) * scale,
                });
                st.layer.set_hidden(false);
            }
            _ => st.layer.set_hidden(true),
        }
    });
}

fn show(frame: PreviewFrame) {
    PREVIEW.with(|p| {
        if let Some(st) = p.borrow_mut().as_mut() {
            let now = st.clock.now();
            st.layer.show(frame, now);
        }
    });
    retire();
}

/// Releases replaced frames the compositor no longer reads. Never waits; while frames
/// wait, a one-shot timer comes back at the oldest one's due time.
fn retire() {
    PREVIEW.with(|p| {
        let mut p = p.borrow_mut();
        let Some(st) = p.as_mut() else { return };
        let now = st.clock.now();
        st.layer.retire(now);
        match st.layer.next_retire() {
            None => st.timer.stop(),
            Some(due) => {
                if !st.timer.running() {
                    let after = due.saturating_since(now).max(Duration::from_millis(1));
                    st.timer.start(slint::TimerMode::SingleShot, after, retire);
                }
            }
        }
    });
}

/// Drops the layer (its frames go back) before the studio stops.
pub fn close() {
    PREVIEW.with(|p| p.borrow_mut().take());
}
