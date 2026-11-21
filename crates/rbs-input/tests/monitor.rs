//! The shared monitor against a scripted backend (`docs/specs/input-port.md`, "Tests").

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::time::Timestamp;
use rbs_input::{
    Button, DesktopPoint, EVENT_RING, EventMask, InputBackend, InputError, InputEvent,
    InputMonitor, InputSink, MAX_SUBSCRIPTIONS, PointerSample, RunningInput, TimedEvent, caps,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Start(EventMask),
    SetMask(EventMask),
    Stop,
}

#[derive(Default)]
struct Script {
    calls: Vec<Call>,
    sink: Option<InputSink>,
    fail_start: bool,
}

#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Script>>);

impl Scripted {
    fn lock(&self) -> MutexGuard<'_, Script> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }
    fn click(&self, ms: u64, button: Button) {
        let script = self.lock();
        // Without a started backend nothing is delivered; the test's asserts catch it.
        let Some(sink) = script.sink.as_ref() else {
            return;
        };
        sink.event(TimedEvent {
            time: ts(ms),
            event: InputEvent::ButtonDown {
                button,
                at: DesktopPoint::new(10.0, 20.0),
            },
        });
    }
    fn wheel(&self, ms: u64) {
        let script = self.lock();
        // Without a started backend nothing is delivered; the test's asserts catch it.
        let Some(sink) = script.sink.as_ref() else {
            return;
        };
        sink.event(TimedEvent {
            time: ts(ms),
            event: InputEvent::Wheel {
                at: DesktopPoint::new(0.0, 0.0),
                dx: 0.0,
                dy: 1.0,
            },
        });
    }
    fn moved(&self, ms: u64, x: f64) {
        let script = self.lock();
        // Without a started backend nothing is delivered; the test's asserts catch it.
        let Some(sink) = script.sink.as_ref() else {
            return;
        };
        sink.pointer(PointerSample {
            time: ts(ms),
            at: DesktopPoint::new(x, 0.0),
        });
    }
}

struct Running(Scripted);

impl RunningInput for Running {
    fn set_mask(&mut self, mask: EventMask) {
        self.0.lock().calls.push(Call::SetMask(mask));
    }
    fn stop(self: Box<Self>) {
        let mut script = self.0.lock();
        script.calls.push(Call::Stop);
        script.sink = None;
    }
}

impl InputBackend for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
            .with(caps::WHEEL, CapabilityValue::Flag(true))
    }
    fn start(&self, mask: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        let mut script = self.lock();
        if script.fail_start {
            return Err(InputError::Backend { code: 5 });
        }
        script.calls.push(Call::Start(mask));
        script.sink = Some(sink);
        Ok(Box::new(Running(self.clone())))
    }
}

fn ts(ms: u64) -> Timestamp {
    Timestamp::from_nanos(ms * 1_000_000)
}

fn monitor() -> (InputMonitor, Scripted) {
    let script = Scripted::default();
    (InputMonitor::new(Box::new(script.clone())), script)
}

#[test]
fn nothing_runs_before_the_first_subscription() {
    let (monitor, script) = monitor();
    assert!(!monitor.is_running());
    assert!(script.calls().is_empty());
}

#[test]
fn first_subscription_starts_and_last_drop_stops() {
    let (monitor, script) = monitor();
    let (a, ra) = monitor.subscribe(EventMask::BUTTONS).expect("subscribe a");
    let (b, rb) = monitor
        .subscribe(EventMask::BUTTONS | EventMask::POINTER)
        .expect("subscribe b");
    assert!(monitor.is_running());
    drop(a);
    drop(ra);
    assert!(monitor.is_running());
    drop(b);
    assert!(!monitor.is_running());
    drop(rb);
    assert_eq!(
        script.calls(),
        vec![
            Call::Start(EventMask::BUTTONS),
            Call::SetMask(EventMask::BUTTONS | EventMask::POINTER),
            Call::Stop,
        ]
    );
}

#[test]
fn mask_narrows_when_a_wider_subscription_leaves() {
    let (monitor, script) = monitor();
    let (_a, _ra) = monitor.subscribe(EventMask::BUTTONS).expect("a");
    let (b, _rb) = monitor.subscribe(EventMask::MOUSE).expect("b");
    drop(b);
    assert_eq!(
        script.calls().last(),
        Some(&Call::SetMask(EventMask::BUTTONS))
    );
}

#[test]
fn readers_get_only_their_classes() {
    let (monitor, script) = monitor();
    let (_a, mut buttons) = monitor.subscribe(EventMask::BUTTONS).expect("a");
    let (_b, mut wheel) = monitor.subscribe(EventMask::WHEEL).expect("b");
    script.click(1, Button::Left);
    script.wheel(2);
    script.moved(3, 7.0);
    assert_eq!(buttons.next_event().map(|e| e.time), Some(ts(1)));
    assert_eq!(buttons.next_event(), None);
    assert_eq!(wheel.next_event().map(|e| e.time), Some(ts(2)));
    assert_eq!(wheel.next_event(), None);
    // Neither asked for the pointer.
    assert_eq!(buttons.latest_pointer(), None);
    assert_eq!(wheel.pointer_at(ts(10)), None);
}

