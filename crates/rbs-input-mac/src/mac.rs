//! The tap thread and its callback (crate docs, "How events travel").

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use block2::RcBlock;
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopRunResult, CFRunLoopSource, CFType,
    kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::log::Area;
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::EdgeClock;
use rbs_input::{
    DesktopPoint, EventMask, InputBackend, InputError, InputEvent, InputSink, PointerSample,
    RunningInput, TimedEvent, caps,
};

use crate::model::{self, Kind};

/// How long `start` waits for the tap thread to report.
const READY_WAIT: Duration = Duration::from_millis(500);
/// How long one run of the run loop lasts before the thread checks its flags anyway.
/// Stop and mask changes wake the loop at once; this only bounds a lost wake-up.
const RUN_SECONDS: f64 = 2.0;

/// `InputError::Backend` codes of this adapter.
pub mod codes {
    /// The thread could not be spawned.
    pub const SPAWN: i64 = 1;
    /// The tap thread did not report within 500 ms.
    pub const READY_TIMEOUT: i64 = 2;
    /// The tap thread ended without reporting.
    pub const THREAD_ENDED: i64 = 3;
    /// The thread has no run loop.
    pub const NO_RUN_LOOP: i64 = 4;
    /// The tap's run-loop source could not be created.
    pub const NO_SOURCE: i64 = 5;
}

/// Mouse input from a listen-only `CGEventTap`.
pub struct MacInput {
    clock: Arc<dyn Clock>,
    stats: Arc<TapStats>,
}

impl MacInput {
    /// An adapter stamping events on `clock`, which must be the engine's master clock
    /// so pointer times and capture frame times share one clock (ADR-0004).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            stats: Arc::new(TapStats::default()),
        }
    }

    /// Counters over every run of this adapter, for measurements (`examples/tap_cost.rs`)
    /// and the stop log line. Never event contents.
    pub fn stats(&self) -> Arc<TapStats> {
        Arc::clone(&self.stats)
    }
}

/// Counters of the tap, summed over every run of one [`MacInput`].
#[derive(Debug, Default)]
pub struct TapStats {
    events: AtomicU64,
    reenabled: AtomicU64,
    cpu_nanos: AtomicU64,
}

impl TapStats {
    /// Events the callback handled (every type the tap asked for).
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Times the system disabled the tap and the callback enabled it again.
    pub fn reenabled(&self) -> u64 {
        self.reenabled.load(Ordering::Relaxed)
    }

    /// CPU time of the tap threads that have ended, in nanoseconds.
    pub fn cpu_nanos(&self) -> u64 {
        self.cpu_nanos.load(Ordering::Relaxed)
    }
}

