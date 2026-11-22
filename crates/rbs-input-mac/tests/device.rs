//! The macOS tap on this Mac, through the shared monitor.
//!
//! `starts_and_stops_through_the_monitor` needs Input Monitoring for the process's
//! responsible app (a terminal, or the app that started it); without it the test says
//! so and returns. `slow_posted_events_reach_the_reader` (`--ignored`: the first post
//! of a process takes about 0.7 s) posts marked events and also needs Accessibility
//! (post access). The posted events are swallowed by a second, active tap placed after
//! ours, so nothing reaches an app: no click lands on the screen.
//! Real mouse input during the run is told apart by position and skipped.
//! `manual_real_click` waits for a click by a person (`--ignored`).
//!
//! Every wait is for a condition, bounded by a hang guard (ADR-0032).

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail by panicking.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CGPoint, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGMouseButton, CGScrollEventUnit,
};
use rbs_core::time::{Clock, MonotonicClock, Timestamp};
use rbs_input::{
    Button, DesktopPoint, EventMask, InputError, InputEvent, InputMonitor, InputReader,
    PointerSample, TimedEvent,
};
use rbs_input_mac::MacInput;

const GUARD: Duration = Duration::from_secs(10);
/// Marks events this test posts (`kCGEventSourceUserData`).
const MARK: i64 = 0x5242_5349_4e50; // "RBSINP"

fn monitor() -> (InputMonitor, Arc<dyn Clock>) {
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    (
        InputMonitor::new(Box::new(MacInput::new(Arc::clone(&clock)))),
        clock,
    )
}

/// Runs `f` on its own thread and fails if it does not finish within `GUARD`.
fn guarded(name: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(GUARD) {
        Ok(()) => thread.join().unwrap(),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = thread.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{name} hung"),
    }
}

/// Spins until `ready` holds; panics after `GUARD` (a hang guard, not a timing).
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

#[test]
fn starts_and_stops_through_the_monitor() {
    guarded("start and stop", || {
        let (monitor, _) = monitor();
        assert_eq!(monitor.backend_name(), "cg-event-tap");
        assert!(!monitor.is_running(), "nothing runs before a subscription");
        let first = match monitor.subscribe(EventMask::POINTER) {
            Ok(pair) => pair,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipped: the system refused a listen-only mouse tap");
                return;
            }
            Err(e) => panic!("subscribe: {e}"),
        };
        assert!(monitor.is_running());
        // A second subscription widens the mask: the tap is rebuilt on its thread.
        let second = monitor
            .subscribe(EventMask::BUTTONS.union(EventMask::WHEEL))
            .unwrap();
        assert!(monitor.is_running());
        drop(first);
        assert!(monitor.is_running(), "one subscription is still open");
        drop(second);
        assert!(
            !monitor.is_running(),
            "the last drop stops and joins the tap"
        );
        // And again: a fresh start after a stop.
        let again = monitor.subscribe(EventMask::MOUSE).unwrap();
        assert!(monitor.is_running());
        drop(again);
        assert!(!monitor.is_running());
    });
}

#[test]
fn keys_are_refused() {
    let (monitor, _) = monitor();
    assert!(matches!(
        monitor.subscribe(EventMask::KEYS),
        Err(InputError::Unsupported(_))
    ));
    assert!(!monitor.is_running());
}

/// Counts and swallows marked events after our session tap, so they never reach an
/// app. Also records the raw timestamp of the last one it saw.
struct Swallow {
    seen: AtomicU64,
    raw_time: AtomicU64,
    /// Host time in nanoseconds when the callback saw it.
    host_time: AtomicU64,
}

unsafe extern "C-unwind" fn swallow(
    _proxy: CGEventTapProxy,
    _ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `user_info` is the `Swallow` the test keeps alive until the tap is gone.
    let state = unsafe { &*user_info.cast::<Swallow>() };
    // SAFETY: the event is valid for the call.
    let event_ref = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(event_ref), CGEventField::EventSourceUserData) == MARK {
        state
            .raw_time
            .store(CGEvent::timestamp(Some(event_ref)), Ordering::Relaxed);
        state.host_time.store(host_nanos(), Ordering::Relaxed);
        state.seen.fetch_add(1, Ordering::Release);
        return std::ptr::null_mut();
    }
    event.as_ptr()
}

