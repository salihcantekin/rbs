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
use rbs_gpu::share::{
    LayerRect, MAX_CUTOUTS, MAX_MARKS, Mark, PreviewEndpoint, PreviewFrame, PreviewLayer,
    PreviewSubscriber,
};
use rbs_ui::layout::Rect;
use rbs_ui::transform::{self, MarkKind};
use slint::ComponentHandle;

use crate::studio_ui::log;
use crate::ui::{MainWindow, Shell, Studio, Theme};

// Every mark of a selection reaches the layer.
const _: () = assert!(transform::MAX_MARKS <= MAX_MARKS);

/// How long connecting to the in-process publisher may take before it is given up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

struct Preview {
    layer: PreviewLayer,
    /// The marks last given to the layer, in its units.
    marks: Vec<Mark>,
    /// Scratch for what Slint draws over the layer (window logical pixels) and the same in
    /// the layer's units; reused.
    covers: Vec<Rect>,
    cutouts: Vec<LayerRect>,
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
    let generation = GENERATION.load(std::sync::atomic::Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("rbs-preview-connect".into())
        .spawn(
            move || match PreviewSubscriber::connect(&endpoint, CONNECT_TIMEOUT) {
                Ok(sub) => {
                    let d = done.clone();
                    let _ = crate::entry::invoke(move || setup(sub, d, generation));
                }
                Err(e) => {
                    log(&format!("preview: cannot connect: {e}"));
                    let _ = crate::entry::invoke(move || done(false));
                }
            },
        );
    if let Err(e) = spawned {
        log(&format!("preview: no thread: {e}"));
    }
}

/// Bumped by [`close`]: a frame thread of an earlier connection ends within one receive
/// timeout and drops its subscriber, so the studio's publisher does no preview work
/// while nothing shows it (P13; ADR-0056 decision 5). Its end is then not logged.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn setup(mut sub: PreviewSubscriber, done: impl Fn(bool), generation: u64) {
    let current = move || GENERATION.load(std::sync::atomic::Ordering::Relaxed) == generation;
    // Closed while connecting (the window was hidden): the subscriber goes away unused.
    if !current() {
        return;
    }
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
            marks: Vec::with_capacity(MAX_MARKS),
            covers: Vec::with_capacity(MAX_CUTOUTS),
            cutouts: Vec::with_capacity(MAX_CUTOUTS),
            attached: false,
            clock: MonotonicClock::new(),
            timer: slint::Timer::default(),
        });
    });
    // Frames arrive on their own thread; only the newest is shown.
    let spawned = std::thread::Builder::new()
        .name("rbs-preview".into())
        .spawn(move || {
            while current() {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        // A frame of an earlier connection never reaches a newer layer.
                        let shown = crate::entry::invoke(move || {
                            if current() {
                                show(frame);
                            }
                        });
                        if shown.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // At quit the studio stops after `close`; that end is expected.
                        if current() {
                            log(&format!("preview: the studio's preview ended: {e}"));
                        }
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
/// gives it what Slint draws over the window (in-window popups, the bottom toast, the
/// error bar), which the layer leaves out of the picture and the marks (ADR-0054). The
/// picture never hides for them (on Windows until the hardware check of ADR-0054
/// decision 5, it does while one overlaps it). Called after every view sync and before
/// every redraw of the main window (popups open, move and close with a redraw);
/// unchanged placement and covers make no layer call.
pub fn place(window: &MainWindow) {
    PREVIEW.with(|p| {
        // Never nested in practice; a redraw inside a layer call would skip one placement.
        let Ok(mut p) = p.try_borrow_mut() else {
            return;
        };
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
        let Some([x, y, w, h]) = AREA.with(Cell::get).filter(|a| a[2] > 0.0 && a[3] > 0.0) else {
            st.layer.set_hidden(true);
            return;
        };
        // Core Animation places in points (logical pixels); DirectComposition in
        // physical pixels.
        #[cfg(target_os = "macos")]
        let scale = 1.0f32;
        #[cfg(windows)]
        let scale = window.window().scale_factor();
        let in_layer = |r: Rect| LayerRect {
            x: f64::from(r.x * scale),
            y: f64::from(r.y * scale),
            width: f64::from(r.w * scale),
            height: f64::from(r.h * scale),
        };
        // What Slint draws over the layer: its in-window popups, the bottom toast and the
        // error bar (ADR-0054 decision 2). The layer cuts them out.
        rbs_app::popups::open_in(window.window(), &mut st.covers);
        let shell = window.global::<Shell>();
        let toast = |c: crate::ui::Cover| Rect {
            x: c.x,
            y: c.y,
            w: c.width,
            h: c.height,
        };
        if shell.get_toast_kind() != 0 || !window.global::<Studio>().get_deleted().is_empty() {
            st.covers.push(toast(shell.get_bottom_toast()));
        }
        if shell.get_error_kind() != 0 {
            st.covers.push(toast(shell.get_top_toast()));
        }
        st.cutouts.clear();
        st.cutouts.extend(st.covers.iter().map(|&c| in_layer(c)));
        st.layer.set_rect(in_layer(Rect { x, y, w, h }));
        st.layer.set_cutouts(&st.cutouts);
        st.layer.set_hidden(false);
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
                    crate::entry::start(&st.timer, slint::TimerMode::SingleShot, after, retire);
                }
            }
        }
    });
}

