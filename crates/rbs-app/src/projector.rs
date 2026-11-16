//! Projector windows (ADR-0125): the program of the main or the vertical canvas in a
//! window of its own, to share in a video call or to put on another display.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Projector window".
//!
//! - The picture is the studio's projector picture at the canvas's size (the host adds
//!   it while the projector is open, decision 2), shown as a native layer over the
//!   whole window, like the Program picture (ADR-0034, ADR-0036). Nothing else is drawn
//!   in the window: a video call captures all of it (decision 5).
//! - The window has no chrome and no minimise button (decisions 5 and 7). A press moves
//!   it; on Windows, where it has no frame, a press near an edge resizes it. Its menu is
//!   the native `CommandMenu` (decision 6). Esc closes it.
//! - Sizes (decision 8): Window, Exact pixels (`canvas / k` physical pixels) and full
//!   screen on a display. A resize by hand turns Exact pixels back to Window.
//! - One projector per canvas; the host knows it by [`host_id`]. With no projector open
//!   nothing here runs (P13): no window, layer, thread or timer.
//!
//! UI thread only. The frame thread of each picture hands frames to the UI thread.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(any(target_os = "macos", windows))]
use rbs_core::time::Clock;
use rbs_core::time::MonotonicClock;
use rbs_gpu::share::PreviewEndpoint;
#[cfg(any(target_os = "macos", windows))]
use rbs_gpu::share::{PreviewFrame, PreviewSubscriber};
use rbs_ui::projector::{
    ProjectorCanvas, ProjectorContent, ProjectorSize, exact_size, shared_size, shared_text,
};
use rbs_ui::workspace::Placement;
use slint::ComponentHandle;
use slint::winit_030::winit;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::keyboard::{Key, NamedKey};

use crate::entry;
use crate::studio_ui::log;
use crate::ui::{ProjectorWindow, Tokens};

/// Every projector window's title starts with this.
const TITLE_PREFIX: &str = "RBS Projector";

/// How long connecting to the projector's picture may take before it is given up.
#[cfg(any(target_os = "macos", windows))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The id the studio host knows a canvas's projector by: one projector per canvas.
pub fn host_id(canvas: ProjectorCanvas) -> u32 {
    match canvas {
        ProjectorCanvas::Main => 0,
        ProjectorCanvas::Vertical => 1,
    }
}

/// The canvas of a host id.
pub fn canvas_of(id: u32) -> Option<ProjectorCanvas> {
    match id {
        0 => Some(ProjectorCanvas::Main),
        1 => Some(ProjectorCanvas::Vertical),
        _ => None,
    }
}

/// The window title, which video calls list the window by (never drawn).
pub fn title(canvas: ProjectorCanvas) -> String {
    match canvas {
        ProjectorCanvas::Main => format!("{TITLE_PREFIX} – Program"),
        ProjectorCanvas::Vertical => format!("{TITLE_PREFIX} – Vertical"),
    }
}

/// `canvas=main` or `canvas=vertical`: what the registry's projector commands take.
pub fn param(canvas: ProjectorCanvas) -> &'static str {
    match canvas {
        ProjectorCanvas::Main => "main",
        ProjectorCanvas::Vertical => "vertical",
    }
}

/// The canvas a command's `canvas` parameter names.
pub fn parse_param(value: &str) -> Option<ProjectorCanvas> {
    match value {
        "main" => Some(ProjectorCanvas::Main),
        "vertical" => Some(ProjectorCanvas::Vertical),
        _ => None,
    }
}

/// The window a projector starts as (decisions 5 and 7), from the backend's window
/// attributes hook; every other window passes unchanged. The hook runs when a window
/// component is created, before it has its title, so [`open_slot`] marks the one it
/// creates ([`MAKING`]). Not focused when it opens, no minimise or zoom button; on
/// macOS a hidden, transparent title bar over the full content (so the window still
/// resizes at its edges), on Windows no frame.
pub fn attributes(a: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    if !MAKING.with(Cell::get) {
        return a;
    }
    let a = a
        .with_active(false)
        .with_enabled_buttons(winit::window::WindowButtons::CLOSE);
    #[cfg(target_os = "macos")]
    let a = {
        use winit::platform::macos::WindowAttributesExtMacOS;
        a.with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_titlebar_buttons_hidden(true)
            .with_fullsize_content_view(true)
    };
    #[cfg(windows)]
    let a = a.with_decorations(false);
    a
}

