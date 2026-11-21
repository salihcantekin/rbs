//! Interact windows (ADR-0132 decision 6): one browser page in a window of its own,
//! to log in, click or scroll it while it stays on air.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Interact window".
//!
//! - The picture is the studio's picture of that source at the page's size (the host
//!   adds it while the window is open), shown as a native layer over the whole window
//!   in the page's shape, like a projector. While the page cannot run, the layer hides
//!   and the window says why.
//! - Pointer, wheel and keys go to the page only while the window has focus, in page
//!   pixels. Typed text is never logged.
//! - Two at most (`studio_host::MAX_INTERACT`): the host refuses a third. With no
//!   window open nothing here runs (P13): no window, layer, thread or timer.
//!
//! UI thread only. The frame thread of each picture hands frames to the UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_browser_ipc::input::{self as wire, Button, InputEvent, Key, NamedKey};
#[cfg(any(target_os = "macos", windows))]
use rbs_core::time::Clock;
use rbs_core::time::MonotonicClock;
use rbs_gpu::share::PreviewEndpoint;
#[cfg(any(target_os = "macos", windows))]
use rbs_gpu::share::{PreviewFrame, PreviewSubscriber};
use rbs_scene::model::SourceId;
use slint::ComponentHandle;
use slint::winit_030::winit;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey as WNamed};

use crate::entry;
use crate::ui::InteractWindow;

/// How long connecting to the page's picture may take before it is given up.
#[cfg(any(target_os = "macos", windows))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Presses of one button closer than this in time and in [`CLICK_SLOP`] physical
/// pixels count as a double or triple click (the common desktop default).
const CLICK_GAP_MS: u128 = 500;
const CLICK_SLOP: f64 = 4.0;

/// Page pixels one wheel line scrolls (as Chromium scrolls a line).
const LINE_PX: f64 = 40.0;

/// A page larger than this share of its display opens smaller, in its shape.
const MOST_OF_DISPLAY: f64 = 0.8;

/// The Page tab's `state` that runs (`browser_props::state::OK`).
const STATE_OK: i32 = 0;

/// What an Interact window tells the shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The user closed it.
    Close(SourceId),
    /// Input for its page.
    Input(SourceId, InputEvent),
    /// Open Add-ons, to install the browser engine.
    OpenEngine,
}

/// What the window shows of its source, from the Page tab's data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The window title.
    pub title: String,
    /// The page size in pixels.
    pub page: (u32, u32),
    /// The Page tab's `state`.
    pub state: i32,
    /// Why it failed, for state 3.
    pub status: String,
}

// Made only where pictures exist (macOS, Windows).
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
struct Layer {
    #[cfg(any(target_os = "macos", windows))]
    layer: rbs_gpu::share::PreviewLayer,
    attached: bool,
    /// A frame was shown (logged once).
    shown: bool,
    clock: MonotonicClock,
    timer: slint::Timer,
}

/// The pointer and keyboard as the window last saw them.
#[derive(Default)]
struct Pointer {
    focused: bool,
    /// Physical pixels in the window.
    at: (f64, f64),
    modifiers: ModifiersState,
    held: u8,
    last: Option<(MouseButton, Instant, (f64, f64), u8)>,
    /// Input reached the page (logged once, never what it was).
    sent: bool,
}

struct Slot {
    source: SourceId,
    window: InteractWindow,
    view: View,
    layer: Option<Layer>,
    /// Bumped to end the frame thread of the connection before.
    generation: Arc<AtomicU64>,
    endpoint: Option<PreviewEndpoint>,
}

type Sink = Rc<dyn Fn(Event)>;

