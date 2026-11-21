//! Lock-free delivery: the per-subscription event ring and the shared pointer history
//! (`docs/specs/input-port.md`, invariants 4 and 5).
//!
//! Both store their payload in atomics, so no `unsafe` is needed: the ring is
//! single-producer single-consumer with acquire/release indices, and the history is a
//! sequence-locked overwrite buffer whose reader retries a bounded number of times.

use std::sync::atomic::{AtomicU64, Ordering, fence};

use rbs_core::time::Timestamp;

use crate::event::{Button, DesktopPoint, EventMask, InputEvent, PointerSample, TimedEvent};

/// Events each subscription's ring holds.
pub const EVENT_RING: usize = 256;
/// Pointer samples the history keeps (about 64 ms at 1000 Hz).
pub const POINTER_HISTORY: usize = 64;
/// Attempts a reader makes on one history slot before it skips it.
const READ_ATTEMPTS: usize = 4;

const KIND_DOWN: u64 = 1;
const KIND_UP: u64 = 2;
const KIND_WHEEL: u64 = 3;

#[derive(Default)]
struct EventSlot {
    time: AtomicU64,
    x: AtomicU64,
    y: AtomicU64,
    tag: AtomicU64,
    wheel: AtomicU64,
}

/// Single-producer single-consumer ring of timestamped events.
pub(crate) struct EventRing {
    slots: Box<[EventSlot]>,
    /// Events written (producer).
    head: AtomicU64,
    /// Events read (consumer).
    tail: AtomicU64,
    dropped: AtomicU64,
}