/// What a projector tells the shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Esc, or the OS asked to close it: close the projector.
    Close(ProjectorCanvas),
    /// Its size or display changed (a resize by hand turns Exact pixels to Window):
    /// save the content and placement.
    Changed(ProjectorCanvas),
}

/// How the menu and the registry see an open projector.
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    /// Its canvas.
    pub canvas: ProjectorCanvas,
    /// "Shared at 1920 × 1080", from the window's physical pixels now.
    pub shared: String,
    /// Its size.
    pub size: ProjectorSize,
    /// The display it is full screen on, by name.
    pub fullscreen: Option<String>,
}

// Made only where pictures exist (macOS, Windows).
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
struct Layer {
    #[cfg(any(target_os = "macos", windows))]
    layer: rbs_gpu::share::PreviewLayer,
    attached: bool,
    clock: MonotonicClock,
    timer: slint::Timer,
}

struct Slot {
    canvas: ProjectorCanvas,
    content: ProjectorContent,
    window: ProjectorWindow,
    /// The canvas's size in pixels (Exact pixels and the shared size use it).
    canvas_size: [u32; 2],
    /// The display it is full screen on, by name.
    fullscreen: Option<String>,
    /// The placement the workspace had: kept while its display is missing, so a save
    /// does not lose it (ADR-0125 round 2, N2).
    saved: Option<Placement>,
    /// Exact pixels: the fraction in use.
    exact_k: Option<u32>,
    /// A size this module asked for: its resize event is not a resize by hand.
    asked: Rc<Cell<Option<[u32; 2]>>>,
    cursor: slint::Timer,
    layer: Option<Layer>,
    /// Bumped to end the frame thread of the connection before.
    generation: Arc<AtomicU64>,
    endpoint: Option<PreviewEndpoint>,
}

/// Where projector events go.
type Sink = Rc<dyn Fn(Event)>;

thread_local! {
    static SLOTS: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
    /// Set while [`open_slot`] creates a projector window, for [`attributes`].
    static MAKING: Cell<bool> = const { Cell::new(false) };
    /// Where the shell's events go, set once by [`install`].
    static EVENTS: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Where projector events go (the shell's `Ctx::update`).
pub fn install(events: impl Fn(Event) + 'static) {
    EVENTS.with(|e| *e.borrow_mut() = Some(Rc::new(events)));
}

fn emit(event: Event) {
    let sink = EVENTS.with(|e| e.borrow().clone());
    if let Some(sink) = sink {
        sink(event);
    }
}

/// The projectors that should be open, with their placements and the canvas sizes:
/// opens a window for each new one (`make` sets up its globals), closes windows no
/// longer wanted, and applies a changed size. Returns the canvases opened and closed,
/// for the host.
pub fn sync(
    wanted: &[(ProjectorContent, Option<Placement>, [u32; 2])],
    make: &dyn Fn(&ProjectorWindow),
) -> (Vec<ProjectorCanvas>, Vec<ProjectorCanvas>) {
    let mut opened = Vec::new();
    let mut closed = Vec::new();
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        slots.retain_mut(|s| {
            let keep = wanted.iter().any(|(c, ..)| c.canvas == s.canvas);
            if !keep {
                close_slot(s);
                closed.push(s.canvas);
            }
            keep
        });
        for (content, placement, canvas_size) in wanted {
            if let Some(s) = slots.iter_mut().find(|s| s.canvas == content.canvas) {
                s.canvas_size = *canvas_size;
                if s.content.size != content.size {
                    s.content = content.clone();
                    apply_size(s);
                }
                continue;
            }
            let Some(slot) = open_slot(content, placement.clone(), *canvas_size, make) else {
                continue;
            };
            opened.push(content.canvas);
            slots.push(slot);
        }
    });
    (opened, closed)
}