/// An active tap at the annotated-session location on its own run-loop thread.
fn start_swallow(
    stop: Arc<AtomicBool>,
    state: Arc<Swallow>,
) -> Option<std::thread::JoinHandle<()>> {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let all_mouse: u64 = [1u32, 2, 3, 4, 5, 6, 7, 22, 25, 26, 27]
            .iter()
            .fold(0, |m, t| m | 1 << t);
        // SAFETY: `swallow` matches the callback type; `state` outlives the tap (held
        // by this thread until the tap is invalidated).
        let port: Option<CFRetained<CFMachPort>> = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::AnnotatedSessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                all_mouse,
                Some(swallow),
                Arc::as_ptr(&state).cast_mut().cast(),
            )
        };
        let Some(port) = port else {
            let _ = tx.send(false);
            return;
        };
        let run_loop = CFRunLoop::current().unwrap();
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0).unwrap();
        // SAFETY: immutable Core Foundation constants.
        let (common, default) = unsafe { (kCFRunLoopCommonModes, kCFRunLoopDefaultMode) };
        run_loop.add_source(Some(&source), common);
        CGEvent::tap_enable(&port, true);
        let _ = tx.send(true);
        while !stop.load(Ordering::Acquire) {
            CFRunLoop::run_in_mode(default, 0.05, false);
        }
        CGEvent::tap_enable(&port, false);
        run_loop.remove_source(Some(&source), common);
        port.invalidate();
    });
    if rx.recv_timeout(GUARD).unwrap_or(false) {
        Some(thread)
    } else {
        let _ = thread.join();
        None
    }
}

fn seen_host(state: &Swallow) -> u64 {
    state.host_time.load(Ordering::Relaxed)
}

fn post(event: &CGEvent) {
    CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, MARK);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
}

fn at_of(event: &InputEvent) -> Option<DesktopPoint> {
    match *event {
        InputEvent::ButtonDown { at, .. }
        | InputEvent::ButtonUp { at, .. }
        | InputEvent::Wheel { at, .. } => Some(at),
        _ => None,
    }
}

fn near(a: DesktopPoint, b: CGPoint) -> bool {
    (a.x - b.x).abs() < 1.0 && (a.y - b.y).abs() < 1.0
}

/// The next event at `here` (a posted one); real input elsewhere is skipped.
fn next_event(reader: &mut InputReader, here: CGPoint, what: &str) -> TimedEvent {
    let mut got = None;
    wait_for(what, || {
        while let Some(e) = reader.next_event() {
            if at_of(&e.event).is_some_and(|at| near(at, here)) {
                got = Some(e);
                return true;
            }
        }
        false
    });
    got.unwrap()
}

/// The newest pointer sample at `here` stamped at or after `posted`, searching the
/// history back from now (real moves elsewhere are skipped).
fn posted_sample(
    reader: &InputReader,
    here: CGPoint,
    posted: Timestamp,
    clock: &dyn Clock,
) -> Option<PointerSample> {
    let mut before = clock.now();
    while let Some(sample) = reader.pointer_at(before) {
        if sample.time + Duration::from_millis(2) < posted {
            return None;
        }
        if near(sample.at, here) {
            return Some(sample);
        }
        before = Timestamp::from_nanos(sample.time.as_nanos().checked_sub(1)?);
    }
    None
}

/// The event's master time lies between the moment it was posted and now: it is the
/// OS event time, not the (later) time the tap saw it. Delivery of posted events can
/// take hundreds of milliseconds, so the bound is on the post, not on now.
fn assert_stamped(time: Timestamp, posted: Timestamp, clock: &dyn Clock, what: &str) {
    let now = clock.now();
    assert!(time <= now, "{what}: event time after now");
    let early = Duration::from_millis(2);
    assert!(
        time + early >= posted,
        "{what}: stamped {:?} before it was posted",
        posted.saturating_since(time)
    );
    println!(
        "{what}: stamped {:.3} ms after the post, seen {:.3} ms after its stamp",
        time.saturating_since(posted).as_secs_f64() * 1e3,
        now.saturating_since(time).as_secs_f64() * 1e3,
    );
}