thread_local! {
    static SLOTS: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
    static EVENTS: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Where Interact events go (the shell's `Ctx::update`).
pub fn install(events: impl Fn(Event) + 'static) {
    EVENTS.with(|e| *e.borrow_mut() = Some(Rc::new(events)));
}

fn emit(event: Event) {
    let sink = EVENTS.with(|e| e.borrow().clone());
    if let Some(sink) = sink {
        sink(event);
    }
}

/// The sources whose Interact window is open.
pub fn open_sources() -> Vec<SourceId> {
    SLOTS.with(|slots| slots.borrow().iter().map(|s| s.source).collect())
}

/// True while `source` has an Interact window.
pub fn is_open(source: SourceId) -> bool {
    SLOTS.with(|slots| slots.borrow().iter().any(|s| s.source == source))
}

/// Opens the Interact window of `source` (`make` sets up its globals), or brings it to
/// the front when it is open.
pub fn open(source: SourceId, view: View, make: &dyn Fn(&InteractWindow)) {
    if is_open(source) {
        raise(source);
        return;
    }
    let window = match InteractWindow::new() {
        Ok(w) => w,
        Err(e) => {
            rbs_core::warn!(rbs_core::log::Area::Browser, "interact: no window: {e}");
            return;
        }
    };
    make(&window);
    apply_view(&window, &view);
    // The page at its own size, so it reads as it does in a browser; the OS keeps
    // the window on the display.
    let (w, h) = view.page;
    window
        .window()
        .set_size(slint::WindowSize::Logical(slint::LogicalSize::new(
            w.max(1) as f32,
            h.max(1) as f32,
        )));
    window.on_open_engine(|| entry::guard(|| emit(Event::OpenEngine)));
    install_events(&window, source);
    let place = window.as_weak();
    let set = entry::set_rendering_notifier(window.window(), move |state, _| {
        if matches!(state, slint::RenderingState::BeforeRendering)
            && let Some(w) = place.upgrade()
        {
            place_layer(source, &w);
        }
    });
    if let Err(e) = set {
        rbs_core::warn!(
            rbs_core::log::Area::Browser,
            "interact: no rendering notifier: {e:?}"
        );
    }
    entry::on_close_requested(window.window(), move || {
        emit(Event::Close(source));
        slint::CloseRequestResponse::HideWindow
    });
    if let Err(e) = window.show() {
        rbs_core::warn!(rbs_core::log::Area::Browser, "interact: cannot show: {e}");
        return;
    }
    rbs_core::debug!(
        rbs_core::log::Area::Browser,
        "interact: a window opened, page {w} x {h}"
    );
    // A page larger than most of its display opens smaller, in its shape.
    let weak = window.as_weak();
    entry::single_shot(Duration::ZERO, move || {
        if let Some(w) = weak.upgrade() {
            fit_to_display(&w, view_page(source));
        }
    });
    SLOTS.with(|slots| {
        slots.borrow_mut().push(Slot {
            source,
            window,
            view,
            layer: None,
            generation: Arc::new(AtomicU64::new(0)),
            endpoint: None,
        });
    });
}

fn view_page(source: SourceId) -> (u32, u32) {
    SLOTS.with(|slots| {
        slots
            .borrow()
            .iter()
            .find(|s| s.source == source)
            .map_or((0, 0), |s| s.view.page)
    })
}

/// Shrinks a window whose page is larger than [`MOST_OF_DISPLAY`] of its display.
fn fit_to_display(window: &InteractWindow, page: (u32, u32)) {
    window.window().with_winit_window(|w| {
        let Some(m) = w.current_monitor() else {
            return;
        };
        let (dw, dh) = (f64::from(m.size().width), f64::from(m.size().height));
        let scale = w.scale_factor();
        let (pw, ph) = (f64::from(page.0) * scale, f64::from(page.1) * scale);
        if pw <= 0.0 || ph <= 0.0 {
            return;
        }
        let k = (dw * MOST_OF_DISPLAY / pw).min(dh * MOST_OF_DISPLAY / ph);
        if k < 1.0 {
            let _ = w.request_inner_size(winit::dpi::PhysicalSize::new(
                (pw * k).round() as u32,
                (ph * k).round() as u32,
            ));
        }
    });
}

fn apply_view(window: &InteractWindow, view: &View) {
    window.set_window_title(view.title.as_str().into());
    window.set_state(view.state);
    window.set_status(view.status.as_str().into());
}

/// What the window of `source` shows now (its source's data changed).
pub fn set_view(source: SourceId, view: View) {
    let changed = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let s = slots.iter_mut().find(|s| s.source == source)?;
        if s.view == view {
            return None;
        }
        apply_view(&s.window, &view);
        s.view = view;
        Some(s.window.as_weak())
    });
    if let Some(w) = changed.and_then(|w| w.upgrade()) {
        place_layer(source, &w);
        w.window().request_redraw();
    }
}

/// Brings the window of `source` to the front.
pub fn raise(source: SourceId) {
    SLOTS.with(|slots| {
        if let Some(s) = slots.borrow().iter().find(|s| s.source == source) {
            s.window.window().with_winit_window(|w| w.focus_window());
        }
    });
}

fn close_slot(s: &mut Slot) {
    s.generation.fetch_add(1, Ordering::Relaxed);
    s.layer = None;
    let _ = s.window.hide();
}