fn open_slot(
    content: &ProjectorContent,
    placement: Option<Placement>,
    canvas_size: [u32; 2],
    make: &dyn Fn(&ProjectorWindow),
) -> Option<Slot> {
    MAKING.with(|m| m.set(true));
    let made = ProjectorWindow::new();
    MAKING.with(|m| m.set(false));
    let window = match made {
        Ok(w) => w,
        Err(e) => {
            log(&format!("projector: no window: {e}"));
            return None;
        }
    };
    let canvas = content.canvas;
    window.set_window_title(title(canvas).into());
    window.set_menu_target(format!("canvas={}", param(canvas)).into());
    make(&window);
    // A window as wide as a floating window, in the canvas's shape.
    let t = window.global::<Tokens>();
    let w = t.get_size_floating_w();
    let h = if canvas_size[0] > 0 {
        w * canvas_size[1] as f32 / canvas_size[0] as f32
    } else {
        t.get_size_floating_h()
    };
    window
        .window()
        .set_size(slint::WindowSize::Logical(slint::LogicalSize::new(w, h)));
    let asked = Rc::new(Cell::new(None));
    install_events(&window, canvas, Rc::clone(&asked));
    let place = window.as_weak();
    let set = entry::set_rendering_notifier(window.window(), move |state, _| {
        if matches!(state, slint::RenderingState::BeforeRendering)
            && let Some(w) = place.upgrade()
        {
            place_layer(canvas, &w);
        }
    });
    if let Err(e) = set {
        log(&format!("projector: no rendering notifier: {e:?}"));
    }
    entry::on_close_requested(window.window(), move || {
        emit(Event::Close(canvas));
        slint::CloseRequestResponse::HideWindow
    });
    if let Err(e) = window.show() {
        log(&format!("projector: cannot show: {e}"));
        return None;
    }
    #[cfg(windows)]
    keep_visible(&window);
    let fullscreen = placement
        .as_ref()
        .filter(|p| p.fullscreen)
        .and_then(|p| p.connector.clone());
    let mut slot = Slot {
        canvas,
        content: content.clone(),
        window,
        canvas_size,
        fullscreen: None,
        saved: placement,
        exact_k: None,
        asked,
        cursor: slint::Timer::default(),
        layer: None,
        generation: Arc::new(AtomicU64::new(0)),
        endpoint: None,
    };
    match fullscreen {
        Some(name) => set_fullscreen_on(&mut slot, Some(name)),
        None => apply_size(&mut slot),
    }
    Some(slot)
}

fn close_slot(s: &mut Slot) {
    s.generation.fetch_add(1, Ordering::Relaxed);
    s.layer = None;
    s.cursor.stop();
    let _ = s.window.hide();
}

/// Closes every projector window and drops its layer (the frames go back before the
/// studio stops). The workspace keeps them.
pub fn close_all() {
    SLOTS.with(|slots| {
        for mut s in slots.borrow_mut().drain(..) {
            close_slot(&mut s);
        }
    });
}

/// Drops every projector's layer (its frames go back) before the studio stops; the
/// windows stay black until the host sends the new pictures after the restart.
pub fn drop_pictures() {
    SLOTS.with(|slots| {
        for s in slots.borrow_mut().iter_mut() {
            s.generation.fetch_add(1, Ordering::Relaxed);
            s.layer = None;
            s.endpoint = None;
        }
    });
}

/// The host's news of a projector's picture: connect to a new endpoint, or drop the
/// layer while there is none (suspended).
pub fn picture(canvas: ProjectorCanvas, endpoint: Option<PreviewEndpoint>) {
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(s) = slots.iter_mut().find(|s| s.canvas == canvas) else {
            return;
        };
        if s.endpoint == endpoint {
            return;
        }
        s.generation.fetch_add(1, Ordering::Relaxed);
        s.layer = None;
        s.endpoint.clone_from(&endpoint);
        if let Some(endpoint) = endpoint {
            connect(canvas, endpoint, Arc::clone(&s.generation));
        }
    });
}

/// The open projectors as the menu shows them.
pub fn shown() -> Vec<Shown> {
    SLOTS.with(|slots| {
        slots
            .borrow()
            .iter()
            .map(|s| {
                let size = s.window.window().size();
                let shared = shared_size([size.width, size.height], s.canvas_size);
                Shown {
                    canvas: s.canvas,
                    shared: shared_text(shared, s.canvas_size, s.exact_k),
                    size: s.content.size,
                    fullscreen: s.fullscreen.clone(),
                }
            })
            .collect()
    })
}