impl InputBackend for MacInput {
    fn name(&self) -> &'static str {
        crate::NAME
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
            .with(caps::WHEEL, CapabilityValue::Flag(true))
    }

    fn start(&self, mask: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        // Without Input Monitoring the system creates even a listen-only mouse tap but
        // never delivers an event to it (macOS 26.7, spec "macOS adapter"), so refuse
        // instead of running a silent tap. The preflight never shows a prompt.
        if !objc2_core_graphics::CGPreflightListenEventAccess() {
            return Err(InputError::PermissionDenied);
        }
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            wanted: AtomicU8::new(model::mask_bits(mask)),
            stats: Arc::clone(&self.stats),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("rbs-input-mac".into())
            .spawn({
                let control = Arc::clone(&control);
                let clock = Arc::clone(&self.clock);
                move || listen(&control, clock, sink, ready_tx)
            })
            .map_err(|_| InputError::Backend { code: codes::SPAWN })?;
        match ready_rx.recv_timeout(READY_WAIT) {
            Ok(Ok(run_loop)) => Ok(Box::new(Running {
                control,
                run_loop,
                thread: Some(thread),
            })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(RecvTimeoutError::Timeout) => {
                // The thread sees the closed channel when it reports and removes its
                // tap; it is not joined here, so `start` keeps its 500 ms bound.
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

/// State shared by the control thread and the tap thread.
struct Control {
    stop: AtomicBool,
    /// The wanted mask ([`model::mask_bits`]).
    wanted: AtomicU8,
    stats: Arc<TapStats>,
}

/// The tap thread's run loop, for the calls Core Foundation allows from any thread.
struct LoopHandle(CFRetained<CFRunLoop>);

// SAFETY: only `CFRunLoopPerformBlock`, `CFRunLoopWakeUp` and the retain count are used
// through this handle from other threads; Core Foundation documents those as safe to
// call from any thread. The loop is run, and sources are added or removed, only on its
// own thread, never through this handle.
unsafe impl Send for LoopHandle {}
// SAFETY: as for `Send`; every call through a shared reference is thread-safe.
unsafe impl Sync for LoopHandle {}

impl LoopHandle {
    /// Makes the loop's current or next run return, even when it has not started yet.
    fn interrupt(&self) {
        let block = RcBlock::new(|| {
            if let Some(current) = CFRunLoop::current() {
                current.stop();
            }
        });
        let Some(mode) = default_mode() else {
            return;
        };
        let mode: &CFType = mode;
        // SAFETY: `mode` is the default run-loop mode constant, a CFString, valid for
        // the process; the block takes no arguments and only stops the run loop it
        // runs on. Core Foundation copies the block, so it may be dropped after this.
        unsafe { self.0.perform_block(Some(mode), Some(&block)) };
        self.0.wake_up();
    }
}

fn default_mode() -> Option<&'static CFRunLoopMode> {
    // SAFETY: reading an immutable Core Foundation constant that is set before any
    // code of ours runs.
    unsafe { kCFRunLoopDefaultMode }
}

fn common_modes() -> Option<&'static CFRunLoopMode> {
    // SAFETY: as in `default_mode`.
    unsafe { kCFRunLoopCommonModes }
}

/// A started tap thread.
struct Running {
    control: Arc<Control>,
    run_loop: LoopHandle,
    thread: Option<JoinHandle<()>>,
}

impl RunningInput for Running {
    fn set_mask(&mut self, mask: EventMask) {
        self.control
            .wanted
            .store(model::mask_bits(mask), Ordering::Release);
        self.run_loop.interrupt();
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
        self.run_loop.interrupt();
        let _ = thread.join();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.halt();
    }
}

/// What the callback works with. Lives on the tap thread for as long as a tap uses it.
struct TapState {
    sink: InputSink,
    clock: Arc<dyn Clock>,
    edge: EdgeClock,
    numer: u32,
    denom: u32,
    control: Arc<Control>,
    /// The installed tap, to enable it again. Set before the run loop runs.
    tap: *const CFMachPort,
}

/// Owns the boxed [`TapState`] the callback's `user_info` points to.
struct OwnedState(*mut TapState);

impl Drop for OwnedState {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::into_raw` in `listen`, and every tap using
        // it was removed and invalidated before this drop (taps are declared after the
        // owner, so they drop first).
        drop(unsafe { Box::from_raw(self.0) });
    }
}

/// An installed tap and its run-loop source.
struct Tap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

impl Tap {
    /// Creates a tap for `mask`, adds it to `run_loop` and enables it. `None` when the
    /// system refuses the tap; `Err` when its source cannot be made.
    fn install(
        state: *mut TapState,
        mask: EventMask,
        run_loop: &CFRetained<CFRunLoop>,
    ) -> Result<Tap, InputError> {
        // SAFETY: `on_event` has the callback signature Core Graphics expects and never
        // unwinds into it; `state` points to a live `TapState` that outlives the tap
        // (`OwnedState` drops after every `Tap`), and the callback runs only on this
        // thread, inside the run loop, where nothing else touches the state.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                model::cg_event_mask(mask),
                Some(on_event),
                state.cast::<c_void>(),
            )
        }
        .ok_or(InputError::PermissionDenied)?;
        let (Some(source), Some(modes)) = (
            CFMachPort::new_run_loop_source(None, Some(&port), 0),
            common_modes(),
        ) else {
            port.invalidate();
            return Err(InputError::Backend {
                code: codes::NO_SOURCE,
            });
        };
        run_loop.add_source(Some(&source), Some(modes));
        // SAFETY: `state` is live (see above) and the run loop is not running, so the
        // callback cannot be using it now.
        unsafe { (*state).tap = CFRetained::as_ptr(&port).as_ptr() };
        CGEvent::tap_enable(&port, true);
        Ok(Tap {
            port,
            source,
            run_loop: run_loop.clone(),
        })
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.port, false);
        self.run_loop
            .remove_source(Some(&self.source), common_modes());
        self.port.invalidate();
    }
}

/// The tap thread.
fn listen(
    control: &Arc<Control>,
    clock: Arc<dyn Clock>,
    sink: InputSink,
    ready: SyncSender<Result<LoopHandle, InputError>>,
) {
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(Err(InputError::Backend {
            code: codes::NO_RUN_LOOP,
        }));
        return;
    };
    let (numer, denom) = timebase();
    let owned = OwnedState(Box::into_raw(Box::new(TapState {
        sink,
        clock,
        edge: EdgeClock::default(),
        numer,
        denom,
        control: Arc::clone(control),
        tap: ptr::null(),
    })));
    let mut built = control.wanted.load(Ordering::Acquire);
    let mut tap = match Tap::install(owned.0, model::mask_from_bits(built), &run_loop) {
        Ok(tap) => tap,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if ready.send(Ok(LoopHandle(run_loop.clone()))).is_err() {
        return; // `start` gave up waiting.
    }
    drop(ready);
    let (events, reenabled) = (control.stats.events(), control.stats.reenabled());
    let mut rebuilds = 0u64;
    let mut ended = false;
    loop {
        let result = CFRunLoop::run_in_mode(default_mode(), RUN_SECONDS, false);
        if control.stop.load(Ordering::Acquire) {
            break;
        }
        if result == CFRunLoopRunResult::Finished {
            // No source left: the system invalidated the tap.
            ended = true;
            break;
        }
        let wanted = control.wanted.load(Ordering::Acquire);
        if wanted != built {
            built = wanted;
            match Tap::install(owned.0, model::mask_from_bits(wanted), &run_loop) {
                Ok(new) => {
                    tap = new; // drops (removes) the old tap
                    rebuilds += 1;
                }
                Err(e) => {
                    // Keep the old tap: the callback filters by the wanted mask, so
                    // only a wider mask is missed.
                    // `install` changes the state's tap only on success.
                    rbs_core::warn!(Area::Capture, "input tap: new event mask refused: {e}");
                }
            }
        }
    }
    drop(tap);
    drop(owned);
    let cpu = thread_cpu_nanos();
    control.stats.cpu_nanos.fetch_add(cpu, Ordering::Relaxed);
    let events = control.stats.events() - events;
    let reenabled = control.stats.reenabled() - reenabled;
    let cpu_ms = cpu as f64 / 1e6;
    if ended {
        rbs_core::warn!(
            Area::Capture,
            "input tap ended by the system ({events} events, {reenabled} re-enables, {rebuilds} mask changes, {cpu_ms:.1} ms CPU)"
        );
    } else {
        rbs_core::info!(
            Area::Capture,
            "input tap stopped ({events} events, {reenabled} re-enables, {rebuilds} mask changes, {cpu_ms:.1} ms CPU)"
        );
    }
}

/// The tap callback. Real-time: no allocation, lock or log.
unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `user_info` is the `TapState` pointer given to `tap_create`, live
        // while the tap is installed; the callback runs only on the tap thread inside
        // the run loop, where no other reference to the state exists.
        let state = unsafe { &mut *user_info.cast::<TapState>() };
        // SAFETY: Core Graphics passes an event valid for the duration of the call.
        let event = unsafe { event.as_ref() };
        state.handle(ty.0, event);
    }));
    event.as_ptr()
}

impl TapState {
    fn handle(&mut self, ty: u32, event: &CGEvent) {
        let Some(kind) = model::classify(ty) else {
            return;
        };
        if kind == Kind::Disabled {
            // SAFETY: `tap` is the installed tap's port, retained by its `Tap` until
            // after the run loop stops calling us.
            if let Some(port) = unsafe { self.tap.as_ref() } {
                CGEvent::tap_enable(port, true);
                self.control.stats.reenabled.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        self.control.stats.events.fetch_add(1, Ordering::Relaxed);
        let wanted = model::mask_from_bits(self.control.wanted.load(Ordering::Relaxed));
        let class = match kind {
            Kind::Move => EventMask::POINTER,
            Kind::Down(_) | Kind::Up(_) => EventMask::BUTTONS,
            Kind::Wheel => EventMask::WHEEL,
            Kind::Disabled => return,
        };
        let pointer = wanted.contains(EventMask::POINTER);
        let discrete = class != EventMask::POINTER && wanted.contains(class);
        if !pointer && !discrete {
            return;
        }
        let time = self.stamp(event);
        let location = CGEvent::location(Some(event));
        let at = DesktopPoint::new(location.x, location.y);
        if pointer {
            self.sink.pointer(PointerSample { time, at });
        }
        if !discrete {
            return;
        }
        let number =
            || CGEvent::integer_value_field(Some(event), CGEventField::MouseEventButtonNumber);
        let event = match kind {
            Kind::Down(side) => match model::button(side, number()) {
                Some(button) => InputEvent::ButtonDown { button, at },
                None => return,
            },
            Kind::Up(side) => match model::button(side, number()) {
                Some(button) => InputEvent::ButtonUp { button, at },
                None => return,
            },
            Kind::Wheel => {
                let axis = |field| CGEvent::double_value_field(Some(event), field) as f32;
                InputEvent::Wheel {
                    at,
                    dx: axis(CGEventField::ScrollWheelEventFixedPtDeltaAxis2),
                    dy: axis(CGEventField::ScrollWheelEventFixedPtDeltaAxis1),
                }
            }
            Kind::Move | Kind::Disabled => return,
        };
        self.sink.event(TimedEvent { time, event });
    }

    /// The event's master time: the edge clock observes (host now, master now), then
    /// maps the event's host time.
    fn stamp(&mut self, event: &CGEvent) -> Timestamp {
        let host_now = host_nanos();
        let master_now = self.clock.now();
        self.edge.observe(host_now, master_now);
        let raw = CGEvent::timestamp(Some(event));
        let at = model::event_host_nanos(raw, self.numer, self.denom, host_now);
        self.edge.to_master(at, master_now)
    }
}

/// Nanoseconds on the host clock (`mach_absolute_time` scaled), the clock of
/// `CGEventGetTimestamp` and of ScreenCaptureKit sample times.
pub(crate) fn host_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// CPU time of the calling thread in nanoseconds, 0 if the call fails.
fn thread_cpu_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// The mach tick to nanosecond ratio, `(1, 1)` if the call fails.
pub(crate) fn timebase() -> (u32, u32) {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: `info` is a valid out pointer of the layout the call expects.
    let status = unsafe { mach_timebase_info(&mut info) };
    if status != 0 || info.denom == 0 {
        (1, 1)
    } else {
        (info.numer, info.denom)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{CGMouseButton, CGScrollEventUnit};
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
            MacInput::new(Arc::new(MonotonicClock::new())).capabilities()
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

    /// The callback's work allocates nothing (hot-path rule, ADR-0165).
    #[test]
    #[ignore = "slow: creating the first CGEvent of a process connects to the window server (0.3-2 s)"]
    fn slow_handling_an_event_does_not_allocate() {
        let slot = Arc::new(Mutex::new(None));
        let monitor = InputMonitor::new(Box::new(Capture(Arc::clone(&slot))));
        let (_sub, mut reader) = monitor.subscribe(EventMask::MOUSE).unwrap();
        let sink = slot.lock().unwrap().take().unwrap();
        let (numer, denom) = timebase();
        let mut state = TapState {
            sink,
            clock: Arc::new(MonotonicClock::new()),
            edge: EdgeClock::default(),
            numer,
            denom,
            control: Arc::new(Control {
                stop: AtomicBool::new(false),
                wanted: AtomicU8::new(model::mask_bits(EventMask::MOUSE)),
                stats: Arc::new(TapStats::default()),
            }),
            tap: ptr::null(),
        };
        let at = CGPoint { x: 10.0, y: 20.0 };
        let events = (
            CGEvent::new_mouse_event(None, CGEventType::LeftMouseDown, at, CGMouseButton::Left),
            CGEvent::new_mouse_event(None, CGEventType::MouseMoved, at, CGMouseButton::Left),
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 1, 1, 0, 0),
        );
        let (Some(down), Some(moved), Some(wheel)) = events else {
            eprintln!("skipped: no window server session to create events in");
            return;
        };
        // Warm up once (first reads may set up lazily held state).
        state.handle(model::cg::MOVED, &moved);
        let info = allocation_counter::measure(|| {
            for _ in 0..100 {
                state.handle(model::cg::MOVED, &moved);
                state.handle(model::cg::LEFT_DOWN, &down);
                state.handle(model::cg::SCROLL_WHEEL, &wheel);
                state.handle(model::cg::DISABLED_BY_TIMEOUT, &moved);
            }
        });
        assert_eq!(info.count_total, 0, "{info:?}");
        assert_eq!(state.control.stats.events(), 301);
        assert!(reader.latest_pointer().is_some());
        let mut events = 0;
        while reader.next_event().is_some() {
            events += 1;
        }
        assert_eq!(events, 200, "downs and wheels; the ring holds 256");
    }
}