#[test]
fn pointer_history_is_shared() {
    let (monitor, script) = monitor();
    let (_a, a) = monitor.subscribe(EventMask::POINTER).expect("a");
    let (_b, b) = monitor
        .subscribe(EventMask::POINTER | EventMask::BUTTONS)
        .expect("b");
    script.moved(10, 1.0);
    script.moved(20, 2.0);
    assert_eq!(a.pointer_at(ts(15)).map(|s| s.at.x), Some(1.0));
    assert_eq!(b.latest_pointer().map(|s| s.at.x), Some(2.0));
    assert_eq!(b.pointer_at(ts(5)), None);
}

#[test]
fn full_ring_counts_drops_per_reader() {
    let (monitor, script) = monitor();
    let (_a, a) = monitor.subscribe(EventMask::BUTTONS).expect("a");
    let (_b, mut b) = monitor.subscribe(EventMask::BUTTONS).expect("b");
    for ms in 0..EVENT_RING as u64 + 2 {
        script.click(ms, Button::Right);
        // b keeps up, a does not read.
        assert!(b.next_event().is_some());
    }
    assert_eq!(a.dropped(), 2);
    assert_eq!(b.dropped(), 0);
}

#[test]
fn keys_and_empty_masks_are_refused() {
    let (monitor, script) = monitor();
    assert!(matches!(
        monitor.subscribe(EventMask::KEYS),
        Err(InputError::Unsupported(_))
    ));
    assert!(matches!(
        monitor.subscribe(EventMask::BUTTONS | EventMask::KEYS),
        Err(InputError::Unsupported(_))
    ));
    assert!(matches!(
        monitor.subscribe(EventMask::NONE),
        Err(InputError::Unsupported(_))
    ));
    assert!(script.calls().is_empty());
}

#[test]
fn classes_the_backend_lacks_are_refused() {
    struct ButtonsOnly;
    impl InputBackend for ButtonsOnly {
        fn name(&self) -> &'static str {
            "buttons-only"
        }
        fn capabilities(&self) -> CapabilitySet {
            CapabilitySet::new().with(caps::BUTTONS, CapabilityValue::Flag(true))
        }
        fn start(
            &self,
            _mask: EventMask,
            _sink: InputSink,
        ) -> Result<Box<dyn RunningInput>, InputError> {
            Err(InputError::Backend { code: 1 })
        }
    }
    let monitor = InputMonitor::new(Box::new(ButtonsOnly));
    assert!(matches!(
        monitor.subscribe(EventMask::POINTER),
        Err(InputError::Unsupported(_))
    ));
}

#[test]
fn busy_after_the_last_slot() {
    let (monitor, _script) = monitor();
    let mut open = Vec::new();
    for _ in 0..MAX_SUBSCRIPTIONS {
        open.push(monitor.subscribe(EventMask::BUTTONS).expect("slot"));
    }
    assert!(matches!(
        monitor.subscribe(EventMask::BUTTONS),
        Err(InputError::Busy)
    ));
    // A slot whose reader still lives is not reused.
    let (sub, reader) = open.pop().expect("one");
    drop(sub);
    assert!(matches!(
        monitor.subscribe(EventMask::BUTTONS),
        Err(InputError::Busy)
    ));
    drop(reader);
    assert!(monitor.subscribe(EventMask::BUTTONS).is_ok());
}

#[test]
fn a_new_subscription_does_not_see_old_events() {
    let (monitor, script) = monitor();
    let (_keep, _keep_reader) = monitor.subscribe(EventMask::POINTER).expect("keep");
    let (a, ra) = monitor.subscribe(EventMask::BUTTONS).expect("a");
    script.click(1, Button::Left);
    drop(a);
    drop(ra);
    let (_b, mut rb) = monitor.subscribe(EventMask::BUTTONS).expect("b");
    assert_eq!(rb.next_event(), None);
    script.click(2, Button::Left);
    assert_eq!(rb.next_event().map(|e| e.time), Some(ts(2)));
}

#[test]
fn start_failure_leaves_nothing_running() {
    let (monitor, script) = monitor();
    script.lock().fail_start = true;
    assert_eq!(
        monitor.subscribe(EventMask::BUTTONS).err(),
        Some(InputError::Backend { code: 5 })
    );
    assert!(!monitor.is_running());
    script.lock().fail_start = false;
    assert!(monitor.subscribe(EventMask::BUTTONS).is_ok());
}

#[test]
fn reader_calls_do_not_allocate() {
    let (monitor, script) = monitor();
    let (_s, mut reader) = monitor.subscribe(EventMask::MOUSE).expect("subscribe");
    for ms in 0..50 {
        script.click(ms, Button::Middle);
        script.moved(ms, ms as f64);
        let info = allocation_counter::measure(|| {
            std::hint::black_box(reader.next_event());
            std::hint::black_box(reader.pointer_at(ts(ms)));
            std::hint::black_box(reader.latest_pointer());
        });
        assert_eq!(info.count_total, 0, "read {ms} allocated");
    }
}