/// The names of the connected displays, for Full screen on a display.
pub fn displays() -> Vec<String> {
    SLOTS.with(|slots| {
        let slots = slots.borrow();
        let Some(s) = slots.first() else {
            return Vec::new();
        };
        s.window
            .window()
            .with_winit_window(|w| {
                w.available_monitors()
                    .filter_map(|m| m.name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    })
}

/// The content and placement to save for a projector now.
pub fn saved(canvas: ProjectorCanvas) -> Option<(ProjectorContent, Option<Placement>)> {
    SLOTS.with(|slots| {
        let slots = slots.borrow();
        let s = slots.iter().find(|s| s.canvas == canvas)?;
        let placement = match &s.fullscreen {
            Some(name) => Some(Placement {
                connector: Some(name.clone()),
                fullscreen: true,
                ..Placement::default()
            }),
            None => s.saved.clone().filter(|p| !p.fullscreen),
        };
        Some((s.content.clone(), placement))
    })
}

/// Sets a projector's size from its menu: Window, Exact pixels, or full screen on the
/// display named `display`.
pub fn set_size(canvas: ProjectorCanvas, size: ProjectorSize, display: Option<String>) {
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(s) = slots.iter_mut().find(|s| s.canvas == canvas) else {
            return;
        };
        s.saved = None;
        match display {
            Some(name) => set_fullscreen_on(s, Some(name)),
            None => {
                if s.fullscreen.is_some() {
                    set_fullscreen_on(s, None);
                }
                s.content = s.content.clone().with_size(size);
                apply_size(s);
            }
        }
    });
    emit(Event::Changed(canvas));
}

/// Brings a projector to the front (a second open of the same canvas, decision 9).
pub fn raise(canvas: ProjectorCanvas) {
    SLOTS.with(|slots| {
        if let Some(s) = slots.borrow().iter().find(|s| s.canvas == canvas) {
            s.window.window().with_winit_window(|w| w.focus_window());
        }
    });
}

/// Exact pixels: the window's content becomes `canvas / k` physical pixels on its
/// display (decision 8). Window size leaves the window as it is.
fn apply_size(s: &mut Slot) {
    if s.content.size != ProjectorSize::Exact || s.fullscreen.is_some() {
        s.exact_k = None;
        return;
    }
    let canvas = s.canvas_size;
    let asked = Rc::clone(&s.asked);
    let k = s.window.window().with_winit_window(|w| {
        let visible = w
            .current_monitor()
            .and_then(|m| visible_area(&m))
            .unwrap_or([canvas[0], canvas[1]]);
        let (size, k) = exact_size(canvas, visible);
        asked.set(Some(size));
        let _ = w.request_inner_size(winit::dpi::PhysicalSize::new(size[0], size[1]));
        k
    });
    s.exact_k = k;
}

/// Full screen on the display named `name` (macOS: a borderless window over the whole
/// display, not a Space; Windows: borderless over the monitor), or back to a window.
fn set_fullscreen_on(s: &mut Slot, name: Option<String>) {
    let found = s.window.window().with_winit_window(|w| {
        let monitor = name.as_ref().and_then(|n| {
            w.available_monitors()
                .find(|m| m.name().as_ref() == Some(n))
        });
        match (&name, monitor) {
            (Some(_), Some(m)) => {
                #[cfg(target_os = "macos")]
                {
                    use winit::platform::macos::WindowExtMacOS;
                    w.set_outer_position(m.position());
                    w.set_simple_fullscreen(true);
                }
                #[cfg(not(target_os = "macos"))]
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(Some(m))));
                true
            }
            _ => {
                #[cfg(target_os = "macos")]
                {
                    use winit::platform::macos::WindowExtMacOS;
                    w.set_simple_fullscreen(false);
                }
                #[cfg(not(target_os = "macos"))]
                w.set_fullscreen(None);
                false
            }
        }
    });
    s.fullscreen = if found == Some(true) { name } else { None };
    s.exact_k = None;
}