/// Closes the window of `source`.
pub fn close(source: SourceId) {
    let slot = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let at = slots.iter().position(|s| s.source == source)?;
        Some(slots.remove(at))
    });
    // Hidden outside the borrow: hiding may run the window's own handlers.
    if let Some(mut s) = slot {
        close_slot(&mut s);
        rbs_core::debug!(rbs_core::log::Area::Browser, "interact: a window closed");
    }
}

/// Closes every Interact window and drops its layer (the frames go back before the
/// studio stops).
pub fn close_all() {
    let all = SLOTS.with(|slots| std::mem::take(&mut *slots.borrow_mut()));
    for mut s in all {
        close_slot(&mut s);
    }
}

/// Drops every window's layer before the studio restarts; the host sends the new
/// pictures after it.
pub fn drop_pictures() {
    SLOTS.with(|slots| {
        for s in slots.borrow_mut().iter_mut() {
            s.generation.fetch_add(1, Ordering::Relaxed);
            s.layer = None;
            s.endpoint = None;
        }
    });
}

/// Runs `f` on every open Interact window (the look again after a theme change).
pub fn each_window(f: impl Fn(&InteractWindow)) {
    SLOTS.with(|slots| {
        for s in slots.borrow().iter() {
            f(&s.window);
        }
    });
}

/// The host's news of a window's picture: connect to a new endpoint, or drop the
/// layer while there is none.
pub fn picture(source: SourceId, endpoint: Option<PreviewEndpoint>) {
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let Some(s) = slots.iter_mut().find(|s| s.source == source) else {
            return;
        };
        if s.endpoint == endpoint {
            return;
        }
        let has = if endpoint.is_some() { "has" } else { "has no" };
        rbs_core::debug!(rbs_core::log::Area::Browser, "interact: {has} picture");
        s.generation.fetch_add(1, Ordering::Relaxed);
        s.layer = None;
        s.endpoint.clone_from(&endpoint);
        if let Some(endpoint) = endpoint {
            connect(source, endpoint, Arc::clone(&s.generation));
        }
    });
}

// ---- Input ----------------------------------------------------------------------

/// Where the page sits in a window of `window` physical pixels: centred in its own
/// shape (the layer's resize-aspect). Returns the scale and the top-left offset.
fn fit(window: (u32, u32), page: (u32, u32)) -> Option<(f64, (f64, f64))> {
    if window.0 == 0 || window.1 == 0 || page.0 == 0 || page.1 == 0 {
        return None;
    }
    let (ww, wh) = (f64::from(window.0), f64::from(window.1));
    let (pw, ph) = (f64::from(page.0), f64::from(page.1));
    let scale = (ww / pw).min(wh / ph);
    Some((scale, ((ww - pw * scale) / 2.0, (wh - ph * scale) / 2.0)))
}

/// A point in the window (physical pixels) in page pixels, clamped to the page.
fn to_page(at: (f64, f64), window: (u32, u32), page: (u32, u32)) -> Option<(i32, i32)> {
    let (scale, (ox, oy)) = fit(window, page)?;
    let x = ((at.0 - ox) / scale).clamp(0.0, f64::from(page.0 - 1));
    let y = ((at.1 - oy) / scale).clamp(0.0, f64::from(page.1 - 1));
    Some((x as i32, y as i32))
}

/// The wire's modifier bits for the keyboard's modifiers and the held buttons.
fn modifier_bits(m: ModifiersState, held: u8) -> u8 {
    let mut bits = held;
    for (on, bit) in [
        (m.shift_key(), wire::SHIFT),
        (m.control_key(), wire::CTRL),
        (m.alt_key(), wire::ALT),
        (m.super_key(), wire::META),
    ] {
        if on {
            bits |= bit;
        }
    }
    bits
}

fn held_bit(button: Button) -> u8 {
    match button {
        Button::Left => wire::LEFT_HELD,
        Button::Middle => wire::MIDDLE_HELD,
        Button::Right => wire::RIGHT_HELD,
    }
}