#[test]
#[ignore = "slow: the first CGEventPost of a process returns after about 0.7 s"]
fn slow_posted_events_reach_the_reader() {
    if !objc2_core_graphics::CGPreflightPostEventAccess() {
        eprintln!("skipped: posting events needs Accessibility for this process");
        return;
    }
    guarded("posted events", || {
        let (monitor, clock) = monitor();
        let (_sub, mut reader) = match monitor.subscribe(EventMask::MOUSE) {
            Ok(pair) => pair,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipped: the system refused a listen-only mouse tap");
                return;
            }
            Err(e) => panic!("subscribe: {e}"),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Swallow {
            seen: AtomicU64::new(0),
            raw_time: AtomicU64::new(0),
            host_time: AtomicU64::new(0),
        });
        let Some(swallower) = start_swallow(Arc::clone(&stop), Arc::clone(&state)) else {
            eprintln!("skipped: no active tap to swallow posted events");
            return;
        };
        let here = CGEvent::location(CGEvent::new(None).as_deref());
        let make = |ty, button| CGEvent::new_mouse_event(None, ty, here, button).unwrap();

        // A move first: harmless if it were not swallowed, and proves the swallow tap
        // works before any click is posted.
        let posted = clock.now();
        let posted_host = host_nanos();
        post(&make(CGEventType::MouseMoved, CGMouseButton::Left));
        println!(
            "the first post returned after {:.3} ms",
            (host_nanos() - posted_host) as f64 / 1e6
        );
        wait_for("the swallow tap to see the move", || {
            state.seen.load(Ordering::Acquire) >= 1
        });
        let mut sample = None;
        wait_for("the pointer sample", || {
            sample = posted_sample(&reader, here, posted, clock.as_ref());
            sample.is_some()
        });
        let sampled = clock.now();
        println!(
            "swallow tap saw the move {:.3} ms after the post, the reader {:.3} ms after",
            (seen_host(&state) as f64 - posted_host as f64) / 1e6,
            sampled.saturating_since(posted).as_secs_f64() * 1e3
        );
        // Which unit CGEventGetTimestamp has here: nanoseconds when it is close to the
        // host time in nanoseconds, mach ticks when close to the tick count.
        let raw = state.raw_time.load(Ordering::Relaxed);
        let seen = state.host_time.load(Ordering::Relaxed);
        let (numer, denom) = timebase();
        let unit = if raw.abs_diff(seen) < raw.abs_diff(seen * u64::from(denom) / u64::from(numer))
        {
            "nanoseconds"
        } else {
            "mach ticks"
        };
        println!(
            "CGEventGetTimestamp {raw} in {unit} (host {seen} ns at the swallow tap, {} ticks now, timebase {numer}/{denom})",
            mach_ticks()
        );
        let sample = sample.unwrap();
        assert_stamped(sample.time, posted, clock.as_ref(), "move");

        let posted = clock.now();
        post(&make(CGEventType::LeftMouseDown, CGMouseButton::Left));
        post(&make(CGEventType::LeftMouseUp, CGMouseButton::Left));
        let down = next_event(&mut reader, here, "button down");
        assert!(
            matches!(
                down.event,
                InputEvent::ButtonDown {
                    button: Button::Left,
                    ..
                }
            ),
            "{down:?}"
        );
        assert_stamped(down.time, posted, clock.as_ref(), "left down");
        let up = next_event(&mut reader, here, "button up");
        assert!(
            matches!(
                up.event,
                InputEvent::ButtonUp {
                    button: Button::Left,
                    ..
                }
            ),
            "{up:?}"
        );
        assert!(up.time >= down.time, "times do not decrease");

        let other = make(CGEventType::OtherMouseDown, CGMouseButton::Center);
        post(&other);
        let middle = next_event(&mut reader, here, "middle down");
        assert!(
            matches!(
                middle.event,
                InputEvent::ButtonDown {
                    button: Button::Middle,
                    ..
                }
            ),
            "{middle:?}"
        );
        post(&make(CGEventType::OtherMouseUp, CGMouseButton::Center));
        let _ = next_event(&mut reader, here, "middle up");

        // Wheel: three lines on axis 1, two the other way on axis 2.
        let wheel =
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 2, 3, -2, 0).unwrap();
        post(&wheel);
        let w = next_event(&mut reader, here, "wheel");
        match w.event {
            InputEvent::Wheel { dx, dy, .. } => {
                println!("wheel posted wheel1=+3 wheel2=-2 -> dy {dy}, dx {dx}");
                assert!(dy > 0.0, "axis 1 keeps its sign: {dy}");
                assert!(dx < 0.0, "axis 2 keeps its sign: {dx}");
            }
            other => panic!("expected a wheel event, got {other:?}"),
        }
        assert_eq!(reader.dropped(), 0);

        stop.store(true, Ordering::Release);
        swallower.join().unwrap();
        drop(_sub);
        drop(reader);
        assert!(!monitor.is_running());
    });
}

/// Waits for one real click and prints its time against the clock.
#[test]
#[ignore = "manual: click once within 20 s"]
fn manual_real_click() {
    let (monitor, clock) = monitor();
    let (_sub, mut reader) = monitor.subscribe(EventMask::MOUSE).unwrap();
    println!("click anywhere within 20 s");
    let start = Instant::now();
    loop {
        if let Some(e) = reader.next_event() {
            let now = clock.now();
            println!(
                "{:?} at {:?}; now {:?}; {:.3} ms before now",
                e.event,
                e.time,
                now,
                now.saturating_since(e.time).as_secs_f64() * 1e3
            );
            if matches!(e.event, InputEvent::ButtonUp { .. }) {
                break;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no click in 20 s"
        );
        std::thread::yield_now();
    }
}

fn host_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

fn mach_ticks() -> u64 {
    // SAFETY: no arguments.
    unsafe { mach_absolute_time() }
}

fn timebase() -> (u32, u32) {
    let mut t = Timebase { numer: 0, denom: 0 };
    // SAFETY: valid out pointer.
    unsafe { mach_timebase_info(&mut t) };
    (t.numer, t.denom)
}