/// The display's area windows may use, in physical pixels (without the menu bar, Dock
/// or taskbar).
#[cfg(target_os = "macos")]
fn visible_area(m: &winit::monitor::MonitorHandle) -> Option<[u32; 2]> {
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::CGRect;
    use winit::platform::macos::MonitorHandleExtMacOS;
    let screen = m.ns_screen()?.cast::<AnyObject>();
    // SAFETY: `screen` is the NSScreen winit found for this monitor, kept alive by
    // AppKit's screen list for this turn of the main thread, which this runs on;
    // `visibleFrame` and `backingScaleFactor` take no arguments and return plain values.
    let (frame, scale): (CGRect, f64) = unsafe {
        let screen = &*screen;
        (
            objc2::msg_send![screen, visibleFrame],
            objc2::msg_send![screen, backingScaleFactor],
        )
    };
    Some([
        (frame.size.width * scale) as u32,
        (frame.size.height * scale) as u32,
    ])
}

#[cfg(windows)]
fn visible_area(m: &winit::monitor::MonitorHandle) -> Option<[u32; 2]> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    use winit::platform::windows::MonitorHandleExtWindows;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };
    // SAFETY: the monitor handle comes from winit for a connected monitor and `info`
    // is a MONITORINFO with its size set, as GetMonitorInfoW requires.
    let ok = unsafe { GetMonitorInfoW(m.hmonitor() as _, &mut info) };
    if ok == 0 {
        return None;
    }
    let r = info.rcWork;
    Some([
        u32::try_from(r.right - r.left).ok()?,
        u32::try_from(r.bottom - r.top).ok()?,
    ])
}

#[cfg(not(any(target_os = "macos", windows)))]
fn visible_area(m: &winit::monitor::MonitorHandle) -> Option<[u32; 2]> {
    let s = m.size();
    Some([s.width, s.height])
}

/// The window's own events, before Slint handles them: a press moves the window (on
/// Windows near an edge it resizes it), a right click opens the menu, Esc closes, the
/// pointer hides after a rest, and a resize by hand leaves Exact pixels.
fn install_events(
    window: &ProjectorWindow,
    canvas: ProjectorCanvas,
    asked: Rc<Cell<Option<[u32; 2]>>>,
) {
    let weak = window.as_weak();
    let at = Rc::new(Cell::new((0.0f64, 0.0f64)));
    window.window().on_winit_window_event(move |sw, event| {
        let Some(pw) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        let _ = rbs_crash::contain(|| {
            sw.with_winit_window(|w| on_event(w, event, canvas, &pw, &at, &asked))
        });
        EventResult::Propagate
    });
}

fn on_event(
    w: &winit::window::Window,
    event: &WindowEvent,
    canvas: ProjectorCanvas,
    pw: &ProjectorWindow,
    at: &Cell<(f64, f64)>,
    asked: &Cell<Option<[u32; 2]>>,
) {
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            at.set((position.x, position.y));
            w.set_cursor_visible(true);
            rest_cursor(canvas, pw);
        }
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } => {
            #[cfg(windows)]
            if let Some(dir) = edge_at(w, at.get(), pw) {
                let _ = w.drag_resize_window(dir);
                return;
            }
            let _ = w.drag_window();
        }
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Right,
            ..
        } => {
            let scale = w.scale_factor();
            let (x, y) = at.get();
            open_menu(pw, (x / scale) as f32, (y / scale) as f32);
        }
        WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => emit(Event::Close(canvas)),
                Key::Named(NamedKey::ContextMenu) => {
                    let size = pw.window().size().to_logical(pw.window().scale_factor());
                    open_menu(pw, size.width / 2.0, size.height / 2.0);
                }
                _ => {}
            }
        }
        WindowEvent::Resized(size) => {
            let mine = asked.get() == Some([size.width, size.height]);
            asked.set(None);
            if !mine {
                resized_by_hand(canvas);
            }
        }
        WindowEvent::ScaleFactorChanged { .. } => rescale(canvas),
        _ => {}
    }
}

/// Opens the projector's menu after this event: the native menu runs its own loop,
/// which must not start inside the window's event handler.
fn open_menu(pw: &ProjectorWindow, x: f32, y: f32) {
    let weak = pw.as_weak();
    entry::single_shot(Duration::ZERO, move || {
        if let Some(pw) = weak.upgrade() {
            pw.invoke_open_menu(x, y);
        }
    });
}