/// The wire's key for a key the page takes as a key (not only as text).
fn wire_key(key: &WKey) -> Option<Key> {
    Some(match key {
        WKey::Named(n) => Key::Named(match n {
            WNamed::Enter => NamedKey::Enter,
            WNamed::Tab => NamedKey::Tab,
            WNamed::Backspace => NamedKey::Backspace,
            WNamed::Escape => NamedKey::Escape,
            WNamed::Delete => NamedKey::Delete,
            WNamed::Insert => NamedKey::Insert,
            WNamed::Home => NamedKey::Home,
            WNamed::End => NamedKey::End,
            WNamed::PageUp => NamedKey::PageUp,
            WNamed::PageDown => NamedKey::PageDown,
            WNamed::ArrowLeft => NamedKey::ArrowLeft,
            WNamed::ArrowRight => NamedKey::ArrowRight,
            WNamed::ArrowUp => NamedKey::ArrowUp,
            WNamed::ArrowDown => NamedKey::ArrowDown,
            WNamed::Space => return Some(Key::Char(' ')),
            WNamed::F1 => NamedKey::F(1),
            WNamed::F2 => NamedKey::F(2),
            WNamed::F3 => NamedKey::F(3),
            WNamed::F4 => NamedKey::F(4),
            WNamed::F5 => NamedKey::F(5),
            WNamed::F6 => NamedKey::F(6),
            WNamed::F7 => NamedKey::F(7),
            WNamed::F8 => NamedKey::F(8),
            WNamed::F9 => NamedKey::F(9),
            WNamed::F10 => NamedKey::F(10),
            WNamed::F11 => NamedKey::F(11),
            WNamed::F12 => NamedKey::F(12),
            _ => return None,
        }),
        WKey::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
        _ => return None,
    })
}

/// Text a key press types into the page: none while a shortcut modifier is held, and
/// none for the keys the runner types itself (Enter, Tab, Backspace, Escape).
fn typed_text(key: &WKey, text: Option<&str>, m: ModifiersState) -> Option<String> {
    if m.control_key() || m.super_key() {
        return None;
    }
    if let WKey::Named(WNamed::Enter | WNamed::Tab | WNamed::Backspace | WNamed::Escape) = key {
        return None;
    }
    let text: String = text?.chars().filter(|c| !c.is_control()).collect();
    (!text.is_empty()).then_some(text)
}

fn install_events(window: &InteractWindow, source: SourceId) {
    let pointer = Rc::new(RefCell::new(Pointer::default()));
    window.window().on_winit_window_event(move |sw, event| {
        let _ = rbs_crash::contain(|| {
            sw.with_winit_window(|w| on_event(w, event, source, &pointer));
        });
        EventResult::Propagate
    });
}

fn page_of(source: SourceId) -> Option<(u32, u32)> {
    SLOTS.with(|slots| {
        let slots = slots.try_borrow().ok()?;
        let s = slots.iter().find(|s| s.source == source)?;
        (s.view.state == STATE_OK).then_some(s.view.page)
    })
}

fn on_event(
    w: &winit::window::Window,
    event: &WindowEvent,
    source: SourceId,
    pointer: &RefCell<Pointer>,
) {
    let mut p = pointer.borrow_mut();
    match event {
        WindowEvent::Focused(on) => {
            let not = if *on { "" } else { "not " };
            rbs_core::debug!(rbs_core::log::Area::Browser, "interact: {not}focused");
            p.focused = *on;
            if !*on {
                p.held = 0;
            }
            return;
        }
        WindowEvent::ModifiersChanged(m) => {
            p.modifiers = m.state();
            return;
        }
        WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
            drop(p);
            place_soon(source);
            return;
        }
        WindowEvent::CursorMoved { position, .. } => p.at = (position.x, position.y),
        _ => {}
    }
    // Only while the window has focus, and only to a page that runs.
    if !p.focused {
        return;
    }
    let Some(page) = page_of(source) else {
        return;
    };
    let size = w.inner_size();
    let window = (size.width, size.height);
    let Some((x, y)) = to_page(p.at, window, page) else {
        return;
    };
    let modifiers = modifier_bits(p.modifiers, p.held);
    if !p.sent {
        p.sent = true;
        rbs_core::debug!(
            rbs_core::log::Area::Browser,
            "interact: input goes to the page"
        );
    }
    let send = |e: InputEvent| emit(Event::Input(source, e));
    match event {
        WindowEvent::CursorMoved { .. } => send(InputEvent::Move { x, y, modifiers }),
        WindowEvent::CursorLeft { .. } => send(InputEvent::Leave { modifiers }),
        WindowEvent::MouseInput { state, button, .. } => {
            let wire_button = match button {
                MouseButton::Left => Button::Left,
                MouseButton::Middle => Button::Middle,
                MouseButton::Right => Button::Right,
                _ => return,
            };
            let down = *state == ElementState::Pressed;
            let clicks = if down {
                let now = Instant::now();
                let at = p.at;
                let clicks = match p.last {
                    Some((b, t, (lx, ly), n))
                        if b == *button
                            && now.duration_since(t).as_millis() < CLICK_GAP_MS
                            && (lx - at.0).abs() <= CLICK_SLOP
                            && (ly - at.1).abs() <= CLICK_SLOP =>
                    {
                        (n % 3) + 1
                    }
                    _ => 1,
                };
                p.last = Some((*button, now, at, clicks));
                p.held |= held_bit(wire_button);
                clicks
            } else {
                p.held &= !held_bit(wire_button);
                p.last.map_or(1, |(.., n)| n)
            };
            send(InputEvent::Button {
                x,
                y,
                button: wire_button,
                down,
                clicks,
                modifiers,
            });
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy) = match delta {
                MouseScrollDelta::LineDelta(dx, dy) => {
                    (f64::from(*dx) * LINE_PX, f64::from(*dy) * LINE_PX)
                }
                MouseScrollDelta::PixelDelta(d) => {
                    let scale = fit(window, page).map_or(1.0, |(s, _)| s);
                    (d.x / scale, d.y / scale)
                }
            };
            send(InputEvent::Wheel {
                x,
                y,
                dx: dx.round() as i32,
                dy: dy.round() as i32,
                modifiers,
            });
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let down = event.state == ElementState::Pressed;
            if let Some(key) = wire_key(&event.logical_key) {
                send(InputEvent::Key {
                    key,
                    down,
                    modifiers,
                });
            }
            if down
                && let Some(text) =
                    typed_text(&event.logical_key, event.text.as_deref(), p.modifiers)
            {
                send(InputEvent::Text(text));
            }
        }
        _ => {}
    }
}

