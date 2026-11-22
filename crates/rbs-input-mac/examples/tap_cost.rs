//! What the tap costs (ADR-0165 decision 8): runs a mouse subscription for 10 s and
//! prints events per second and the CPU time of the tap thread.
//!
//! `cargo run --release -p rbs-input-mac --example tap_cost` measures whatever the
//! mouse does meanwhile (idle when nobody touches it). With `--posted` it also posts
//! 1000 marked moves a second at the pointer's own position and swallows them with an
//! active tap placed after ours, so the cursor and apps see nothing (needs
//! Accessibility for the terminal; the posting thread's own CPU is not counted).

#![allow(clippy::unwrap_used, clippy::expect_used)] // A measurement tool fails by panicking.

#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{
        CFMachPort, CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
    };
    use objc2_core_graphics::{
        CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventTapProxy, CGEventType, CGMouseButton,
    };
    use rbs_core::time::{Clock, MonotonicClock};
    use rbs_input::{EventMask, InputMonitor};
    use rbs_input_mac::MacInput;

    const RUN: Duration = Duration::from_secs(10);
    const MARK: i64 = 0x5242_5354_4150; // "RBSTAP"

    static SWALLOWED: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C-unwind" fn swallow(
        _proxy: CGEventTapProxy,
        _ty: CGEventType,
        event: NonNull<CGEvent>,
        _user: *mut c_void,
    ) -> *mut CGEvent {
        // SAFETY: the event is valid for the call.
        let e = unsafe { event.as_ref() };
        if CGEvent::integer_value_field(Some(e), CGEventField::EventSourceUserData) == MARK {
            SWALLOWED.fetch_add(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        event.as_ptr()
    }

    fn swallow_thread(stop: Arc<AtomicBool>, ready: std::sync::mpsc::Sender<bool>) {
        let mask = 1u64 << 5;
        // SAFETY: `swallow` matches the callback type and uses no user data.
        let Some(port) = (unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::AnnotatedSessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(swallow),
                std::ptr::null_mut(),
            )
        }) else {
            let _ = ready.send(false);
            return;
        };
        let run_loop = CFRunLoop::current().unwrap();
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0).unwrap();
        // SAFETY: immutable Core Foundation constants.
        let (common, default) = unsafe { (kCFRunLoopCommonModes, kCFRunLoopDefaultMode) };
        run_loop.add_source(Some(&source), common);
        CGEvent::tap_enable(&port, true);
        let _ = ready.send(true);
        while !stop.load(Ordering::Acquire) {
            CFRunLoop::run_in_mode(default, 0.05, false);
        }
        CGEvent::tap_enable(&port, false);
        run_loop.remove_source(Some(&source), common);
        port.invalidate();
    }

    pub fn run() {
        let posted = std::env::args().any(|a| a == "--posted");
        println!(
            "Input Monitoring granted: {}, Accessibility (post) granted: {}",
            objc2_core_graphics::CGPreflightListenEventAccess(),
            objc2_core_graphics::CGPreflightPostEventAccess()
        );
        let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
        let input = MacInput::new(Arc::clone(&clock));
        let stats = input.stats();
        let monitor = InputMonitor::new(Box::new(input));
        let (sub, mut reader) = monitor.subscribe(EventMask::MOUSE).expect("subscribe");

        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        if posted {
            let (tx, rx) = std::sync::mpsc::channel();
            let s = Arc::clone(&stop);
            threads.push(std::thread::spawn(move || swallow_thread(s, tx)));
            assert!(
                rx.recv().unwrap_or(false),
                "no active tap: Accessibility needed"
            );
            let s = Arc::clone(&stop);
            threads.push(std::thread::spawn(move || {
                let start = Instant::now();
                let mut n = 0u64;
                while !s.load(Ordering::Acquire) {
                    let here = CGEvent::location(CGEvent::new(None).as_deref());
                    if let Some(e) = CGEvent::new_mouse_event(
                        None,
                        CGEventType::MouseMoved,
                        here,
                        CGMouseButton::Left,
                    ) {
                        CGEvent::set_integer_value_field(
                            Some(&e),
                            CGEventField::EventSourceUserData,
                            MARK,
                        );
                        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
                    }
                    n += 1;
                    // Pace at 1000 a second against the start, not per sleep.
                    let due = start + Duration::from_millis(n);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
            }));
        }

        let start = Instant::now();
        let mut discrete = 0u64;
        while start.elapsed() < RUN {
            while reader.next_event().is_some() {
                discrete += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Release);
        for t in threads {
            let _ = t.join();
        }
        let elapsed = start.elapsed().as_secs_f64();
        drop(sub);
        drop(reader);
        assert!(!monitor.is_running());
        let events = stats.events();
        let cpu_ms = stats.cpu_nanos() as f64 / 1e6;
        println!(
            "{}: {elapsed:.1} s, {events} tapped events ({:.1}/s), {discrete} button/wheel events, \
             {} swallowed posts, tap thread CPU {cpu_ms:.2} ms ({:.3} % of one core), {} re-enables",
            if posted { "posted 1000 Hz" } else { "idle" },
            events as f64 / elapsed,
            SWALLOWED.load(Ordering::Relaxed),
            cpu_ms / (elapsed * 1e3) * 100.0,
            stats.reenabled(),
        );
    }
}