/// The pointer hides after `delay.projector-cursor` without moving over the picture.
fn rest_cursor(canvas: ProjectorCanvas, pw: &ProjectorWindow) {
    let delay = pw.global::<Tokens>().get_delay_projector_cursor();
    let after = Duration::from_millis(u64::try_from(delay).unwrap_or(0));
    SLOTS.with(|slots| {
        let Ok(slots) = slots.try_borrow() else {
            return;
        };
        let Some(s) = slots.iter().find(|s| s.canvas == canvas) else {
            return;
        };
        let weak = s.window.as_weak();
        entry::start(&s.cursor, slint::TimerMode::SingleShot, after, move || {
            if let Some(w) = weak.upgrade() {
                w.window()
                    .with_winit_window(|w| w.set_cursor_visible(false));
            }
        });
    });
}

/// A resize the user made: Exact pixels becomes Window (decision 8).
fn resized_by_hand(canvas: ProjectorCanvas) {
    let changed = SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return false;
        };
        let Some(s) = slots.iter_mut().find(|s| s.canvas == canvas) else {
            return false;
        };
        if s.content.size != ProjectorSize::Exact || s.fullscreen.is_some() {
            return false;
        }
        s.content = s.content.clone().with_size(ProjectorSize::Window);
        s.exact_k = None;
        true
    });
    if changed {
        emit(Event::Changed(canvas));
    }
}

/// The window moved to a display of another scale: Exact pixels is computed again.
fn rescale(canvas: ProjectorCanvas) {
    // After this event: the window's monitor is the new one then.
    entry::single_shot(Duration::ZERO, move || {
        SLOTS.with(|slots| {
            if let Ok(mut slots) = slots.try_borrow_mut()
                && let Some(s) = slots.iter_mut().find(|s| s.canvas == canvas)
            {
                apply_size(s);
            }
        });
    });
}

/// Which edge of a frameless window (Windows) the pointer is on, within
/// `size.projector.resize-edge`.
#[cfg(windows)]
fn edge_at(
    w: &winit::window::Window,
    at: (f64, f64),
    pw: &ProjectorWindow,
) -> Option<winit::window::ResizeDirection> {
    use winit::window::ResizeDirection as D;
    let edge = f64::from(pw.global::<Tokens>().get_size_projector_resize_edge())
        * f64::from(w.scale_factor());
    let size = w.inner_size();
    let (x, y) = at;
    let left = x < edge;
    let right = x > f64::from(size.width) - edge;
    let top = y < edge;
    let bottom = y > f64::from(size.height) - edge;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => D::NorthWest,
        (_, true, true, _) => D::NorthEast,
        (true, _, _, true) => D::SouthWest,
        (_, true, _, true) => D::SouthEast,
        (true, ..) => D::West,
        (_, true, ..) => D::East,
        (_, _, true, _) => D::North,
        (_, _, _, true) => D::South,
        _ => return None,
    })
}

