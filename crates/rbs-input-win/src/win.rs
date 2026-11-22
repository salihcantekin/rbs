//! The Raw Input thread and its handler (crate docs, "How events travel").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::log::Area;
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::EdgeClock;
use rbs_input::{
    DesktopPoint, EventMask, InputBackend, InputError, InputEvent, InputSink, PointerSample,
    RunningInput, TimedEvent, caps,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    HWND_MESSAGE, MSG, PostMessageW, RegisterClassExW, UnregisterClassW, WINDOW_EX_STYLE, WM_APP,
    WM_INPUT, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::model;

/// How long `start` waits for the input thread to report.
const READY_WAIT: Duration = Duration::from_millis(500);
/// The private message that ends the thread's loop.
const WM_STOP: u32 = WM_APP + 1;
/// HID usage page and usage of a mouse (generic desktop, mouse).
const USAGE_PAGE_GENERIC: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
/// The window class of the message-only window.
const CLASS: PCWSTR = w!("RbsRawInputSink");

/// `InputError::Backend` codes of this adapter.
pub mod codes {
    /// The thread could not be spawned.
    pub const SPAWN: i64 = 1;
    /// The input thread did not report within 500 ms.
    pub const READY_TIMEOUT: i64 = 2;
    /// The input thread ended without reporting.
    pub const THREAD_ENDED: i64 = 3;
    /// The message-only window could not be created.
    pub const NO_WINDOW: i64 = 4;
    /// `RegisterRawInputDevices` refused the mouse.
    pub const NOT_REGISTERED: i64 = 5;
}

/// Mouse input from Raw Input.
pub struct WinInput {
    clock: Arc<dyn Clock>,
    stats: Arc<InputStats>,
}

impl WinInput {
    /// An adapter stamping events on `clock`, which must be the engine's master clock
    /// so pointer times and capture frame times share one clock (ADR-0004).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            stats: Arc::new(InputStats::default()),
        }
    }

    /// Counters over every run of this adapter, for measurements and the stop log
    /// line. Never event contents.
    pub fn stats(&self) -> Arc<InputStats> {
        Arc::clone(&self.stats)
    }
}

/// Counters of the input thread, summed over every run of one [`WinInput`].
#[derive(Debug, Default)]
pub struct InputStats {
    reports: AtomicU64,
    cpu_nanos: AtomicU64,
}

impl InputStats {
    /// Mouse reports the handler read.
    pub fn reports(&self) -> u64 {
        self.reports.load(Ordering::Relaxed)
    }

    /// CPU time of the input threads that have ended, in nanoseconds.
    pub fn cpu_nanos(&self) -> u64 {
        self.cpu_nanos.load(Ordering::Relaxed)
    }
}