impl EventRing {
    pub(crate) fn new() -> Self {
        Self {
            slots: (0..EVENT_RING).map(|_| EventSlot::default()).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn slot(&self, index: u64) -> &EventSlot {
        // The modulo keeps the index in range.
        &self.slots[(index % EVENT_RING as u64) as usize]
    }

    /// Producer. `generation` names the subscription the producer saw on this channel;
    /// the reader skips events of any other. Drops the newest event and counts it when
    /// the ring is full.
    pub(crate) fn push(&self, timed: &TimedEvent, generation: u32) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= EVENT_RING as u64 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let (at, tag, wheel) = match timed.event {
            InputEvent::ButtonDown { button, at } => {
                (at, KIND_DOWN | u64::from(button.index()) << 8, 0)
            }
            InputEvent::ButtonUp { button, at } => {
                (at, KIND_UP | u64::from(button.index()) << 8, 0)
            }
            InputEvent::Wheel { at, dx, dy } => (
                at,
                KIND_WHEEL,
                u64::from(dx.to_bits()) << 32 | u64::from(dy.to_bits()),
            ),
        };
        let slot = self.slot(head);
        slot.time.store(timed.time.as_nanos(), Ordering::Relaxed);
        slot.x.store(at.x.to_bits(), Ordering::Relaxed);
        slot.y.store(at.y.to_bits(), Ordering::Relaxed);
        slot.tag
            .store(tag | u64::from(generation) << 32, Ordering::Relaxed);
        slot.wheel.store(wheel, Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Consumer. Returns only events pushed for `generation` whose class is in `mask`,
    /// so a push that read an earlier subscription's mask just before this one took the
    /// channel never reaches this reader (spec invariant 3).
    pub(crate) fn pop(&self, generation: u32, mask: EventMask) -> Option<TimedEvent> {
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if tail == head {
                return None;
            }
            let slot = self.slot(tail);
            let time = Timestamp::from_nanos(slot.time.load(Ordering::Relaxed));
            let at = DesktopPoint::new(
                f64::from_bits(slot.x.load(Ordering::Relaxed)),
                f64::from_bits(slot.y.load(Ordering::Relaxed)),
            );
            let tag = slot.tag.load(Ordering::Relaxed);
            let wheel = slot.wheel.load(Ordering::Relaxed);
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            let button = Button::from_index(((tag >> 8) & 0xff) as u8);
            let event = match (tag & 0xff, button) {
                (KIND_DOWN, Some(button)) => InputEvent::ButtonDown { button, at },
                (KIND_UP, Some(button)) => InputEvent::ButtonUp { button, at },
                (KIND_WHEEL, _) => InputEvent::Wheel {
                    at,
                    dx: f32::from_bits((wheel >> 32) as u32),
                    dy: f32::from_bits(wheel as u32),
                },
                // Not written by `push`; skip rather than invent an event.
                _ => continue,
            };
            if (tag >> 32) as u32 != generation || !mask.contains(event.class()) {
                continue;
            }
            return Some(TimedEvent { time, event });
        }
    }

    /// Consumer, when a subscription takes the ring: forget what an earlier one left.
    pub(crate) fn reset(&self) {
        self.tail
            .store(self.head.load(Ordering::Acquire), Ordering::Release);
        self.dropped.store(0, Ordering::Relaxed);
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct HistorySlot {
    /// `2n + 1` while sample `n` is written, `2n + 2` once it is complete.
    seq: AtomicU64,
    time: AtomicU64,
    x: AtomicU64,
    y: AtomicU64,
}

/// The newest pointer samples, overwritten in place. One writer, any number of readers.
pub(crate) struct PointerHistory {
    slots: Box<[HistorySlot]>,
    /// Samples written.
    count: AtomicU64,
}

impl PointerHistory {
    pub(crate) fn new() -> Self {
        Self {
            slots: (0..POINTER_HISTORY)
                .map(|_| HistorySlot::default())
                .collect(),
            count: AtomicU64::new(0),
        }
    }

    fn slot(&self, index: u64) -> &HistorySlot {
        // The modulo keeps the index in range.
        &self.slots[(index % POINTER_HISTORY as u64) as usize]
    }

    /// Writer (one thread).
    pub(crate) fn write(&self, sample: &PointerSample) {
        let n = self.count.load(Ordering::Relaxed);
        let slot = self.slot(n);
        slot.seq.store(2 * n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        slot.time.store(sample.time.as_nanos(), Ordering::Relaxed);
        slot.x.store(sample.at.x.to_bits(), Ordering::Relaxed);
        slot.y.store(sample.at.y.to_bits(), Ordering::Relaxed);
        slot.seq.store(2 * n + 2, Ordering::Release);
        self.count.store(n + 1, Ordering::Release);
    }

    /// Sample `n`, if the slot still holds it and a consistent read succeeds.
    /// `Err(())` when the slot already holds a newer sample (so older ones are gone too).
    fn read(&self, n: u64) -> Result<Option<PointerSample>, ()> {
        let slot = self.slot(n);
        let done = 2 * n + 2;
        for _ in 0..READ_ATTEMPTS {
            let before = slot.seq.load(Ordering::Acquire);
            if before > done {
                // A newer sample took the slot (or is being written into it): sample `n`
                // and every older one are gone.
                return Err(());
            }
            if before != done {
                // Not written yet as sample `n`; cannot happen for `n < count`.
                return Ok(None);
            }
            let time = slot.time.load(Ordering::Relaxed);
            let x = slot.x.load(Ordering::Relaxed);
            let y = slot.y.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Relaxed) == before {
                return Ok(Some(PointerSample {
                    time: Timestamp::from_nanos(time),
                    at: DesktopPoint::new(f64::from_bits(x), f64::from_bits(y)),
                }));
            }
        }
        Ok(None)
    }

    /// The newest sample at or before `time` (`None` for any time: the newest).
    pub(crate) fn at(&self, time: Option<Timestamp>) -> Option<PointerSample> {
        let count = self.count.load(Ordering::Acquire);
        let oldest = count.saturating_sub(POINTER_HISTORY as u64);
        let mut n = count;
        while n > oldest {
            n -= 1;
            match self.read(n) {
                Err(()) => return None,
                Ok(None) => {}
                Ok(Some(sample)) => {
                    if time.is_none_or(|t| sample.time <= t) {
                        return Some(sample);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use std::sync::atomic::AtomicBool;

    use super::*;

    const GEN: u32 = 7;

    fn ts(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * 1_000_000)
    }

    fn sample(ms: u64) -> PointerSample {
        PointerSample {
            time: ts(ms),
            at: DesktopPoint::new(ms as f64, -(ms as f64)),
        }
    }

    fn click(ms: u64) -> TimedEvent {
        TimedEvent {
            time: ts(ms),
            event: InputEvent::ButtonDown {
                button: Button::Right,
                at: DesktopPoint::new(-5.5, 7.25),
            },
        }
    }

    #[test]
    fn ring_round_trips_every_kind() {
        let ring = EventRing::new();
        let events = [
            click(1),
            TimedEvent {
                time: ts(2),
                event: InputEvent::ButtonUp {
                    button: Button::Forward,
                    at: DesktopPoint::new(1.0, 2.0),
                },
            },
            TimedEvent {
                time: ts(3),
                event: InputEvent::Wheel {
                    at: DesktopPoint::new(3.0, 4.0),
                    dx: -1.5,
                    dy: 2.0,
                },
            },
        ];
        for e in &events {
            ring.push(e, GEN);
        }
        for e in &events {
            assert_eq!(ring.pop(GEN, EventMask::MOUSE).as_ref(), Some(e));
        }
        assert_eq!(ring.pop(GEN, EventMask::MOUSE), None);
    }

    #[test]
    fn full_ring_drops_the_newest_and_counts() {
        let ring = EventRing::new();
        for ms in 0..EVENT_RING as u64 + 3 {
            ring.push(&click(ms), GEN);
        }
        assert_eq!(ring.dropped(), 3);
        assert_eq!(ring.pop(GEN, EventMask::MOUSE).map(|e| e.time), Some(ts(0)));
        let mut last = None;
        while let Some(e) = ring.pop(GEN, EventMask::MOUSE) {
            last = Some(e.time);
        }
        assert_eq!(last, Some(ts(EVENT_RING as u64 - 1)));
    }

    #[test]
    fn events_of_another_generation_or_class_are_skipped() {
        let ring = EventRing::new();
        // A push that read the previous subscription's state just before the reset.
        ring.push(&click(1), GEN - 1);
        ring.push(
            &TimedEvent {
                time: ts(2),
                event: InputEvent::Wheel {
                    at: DesktopPoint::new(0.0, 0.0),
                    dx: 0.0,
                    dy: 1.0,
                },
            },
            GEN,
        );
        ring.push(&click(3), GEN);
        assert_eq!(
            ring.pop(GEN, EventMask::BUTTONS).map(|e| e.time),
            Some(ts(3))
        );
        assert_eq!(ring.pop(GEN, EventMask::BUTTONS), None);
    }

    #[test]
    fn racing_producer_and_consumer_keep_order_and_lose_nothing_counted() {
        let ring = Arc::new(EventRing::new());
        const TOTAL: u64 = 100_000;
        let producer = {
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                for ms in 1..=TOTAL {
                    ring.push(&click(ms), GEN);
                }
            })
        };
        // Hang guard only (ADR-0032).
        let guard = Instant::now() + Duration::from_secs(30);
        let mut last = 0;
        let mut got = 0u64;
        loop {
            assert!(Instant::now() < guard, "producer did not finish");
            // Read before popping: an empty pop after the producer finished means done.
            let finished = producer.is_finished();
            match ring.pop(GEN, EventMask::MOUSE) {
                Some(e) => {
                    let ms = e.time.as_nanos() / 1_000_000;
                    assert!(ms > last, "out of order: {ms} after {last}");
                    assert_eq!(e, click(ms));
                    last = ms;
                    got += 1;
                }
                None if finished => break,
                None => {}
            }
        }
        producer.join().expect("producer thread panicked");
        assert_eq!(got + ring.dropped(), TOTAL);
    }

    #[test]
    fn reset_forgets_old_events() {
        let ring = EventRing::new();
        ring.push(&click(1), GEN);
        ring.reset();
        assert_eq!(ring.pop(GEN, EventMask::MOUSE), None);
        ring.push(&click(2), GEN);
        assert_eq!(ring.pop(GEN, EventMask::MOUSE).map(|e| e.time), Some(ts(2)));
    }

    #[test]
    fn pop_does_not_allocate() {
        let ring = EventRing::new();
        for ms in 0..100 {
            ring.push(&click(ms), GEN);
            let info = allocation_counter::measure(|| {
                std::hint::black_box(ring.pop(GEN, EventMask::MOUSE));
            });
            assert_eq!(info.count_total, 0);
        }
    }

    #[test]
    fn history_finds_the_sample_at_or_before_a_time() {
        let history = PointerHistory::new();
        assert_eq!(history.at(None), None);
        for ms in (10..=100).step_by(10) {
            history.write(&sample(ms));
        }
        assert_eq!(history.at(None), Some(sample(100)));
        assert_eq!(history.at(Some(ts(55))), Some(sample(50)));
        assert_eq!(history.at(Some(ts(50))), Some(sample(50)));
        assert_eq!(history.at(Some(ts(5))), None);
    }

    #[test]
    fn history_keeps_only_the_newest_window() {
        let history = PointerHistory::new();
        let total = POINTER_HISTORY as u64 * 3;
        for ms in 1..=total {
            history.write(&sample(ms));
        }
        let oldest = total - POINTER_HISTORY as u64 + 1;
        assert_eq!(history.at(Some(ts(oldest))), Some(sample(oldest)));
        assert_eq!(history.at(Some(ts(oldest - 1))), None);
        let info = allocation_counter::measure(|| {
            std::hint::black_box(history.at(Some(ts(oldest + 3))));
        });
        assert_eq!(info.count_total, 0);
    }

    #[test]
    fn racing_reader_never_sees_a_torn_sample() {
        // Every sample written has y == -x and time == x ms, so a torn read shows up.
        // The writer runs until the reader has checked enough samples; no timing.
        let history = Arc::new(PointerHistory::new());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let history = Arc::clone(&history);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut ms = 0u64;
                while !stop.load(Ordering::Acquire) {
                    ms += 1;
                    history.write(&sample(ms));
                    if ms.is_multiple_of(64) {
                        // Lets the reader run on a machine with one core.
                        std::thread::yield_now();
                    }
                }
                ms
            })
        };
        // Hang guard only (ADR-0032).
        let guard = Instant::now() + Duration::from_secs(30);
        let mut seen = 0u64;
        let mut newest = 0u64;
        while seen < 2_000 {
            assert!(Instant::now() < guard, "too few consistent reads");
            std::thread::yield_now();
            if let Some(s) = history.at(None) {
                assert_eq!(s.at.y, -s.at.x);
                assert_eq!(s.time, ts(s.at.x as u64));
                if s.at.x as u64 > newest {
                    newest = s.at.x as u64;
                    seen += 1;
                }
            }
        }
        stop.store(true, Ordering::Release);
        let written = writer.join().expect("writer thread panicked");
        assert_eq!(history.at(None), Some(sample(written)));
    }
}