/// Building a native menu must not drop the window out of the compositor: that would
/// be a missing frame in the share (`win::redraw`, ADR-0125 decision 6).
#[cfg(windows)]
fn keep_visible(window: &ProjectorWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window().window_handle();
    if let Ok(handle) = handle.window_handle()
        && let RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        rbs_app::win::redraw::keep_visible(win32.hwnd);
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn connect(_: ProjectorCanvas, _: PreviewEndpoint, _: Arc<AtomicU64>) {}

/// Connects a projector to its picture off the UI thread, then makes its layer on the
/// UI thread; frames follow on a thread of their own until the generation changes.
#[cfg(any(target_os = "macos", windows))]
fn connect(canvas: ProjectorCanvas, endpoint: PreviewEndpoint, generation: Arc<AtomicU64>) {
    let at = generation.load(Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("rbs-projector-connect".into())
        .spawn(
            move || match PreviewSubscriber::connect(&endpoint, CONNECT_TIMEOUT) {
                Ok(sub) => {
                    let _ = entry::invoke(move || setup(canvas, sub, generation, at));
                }
                Err(e) => log(&format!("projector: cannot connect: {e}")),
            },
        );
    if let Err(e) = spawned {
        log(&format!("projector: no thread: {e}"));
    }
}

#[cfg(any(target_os = "macos", windows))]
fn setup(canvas: ProjectorCanvas, mut sub: PreviewSubscriber, generation: Arc<AtomicU64>, at: u64) {
    let current = {
        let generation = Arc::clone(&generation);
        move || generation.load(Ordering::Relaxed) == at
    };
    if !current() {
        return;
    }
    #[cfg(target_os = "macos")]
    let made: Result<rbs_gpu::share::PreviewLayer, rbs_gpu::GpuError> =
        Ok(rbs_gpu::share::PreviewLayer::new(&sub));
    #[cfg(windows)]
    let made = rbs_gpu::share::PreviewLayer::new(&sub);
    let layer = match made {
        Ok(l) => l,
        Err(e) => {
            log(&format!("projector: no layer in this session: {e}"));
            return;
        }
    };
    let window = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let s = slots.iter_mut().find(|s| s.canvas == canvas)?;
        s.layer = Some(Layer {
            layer,
            attached: false,
            clock: MonotonicClock::new(),
            timer: slint::Timer::default(),
        });
        Some(s.window.as_weak())
    });
    // The first frame needs the layer placed: ask the window for a redraw.
    if let Some(w) = window.and_then(|w| w.upgrade()) {
        w.window().request_redraw();
    }
    let spawned = std::thread::Builder::new()
        .name("rbs-projector".into())
        .spawn(move || {
            while current() {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        let current = current.clone();
                        let shown = entry::invoke(move || {
                            if current() {
                                show(canvas, frame);
                            }
                        });
                        if shown.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if current() {
                            log(&format!("projector: the picture ended: {e}"));
                        }
                        break;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log(&format!("projector: no thread: {e}"));
    }
}

/// Attaches the layer once the window exists and covers the whole window with it; the
/// layer keeps the canvas's aspect (resize-aspect), the backdrop shows around it.
#[cfg(any(target_os = "macos", windows))]
fn place_layer(canvas: ProjectorCanvas, window: &ProjectorWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rbs_gpu::share::LayerRect;
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        let Some(st) = slots
            .iter_mut()
            .find(|s| s.canvas == canvas)
            .and_then(|s| s.layer.as_mut())
        else {
            return;
        };
        if !st.attached {
            let handle = window.window().window_handle();
            let Ok(handle) = handle.window_handle() else {
                return;
            };
            #[cfg(target_os = "macos")]
            let attach = {
                let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                    return;
                };
                // SAFETY: the handle is the live NSView of this window, which the
                // renderer made layer-backed; this runs on the main thread that owns it.
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
                log(&format!("projector: cannot attach the layer: {e}"));
                return;
            }
            st.attached = true;
        }
        // Core Animation places in points; DirectComposition in physical pixels.
        let size = window.window().size();
        #[cfg(target_os = "macos")]
        let (w, h) = {
            let l = size.to_logical(window.window().scale_factor());
            (f64::from(l.width), f64::from(l.height))
        };
        #[cfg(windows)]
        let (w, h) = (f64::from(size.width), f64::from(size.height));
        st.layer.set_rect(LayerRect {
            x: 0.0,
            y: 0.0,
            width: w,
            height: h,
        });
        st.layer.set_hidden(false);
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn place_layer(_: ProjectorCanvas, _: &ProjectorWindow) {}

#[cfg(any(target_os = "macos", windows))]
fn show(canvas: ProjectorCanvas, frame: PreviewFrame) {
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        if let Some(st) = slots
            .iter_mut()
            .find(|s| s.canvas == canvas)
            .and_then(|s| s.layer.as_mut())
        {
            let now = st.clock.now();
            st.layer.show(frame, now);
        }
    });
    retire(canvas);
}

/// Releases replaced frames the compositor no longer reads; while frames wait, a
/// one-shot timer comes back at the oldest one's due time (as the Program picture).
#[cfg(any(target_os = "macos", windows))]
fn retire(canvas: ProjectorCanvas) {
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        let Some(st) = slots
            .iter_mut()
            .find(|s| s.canvas == canvas)
            .and_then(|s| s.layer.as_mut())
        else {
            return;
        };
        let now = st.clock.now();
        st.layer.retire(now);
        match st.layer.next_retire() {
            None => st.timer.stop(),
            Some(due) => {
                if !st.timer.running() {
                    let after = due.saturating_since(now).max(Duration::from_millis(1));
                    entry::start(&st.timer, slint::TimerMode::SingleShot, after, move || {
                        retire(canvas);
                    });
                }
            }
        }
    });
}