impl InputBackend for WinInput {
    fn name(&self) -> &'static str {
        crate::NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        // No permission keys: Raw Input needs none (the default `Granted`).
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
            .with(caps::WHEEL, CapabilityValue::Flag(true))
    }

    fn start(&self, mask: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            wanted: AtomicU8::new(model::mask_bits(mask)),
            window: AtomicIsize::new(0),
            stats: Arc::clone(&self.stats),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("rbs-input-win".into())
            .spawn({
                let control = Arc::clone(&control);
                let clock = Arc::clone(&self.clock);
                move || listen(&control, clock, sink, ready_tx)
            })
            .map_err(|_| InputError::Backend { code: codes::SPAWN })?;
        match ready_rx.recv_timeout(READY_WAIT) {
            Ok(Ok(())) => Ok(Box::new(Running {
                control,
                thread: Some(thread),
            })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(RecvTimeoutError::Timeout) => {
                // The thread checks the flag right after it reports and then removes
                // its registration; not joined here, so `start` keeps its bound.
                control.stop.store(true, Ordering::Release);
                Err(InputError::Backend {
                    code: codes::READY_TIMEOUT,
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(InputError::Backend {
                    code: codes::THREAD_ENDED,
                })
            }
        }
    }
}

/// State shared by the control thread and the input thread.
struct Control {
    stop: AtomicBool,
    /// The wanted mask ([`model::mask_bits`]).
    wanted: AtomicU8,
    /// The message-only window (`HWND` value), 0 before it exists and after it is gone.
    window: AtomicIsize,
    stats: Arc<InputStats>,
}

impl Control {
    /// Ends the thread's message loop. Any thread; never waits.
    fn wake(&self) {
        let window = self.window.load(Ordering::Acquire);
        if window != 0 {
            // SAFETY: posting to a window of another thread is allowed; a window that
            // is already gone makes the call fail, which is harmless.
            let _ = unsafe {
                PostMessageW(Some(HWND(window as *mut _)), WM_STOP, WPARAM(0), LPARAM(0))
            };
        }
    }
}

/// A started input thread.
struct Running {
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

impl RunningInput for Running {
    fn set_mask(&mut self, mask: EventMask) {
        // Raw Input always delivers the whole mouse; the handler filters.
        self.control
            .wanted
            .store(model::mask_bits(mask), Ordering::Release);
    }

    fn stop(mut self: Box<Self>) {
        self.halt();
    }
}

impl Running {
    fn halt(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.control.stop.store(true, Ordering::Release);
        self.control.wake();
        let _ = thread.join();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.halt();
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: forwards the arguments the system passed, unchanged.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// The message-only window and its Raw Input registration; removed on drop, on the
/// thread that made them.
struct Sink {
    window: HWND,
}

impl Sink {
    fn create() -> Result<Self, InputError> {
        // SAFETY: a null module name gives this executable's handle.
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|_| InputError::Backend {
                code: codes::NO_WINDOW,
            })?
            .into();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: `class` is fully initialised; registering a class that already
        // exists (an earlier start in this process) fails harmlessly and the existing
        // one, with the same procedure, is used.
        let _ = unsafe { RegisterClassExW(&class) };
        // SAFETY: a message-only window (parent `HWND_MESSAGE`) of the class above,
        // owned by this thread.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("RBS input"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|_| InputError::Backend {
            code: codes::NO_WINDOW,
        })?;
        let sink = Sink { window };
        let device = RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_GENERIC,
            usUsage: USAGE_MOUSE,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        // SAFETY: one valid device description and its size.
        unsafe { RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
            .map_err(|_| InputError::Backend {
                code: codes::NOT_REGISTERED,
            })?;
        Ok(sink)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let device = RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_GENERIC,
            usUsage: USAGE_MOUSE,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        // SAFETY: removes this process's mouse registration (the target must be null
        // with `RIDEV_REMOVE`), then destroys the window this thread created.
        unsafe {
            let _ =
                RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            let _ = DestroyWindow(self.window);
            if let Ok(instance) = GetModuleHandleW(None) {
                let _ = UnregisterClassW(CLASS, Some(instance.into()));
            }
        }
    }
}

/// The input thread.
fn listen(
    control: &Arc<Control>,
    clock: Arc<dyn Clock>,
    sink: InputSink,
    ready: SyncSender<Result<(), InputError>>,
) {
    // Per-monitor v2, like the capture adapter's mapping thread, so `GetCursorPos`
    // gives physical pixels of the virtual screen (capture-port.md, picture mapping
    // rule 5). Only this thread changes.
    // SAFETY: the constant is a documented sentinel; the call changes this thread only.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let window = match Sink::create() {
        Ok(window) => window,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    control
        .window
        .store(window.window.0 as isize, Ordering::Release);
    // `start` may give up between this send and its receive: the stop flag it sets is
    // checked right after.
    if ready.send(Ok(())).is_err() || control.stop.load(Ordering::Acquire) {
        control.window.store(0, Ordering::Release);
        return; // `window` drops here: registration removed, window destroyed.
    }
    drop(ready);
    let mut frequency = 0i64;
    // SAFETY: valid out pointer.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    let mut state = Handler {
        sink,
        clock,
        edge: EdgeClock::default(),
        frequency,
        control: Arc::clone(control),
    };
    let reports = control.stats.reports();
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a valid out pointer; messages of any window of this
        // thread. 0 is WM_QUIT, -1 an error: both end the loop.
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if got.0 <= 0 || message.message == WM_STOP || control.stop.load(Ordering::Acquire) {
            break;
        }
        if message.message == WM_INPUT {
            state.handle(HRAWINPUT(message.lParam.0 as *mut _));
        }
        // `WM_INPUT` still goes to `DefWindowProcW`, which frees the system's copy.
        // SAFETY: a message this thread just received.
        unsafe { DispatchMessageW(&message) };
    }
    control.window.store(0, Ordering::Release);
    drop(window);
    let cpu = thread_cpu_nanos();
    control.stats.cpu_nanos.fetch_add(cpu, Ordering::Relaxed);
    let reports = control.stats.reports() - reports;
    let cpu_ms = cpu as f64 / 1e6;
    rbs_core::info!(
        Area::Capture,
        "raw input stopped ({reports} mouse reports, {cpu_ms:.1} ms CPU)"
    );
}

/// What the `WM_INPUT` handler works with; lives on the input thread.
struct Handler {
    sink: InputSink,
    clock: Arc<dyn Clock>,
    edge: EdgeClock,
    frequency: i64,
    control: Arc<Control>,
}

impl Handler {
    /// One `WM_INPUT`. Real-time: no allocation, lock or log.
    fn handle(&mut self, input: HRAWINPUT) {
        let mut raw = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        // SAFETY: `raw` is a writable buffer of `size` bytes, large enough for any
        // mouse report; the header size is the one the API expects.
        let read = unsafe {
            GetRawInputData(
                input,
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || read == 0 || raw.header.dwType != RIM_TYPEMOUSE.0 {
            return;
        }
        self.control.stats.reports.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the header says this report is a mouse's, so `mouse` is the union
        // member the system wrote.
        let (flags, data) = unsafe {
            let buttons = raw.data.mouse.Anonymous.Anonymous;
            (buttons.usButtonFlags, buttons.usButtonData)
        };
        let wanted = model::mask_from_bits(self.control.wanted.load(Ordering::Relaxed));
        let pointer = wanted.contains(EventMask::POINTER);
        let buttons = wanted.contains(EventMask::BUTTONS) && flags & model::BUTTON_FLAGS != 0;
        let wheel = if wanted.contains(EventMask::WHEEL) {
            model::wheel(flags, data)
        } else {
            None
        };
        if !pointer && !buttons && wheel.is_none() {
            return;
        }
        let mut point = POINT::default();
        // SAFETY: valid out pointer.
        if unsafe { GetCursorPos(&mut point) }.is_err() {
            return;
        }
        let at = DesktopPoint::new(f64::from(point.x), f64::from(point.y));
        let time = self.stamp();
        if pointer {
            self.sink.pointer(PointerSample { time, at });
        }
        if buttons {
            for (flag, button, down) in model::TRANSITIONS {
                if flags & flag == 0 {
                    continue;
                }
                let event = if down {
                    InputEvent::ButtonDown { button, at }
                } else {
                    InputEvent::ButtonUp { button, at }
                };
                self.sink.event(TimedEvent { time, event });
            }
        }
        if let Some((dx, dy)) = wheel {
            self.sink.event(TimedEvent {
                time,
                event: InputEvent::Wheel { at, dx, dy },
            });
        }
    }

    /// The report's master time: QPC when it was read (`GetMessageTime` has only
    /// millisecond resolution), mapped through the edge clock.
    fn stamp(&mut self) -> Timestamp {
        let mut counter = 0i64;
        // SAFETY: valid out pointer.
        let _ = unsafe { QueryPerformanceCounter(&mut counter) };
        let host = model::qpc_to_nanos(counter, self.frequency);
        let master = self.clock.now();
        self.edge.observe(host, master);
        self.edge.to_master(host, master)
    }
}

/// CPU time (kernel and user) of the calling thread in nanoseconds, 0 on failure.
fn thread_cpu_nanos() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the pseudo handle of this thread and four valid out pointers.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_err() {
        return 0;
    }
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    (ticks(kernel) + ticks(user)).saturating_mul(100)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rbs_core::time::MonotonicClock;
    use rbs_input::InputMonitor;

    use super::*;

    /// A backend that hands its sink to the test and starts nothing.
    struct Capture(Arc<Mutex<Option<InputSink>>>);

    struct Idle;

    impl RunningInput for Idle {
        fn set_mask(&mut self, _: EventMask) {}
        fn stop(self: Box<Self>) {}
    }

    impl InputBackend for Capture {
        fn name(&self) -> &'static str {
            "capture"
        }
        fn capabilities(&self) -> CapabilitySet {
            WinInput::new(Arc::new(MonotonicClock::new())).capabilities()
        }
        fn start(
            &self,
            _: EventMask,
            sink: InputSink,
        ) -> Result<Box<dyn RunningInput>, InputError> {
            *self.0.lock().unwrap() = Some(sink);
            Ok(Box::new(Idle))
        }
    }

    /// Starting and stopping the real adapter is quick and leaves nothing behind;
    /// a second start in the same process works (the window class is reused).
    #[test]
    fn starts_and_stops_twice() {
        let input = WinInput::new(Arc::new(MonotonicClock::new()));
        let monitor = InputMonitor::new(Box::new(input));
        for _ in 0..2 {
            let (sub, _reader) = monitor.subscribe(EventMask::MOUSE).expect("subscribe");
            assert!(monitor.is_running());
            drop(sub);
        }
    }

    /// The mask filter and stamp allocate nothing (hot-path rule, ADR-0165). The
    /// `GetRawInputData` call itself is the OS's; an invalid handle makes it return
    /// early, so the filter path is measured on a stamp.
    #[test]
    fn stamping_does_not_allocate() {
        let slot = Arc::new(Mutex::new(None));
        let monitor = InputMonitor::new(Box::new(Capture(Arc::clone(&slot))));
        let (_sub, mut reader) = monitor.subscribe(EventMask::MOUSE).unwrap();
        let sink = slot.lock().unwrap().take().unwrap();
        let mut frequency = 0i64;
        // SAFETY: valid out pointer.
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        let mut handler = Handler {
            sink,
            clock: Arc::new(MonotonicClock::new()),
            edge: EdgeClock::default(),
            frequency,
            control: Arc::new(Control {
                stop: AtomicBool::new(false),
                wanted: AtomicU8::new(model::mask_bits(EventMask::MOUSE)),
                window: AtomicIsize::new(0),
                stats: Arc::new(InputStats::default()),
            }),
        };
        let _ = handler.stamp();
        let at = DesktopPoint::new(1.0, 2.0);
        let info = allocation_counter::measure(|| {
            for _ in 0..100 {
                let time = handler.stamp();
                handler.sink.pointer(PointerSample { time, at });
                handler.sink.event(TimedEvent {
                    time,
                    event: InputEvent::ButtonDown {
                        button: rbs_input::Button::Left,
                        at,
                    },
                });
                handler.handle(HRAWINPUT(std::ptr::null_mut()));
            }
        });
        assert_eq!(info.count_total, 0, "{info:?}");
        assert!(reader.latest_pointer().is_some());
        assert!(reader.next_event().is_some());
    }
}
