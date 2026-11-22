//! The session's mapping thread (`rbs-wgc-watch`): the target's desktop rectangle and,
//! for windows, the covered test, while the picture mapping switch is on (ADR-0165
//! decision 5, capture-port.md "Picture mapping" rules 4 and 5, `crate::geometry`).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::thread::JoinHandle;

use rbs_capture::Cover;
use windows::Win32::Foundation::{POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, GA_ROOT, GetAncestor,
    GetCursorPos, GetWindowThreadProcessId, IsIconic, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, OBJID_WINDOW, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, QS_ALLINPUT, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_NULL,
    WindowFromPoint,
};

use super::*;
use crate::geometry::{self, Rect, Under};

/// How often the thread looks at the pointer and the target without an event
/// (capture-port.md rule 4: a 50 ms poll that skips while the pointer stands still).
const POLL_MS: u32 = 50;

/// The captured target, as plain values so the watcher is `Send`.
#[derive(Clone, Copy)]
pub(super) enum Target {
    Display(isize),
    Window(isize),
}

impl Target {
    pub(super) fn of(resolved: Resolved) -> Self {
        match resolved {
            Resolved::Display(monitor) => Target::Display(monitor.0 as isize),
            Resolved::Window(hwnd) => Target::Window(hwnd.0 as isize),
        }
    }
}

/// Where the target sits on the desktop, as of the thread's last look.
#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    pub(super) target: Rect,
    pub(super) cover: Cover,
}

/// State of the session's mapping thread.
///
/// The thread runs only while the session is Active and the switch is on (P13). It
/// ends itself within one poll when either goes, and `stop` joins it.
pub(super) struct Watch {
    target: Target,
    pub(super) active: AtomicBool,
    /// The mapping switch is on.
    pub(super) mapping: AtomicBool,
    /// A thread runs the loop. Claimed with a compare-exchange by whoever starts one.
    running: AtomicBool,
    /// The running thread's id, 0 when none, to wake it.
    thread_id: AtomicU32,
    /// Control path only (start, stop, the switch callback).
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Read by the frame handler with `try_lock`.
    pub(super) geometry: Mutex<Option<Geometry>>,
}

impl Watch {
    pub(super) fn new(target: Target) -> Self {
        Self {
            target,
            active: AtomicBool::new(false),
            mapping: AtomicBool::new(false),
            running: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
            threads: Mutex::new(Vec::new()),
            geometry: Mutex::new(None),
        }
    }

    fn needed(&self) -> bool {
        self.active.load(Ordering::SeqCst) && self.mapping.load(Ordering::SeqCst)
    }

    /// Starts the thread if it has a job and none runs (control path), and joins
    /// threads that already ended (joining them does not wait).
    pub(super) fn ensure(self: &Arc<Self>) {
        let done: Vec<_> = {
            let mut threads = lock(&self.threads);
            let (done, running): (Vec<_>, Vec<_>) =
                threads.drain(..).partition(JoinHandle::is_finished);
            *threads = running;
            done
        };
        for thread in done {
            let _ = thread.join();
        }
        self.spawn_if_needed();
    }

    /// [`ensure`](Self::ensure) for the switch's callback, which runs under the
    /// switch's lock: it only spawns or wakes, never joins or waits (rule 2).
    pub(super) fn signal(self: &Arc<Self>) {
        self.wake();
        self.spawn_if_needed();
    }

    /// Wakes a running thread so it sees a change at once. Never waits.
    fn wake(&self) {
        let id = self.thread_id.load(Ordering::SeqCst);
        if id != 0 {
            // SAFETY: posting a no-op message to a thread; a thread that has ended or
            // has no queue makes the call fail, which is harmless.
            let _ = unsafe { PostThreadMessageW(id, WM_NULL, WPARAM(0), LPARAM(0)) };
        }
    }

    fn spawn_if_needed(self: &Arc<Self>) {
        if !self.needed()
            || self
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let watch = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("rbs-wgc-watch".into())
            .spawn(move || watch.run());
        match spawned {
            Ok(thread) => lock(&self.threads).push(thread),
            Err(_) => self.running.store(false, Ordering::SeqCst),
        }
    }