// ---- Picture --------------------------------------------------------------------

/// Places the layer after this event, outside any borrow.
fn place_soon(source: SourceId) {
    #[cfg(any(target_os = "macos", windows))]
    {
        let window = SLOTS.with(|slots| {
            let slots = slots.try_borrow().ok()?;
            Some(slots.iter().find(|s| s.source == source)?.window.as_weak())
        });
        if let Some(weak) = window {
            entry::single_shot(Duration::ZERO, move || {
                if let Some(w) = weak.upgrade() {
                    place_layer(source, &w);
                }
            });
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = source;
}

#[cfg(not(any(target_os = "macos", windows)))]
fn connect(_: SourceId, _: PreviewEndpoint, _: Arc<AtomicU64>) {}

/// Connects a window to its picture off the UI thread, then makes its layer on the UI
/// thread; frames follow on a thread of their own until the generation changes.
#[cfg(any(target_os = "macos", windows))]
fn connect(source: SourceId, endpoint: PreviewEndpoint, generation: Arc<AtomicU64>) {
    let at = generation.load(Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("rbs-interact-connect".into())
        .spawn(
            move || match PreviewSubscriber::connect(&endpoint, CONNECT_TIMEOUT) {
                Ok(sub) => {
                    let _ = entry::invoke(move || setup(source, sub, generation, at));
                }
                Err(e) => rbs_core::warn!(
                    rbs_core::log::Area::Browser,
                    "interact: cannot connect: {e}"
                ),
            },
        );
    if let Err(e) = spawned {
        rbs_core::warn!(rbs_core::log::Area::Browser, "interact: no thread: {e}");
    }
}

#[cfg(any(target_os = "macos", windows))]
fn setup(source: SourceId, mut sub: PreviewSubscriber, generation: Arc<AtomicU64>, at: u64) {
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
            rbs_core::warn!(
                rbs_core::log::Area::Browser,
                "interact: no layer in this session: {e}"
            );
            return;
        }
    };
    let window = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let s = slots.iter_mut().find(|s| s.source == source)?;
        s.layer = Some(Layer {
            layer,
            attached: false,
            shown: false,
            clock: MonotonicClock::new(),
            timer: slint::Timer::default(),
        });
        Some(s.window.as_weak())
    });
    if let Some(w) = window.and_then(|w| w.upgrade()) {
        rbs_core::debug!(rbs_core::log::Area::Browser, "interact: picture connected");
        place_layer(source, &w);
        w.window().request_redraw();
    }
    let spawned = std::thread::Builder::new()
        .name("rbs-interact".into())
        .spawn(move || {
            // Latest-only: a slow UI shows the newest frame, never a queue (WP-J).
            let slot = Arc::new(rbs_app::latest::Latest::new());
            while current() {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        let current = current.clone();
                        let shown = entry::invoke_latest(&slot, frame, move |frame| {
                            if current() {
                                show(source, frame);
                            }
                        });
                        if shown.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if current() {
                            rbs_core::warn!(
                                rbs_core::log::Area::Browser,
                                "interact: the picture ended: {e}"
                            );
                        }
                        break;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        rbs_core::warn!(rbs_core::log::Area::Browser, "interact: no thread: {e}");
    }
}

/// Attaches the layer once the window exists and covers the whole window with it (the
/// layer keeps the page's shape); hidden while the page does not run, so the window's
/// reason shows.
#[cfg(any(target_os = "macos", windows))]
fn place_layer(source: SourceId, window: &InteractWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rbs_gpu::share::LayerRect;
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        let Some(s) = slots.iter_mut().find(|s| s.source == source) else {
            return;
        };
        let runs = s.view.state == STATE_OK;
        let Some(st) = s.layer.as_mut() else {
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
                rbs_core::warn!(
                    rbs_core::log::Area::Browser,
                    "interact: cannot attach the layer: {e}"
                );
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
        st.layer.set_hidden(!runs);
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn place_layer(_: SourceId, _: &InteractWindow) {}

#[cfg(any(target_os = "macos", windows))]
fn show(source: SourceId, frame: PreviewFrame) {
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        if let Some(st) = slots
            .iter_mut()
            .find(|s| s.source == source)
            .and_then(|s| s.layer.as_mut())
        {
            if !st.shown {
                st.shown = true;
                rbs_core::debug!(rbs_core::log::Area::Browser, "interact: first frame");
            }
            let now = st.clock.now();
            st.layer.show(frame, now);
        }
    });
    retire(source);
}

/// Releases replaced frames the compositor no longer reads, as the projector does.
#[cfg(any(target_os = "macos", windows))]
fn retire(source: SourceId) {
    SLOTS.with(|slots| {
        let Ok(mut slots) = slots.try_borrow_mut() else {
            return;
        };
        let Some(st) = slots
            .iter_mut()
            .find(|s| s.source == source)
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
                        retire(source);
                    });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_maps_into_the_letterboxed_page() {
        // A 1000 x 1000 window shows a 1920 x 1080 page 1000 x 562.5, centred.
        let page = (1920, 1080);
        assert_eq!(
            to_page((500.0, 500.0), (1000, 1000), page),
            Some((960, 540))
        );
        assert_eq!(to_page((0.0, 218.75), (1000, 1000), page), Some((0, 0)));
        // Outside the page clamps to its edge.
        assert_eq!(to_page((-5.0, 0.0), (1000, 1000), page), Some((0, 0)));
        assert_eq!(
            to_page((1000.0, 1000.0), (1000, 1000), page),
            Some((1919, 1079))
        );
        assert_eq!(to_page((1.0, 1.0), (0, 10), page), None);
    }

    #[test]
    fn modifiers_and_held_buttons_become_wire_bits() {
        let m = ModifiersState::SHIFT | ModifiersState::SUPER;
        assert_eq!(
            modifier_bits(m, wire::LEFT_HELD),
            wire::SHIFT | wire::META | wire::LEFT_HELD
        );
        assert_eq!(modifier_bits(ModifiersState::empty(), 0), 0);
    }

    #[test]
    fn keys_map_to_the_wire_and_text_skips_shortcuts() {
        assert_eq!(
            wire_key(&WKey::Named(WNamed::Enter)),
            Some(Key::Named(NamedKey::Enter))
        );
        assert_eq!(
            wire_key(&WKey::Named(WNamed::F12)),
            Some(Key::Named(NamedKey::F(12)))
        );
        assert_eq!(wire_key(&WKey::Named(WNamed::Space)), Some(Key::Char(' ')));
        assert_eq!(wire_key(&WKey::Character("a".into())), Some(Key::Char('a')));
        assert_eq!(wire_key(&WKey::Named(WNamed::Shift)), None);
        let a = WKey::Character("a".into());
        assert_eq!(
            typed_text(&a, Some("a"), ModifiersState::empty()),
            Some("a".into())
        );
        assert_eq!(typed_text(&a, Some("a"), ModifiersState::CONTROL), None);
        let enter = WKey::Named(WNamed::Enter);
        assert_eq!(
            typed_text(&enter, Some("\r"), ModifiersState::empty()),
            None
        );
    }
}