/// Draws the selection over the picture (ADR-0048): `marks` in the Program panel's
/// coordinates, placed into the window like the layer, in the theme's colours. Does
/// nothing when they did not change.
pub fn set_marks(window: &MainWindow, marks: &[transform::Mark]) {
    let theme = window.global::<Theme>();
    let rgba = |c: slint::Color| {
        let c = c.to_argb_u8();
        [c.red, c.green, c.blue, c.alpha]
    };
    let accent = rgba(theme.get_accent());
    let fill = rgba(theme.get_surface_raised());
    #[cfg(target_os = "macos")]
    let scale = 1.0;
    #[cfg(windows)]
    let scale = f64::from(window.window().scale_factor());
    // Marks are clipped to the panel's picture area: past it they would cover other
    // panels, which the layer sits above.
    let Some(area) = AREA.with(Cell::get) else {
        return;
    };
    PREVIEW.with(|p| {
        let mut p = p.borrow_mut();
        let Some(st) = p.as_mut() else { return };
        let mut changed = false;
        let mut n = 0;
        for m in marks {
            let x0 = m.rect.x.max(0.0);
            let y0 = m.rect.y.max(0.0);
            let x1 = (m.rect.x + m.rect.w).min(area[2]);
            let y1 = (m.rect.y + m.rect.h).min(area[3]);
            if x1 <= x0 || y1 <= y0 || n == MAX_MARKS {
                continue;
            }
            let mark = Mark {
                x: f64::from(area[0] + x0) * scale,
                y: f64::from(area[1] + y0) * scale,
                width: f64::from(x1 - x0) * scale,
                height: f64::from(y1 - y0) * scale,
                rgba: if m.kind == MarkKind::HandleFill {
                    fill
                } else {
                    accent
                },
            };
            match st.marks.get_mut(n) {
                Some(old) if *old == mark => {}
                Some(old) => {
                    *old = mark;
                    changed = true;
                }
                None => {
                    st.marks.push(mark);
                    changed = true;
                }
            }
            n += 1;
        }
        if st.marks.len() != n {
            st.marks.truncate(n);
            changed = true;
        }
        if changed {
            st.layer.set_marks(&st.marks);
        }
    });
}

/// Drops the layer (its frames go back) before the studio stops, or while the main
/// window is hidden; the frame thread ends and unsubscribes. [`connect`] starts again.
pub fn close() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PREVIEW.with(|p| p.borrow_mut().take());
}