    /// Ends every mapping thread and waits for them (control path).
    pub(super) fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.wake();
        let threads = std::mem::take(&mut *lock(&self.threads));
        for thread in threads {
            let _ = thread.join();
        }
        *lock(&self.geometry) = None;
    }

    fn run(self: Arc<Self>) {
        // A panic in a look (an OS call) must not leave `running` claimed.
        struct Unclaim<'a>(&'a AtomicBool, &'a AtomicU32, u32);
        impl Drop for Unclaim<'_> {
            fn drop(&mut self) {
                // Only this thread's own id: a newer thread may already have stored its.
                let _ = self
                    .1
                    .compare_exchange(self.2, 0, Ordering::SeqCst, Ordering::SeqCst);
                if std::thread::panicking() {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
        }
        // SAFETY: no arguments.
        let own_id = unsafe { GetCurrentThreadId() };
        let _unclaim = Unclaim(&self.running, &self.thread_id, own_id);
        // Rule 5: the same coordinates as the input adapter's thread.
        // SAFETY: the constant is a documented sentinel; the call changes this thread.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let hooks = Hooks::install(self.target);
        // The id is published only once this thread has a message queue, so a wake
        // never goes to a thread that cannot receive it. A change before that is seen
        // by the `needed` check at the top of the loop.
        let mut message = MSG::default();
        // SAFETY: `message` is a valid out pointer; PM_NOREMOVE only creates the queue.
        let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
        self.thread_id.store(own_id, Ordering::SeqCst);
        let mut last_point = None;
        loop {
            if !self.needed() {
                *lock(&self.geometry) = None;
                self.running.store(false, Ordering::SeqCst);
                // A job given between the check and the store is taken by this thread
                // if no other one claimed it.
                if !self.needed()
                    || self
                        .running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    break;
                }
            }
            let changed = CHANGED.with(|c| c.replace(false));
            let geometry = self.look(&mut last_point, changed);
            if let Some(geometry) = geometry {
                *lock(&self.geometry) = Some(geometry);
            }
            wait_for_messages();
        }
        drop(hooks);
    }

    /// One look at the target. `changed`: a window event since the last look.
    /// `None` keeps the last geometry (nothing moved, or a read failed).
    fn look(&self, last_point: &mut Option<(i32, i32)>, changed: bool) -> Option<Geometry> {
        let first = lock(&self.geometry).is_none();
        match self.target {
            Target::Display(monitor) => {
                // A monitor's rectangle changes only with the display settings; one
                // `GetMonitorInfoW` per poll is cheap.
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                // SAFETY: `info` has `cbSize` set; a stale handle makes the call fail.
                let ok = unsafe { GetMonitorInfoW(HMONITOR(monitor as *mut _), &mut info) };
                ok.as_bool().then(|| Geometry {
                    target: rect(info.rcMonitor),
                    cover: Cover::Unknown,
                })
            }
            Target::Window(value) => {
                let hwnd = HWND(value as *mut _);
                let point = cursor();
                let moved = point != *last_point;
                *last_point = point;
                if !first && !moved && !changed {
                    return None;
                }
                let frame = frame_bounds(hwnd)?;
                // SAFETY: plain queries on window handles; a stale handle gives null or
                // false.
                let (minimised, under) = unsafe {
                    let minimised = IsIconic(hwnd).as_bool();
                    let under = match point {
                        Some((x, y)) => {
                            let at = WindowFromPoint(POINT { x, y });
                            if GetAncestor(at, GA_ROOT) == hwnd {
                                Under::Target
                            } else {
                                Under::Other
                            }
                        }
                        None => Under::Other,
                    };
                    (minimised, under)
                };
                Some(Geometry {
                    target: frame,
                    cover: geometry::window_cover(point, frame, minimised, under),
                })
            }
        }
    }
}

fn rect(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// The pointer in physical pixels of the virtual screen (this thread is per-monitor
/// v2), `None` when it cannot be read (secure desktop).
fn cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: valid out pointer.
    unsafe { GetCursorPos(&mut point) }
        .ok()
        .map(|()| (point.x, point.y))
}

/// The window's DWM extended frame bounds, the rectangle WGC captures.
fn frame_bounds(hwnd: HWND) -> Option<Rect> {
    let mut bounds = RECT::default();
    // SAFETY: `bounds` is a RECT, the type and size this attribute writes.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut bounds as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(rect(bounds))
}

/// Waits up to one poll for a message (a window event or a wake), then dispatches
/// what came, which runs the hook callbacks.
fn wait_for_messages() {
    // SAFETY: no handles, a timeout, and the documented flags.
    let _ = unsafe { MsgWaitForMultipleObjectsEx(None, POLL_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    let mut message = MSG::default();
    // SAFETY: `message` is a valid out pointer; messages of this thread only.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: a message this thread just received.
        unsafe { DispatchMessageW(&message) };
    }
}

thread_local! {
    /// Set by the window event hooks of this thread; read and cleared by each look.
    static CHANGED: Cell<bool> = const { Cell::new(false) };
    /// The window whose events count, as a plain value.
    static WATCHED: Cell<isize> = const { Cell::new(0) };
}

/// Cannot panic: two thread-local cell accesses and comparisons (a panic must not
/// unwind into the system's frames).
unsafe extern "system" fn on_window_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    let ours = WATCHED.with(Cell::get) == hwnd.0 as isize;
    if event == EVENT_SYSTEM_FOREGROUND || (ours && object == OBJID_WINDOW.0) {
        CHANGED.with(|c| c.set(true));
    }
}

/// The out-of-context window event hooks of a window capture (rule 4): the target's
/// location changes and any foreground change. Removed on drop, on the same thread.
struct Hooks(Vec<HWINEVENTHOOK>);

impl Hooks {
    fn install(target: Target) -> Hooks {
        let Target::Window(value) = target else {
            return Hooks(Vec::new());
        };
        let hwnd = HWND(value as *mut _);
        WATCHED.with(|w| w.set(value));
        let mut process = 0u32;
        // SAFETY: a plain query with a valid out pointer.
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
        // The location hook does not skip this process: the target may be one of
        // RBS's own windows. The foreground hook does, since RBS's own focus changes
        // need no look of their own (the poll sees them).
        // SAFETY: out-of-context hooks with a callback of the documented signature;
        // they are delivered to this thread's message queue, and removed on drop on
        // this thread.
        let hooks = unsafe {
            [
                SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(on_window_event),
                    process,
                    thread,
                    WINEVENT_OUTOFCONTEXT,
                ),
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(on_window_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                ),
            ]
        };
        Hooks(hooks.into_iter().filter(|h| !h.is_invalid()).collect())
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for hook in self.0.drain(..) {
            // SAFETY: a hook this thread installed and has not removed.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
}
