//! The three-slot hand-off between the render thread and an adapter's sender thread
//! (ADR-0066 decision 3; spec `docs/specs/virtual-camera.md`, "Session").
//!
//! The producer reserves an empty slot before it produces a value
//! ([`Mailbox::reserve`]), fills it later ([`Reservation::fill`]) and never drops a
//! value it handed over: the consumer takes the newest full slot and drops every older
//! full one itself ([`Mailbox::take_newest`]). Nothing waits and nothing allocates.
//!
//! This is a marked low-level module (CLAUDE.md rule 7): each slot's value lives in an
//! `UnsafeCell` whose access is guarded by the slot's state word; every `unsafe` block
//! states the invariant.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Slots in a mailbox.
pub const SLOTS: usize = 3;

const EMPTY: u64 = 0;
const RESERVED: u64 = 1;
const FULL: u64 = 2;
const TAKEN: u64 = 3;
const MASK: u64 = 0b11;

struct Slot<T> {
    /// `seq << 2 | tag`. Who may touch `value`: the reservation holder while `RESERVED`,
    /// the consumer while `TAKEN`, nobody while `EMPTY` (it is `None`) or `FULL` (it is
    /// `Some`, waiting for the consumer).
    state: AtomicU64,
    value: UnsafeCell<Option<T>>,
}

/// A fixed set of slots shared by one producer and one consumer.
pub struct Mailbox<T> {
    slots: [Slot<T>; SLOTS],
    next_seq: AtomicU64,
}

// SAFETY: a value moves from the producer thread to the consumer thread, so `T: Send` is
// needed and enough; the state words make sure only one thread touches a slot's value at
// a time (see `Slot::state`), so sharing `&Mailbox` between threads is sound.
unsafe impl<T: Send> Sync for Mailbox<T> {}
// SAFETY: as above; moving the mailbox moves its values, which are `Send`.
unsafe impl<T: Send> Send for Mailbox<T> {}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot {
                state: AtomicU64::new(EMPTY),
                value: UnsafeCell::new(None),
            }),
            next_seq: AtomicU64::new(1),
        }
    }
}

impl<T> std::fmt::Debug for Mailbox<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let states: [u64; SLOTS] =
            std::array::from_fn(|i| self.slots[i].state.load(Ordering::Relaxed) & MASK);
        f.debug_struct("Mailbox").field("states", &states).finish()
    }
}

impl<T> Mailbox<T> {
    /// An empty mailbox.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reserves an empty slot (compare-and-swap `EMPTY -> RESERVED`). `None` when every
    /// slot is reserved, full or being taken. One atomic load per slot and at most one
    /// compare-and-swap; no allocation (the `Arc` clone is a counter increment).
    pub fn reserve(self: &Arc<Self>) -> Option<Reservation<T>> {
        for (index, slot) in self.slots.iter().enumerate() {
            let w = slot.state.load(Ordering::Acquire);
            if w & MASK == EMPTY
                && slot
                    .state
                    .compare_exchange(w, RESERVED, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                return Some(Reservation {
                    mailbox: Arc::clone(self),
                    index,
                    filled: false,
                });
            }
        }
        None
    }

    /// True when a slot is empty (a hint: a reservation may still fail).
    pub fn has_room(&self) -> bool {
        self.slots
            .iter()
            .any(|s| s.state.load(Ordering::Acquire) & MASK == EMPTY)
    }

    /// Takes the newest full slot's value and empties every older full slot, handing each
    /// dropped value to `replaced` (which typically just drops it, on this thread).
    /// Call from one consumer thread only.
    pub fn take_newest(&self, mut replaced: impl FnMut(T)) -> Option<T> {
        let mut newest: Option<(u64, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let w = slot.state.load(Ordering::Acquire);
            if w & MASK == FULL && newest.is_none_or(|(seq, _)| w >> 2 > seq) {
                newest = Some((w >> 2, index));
            }
        }
        let (newest_seq, _) = newest?;
        let mut out = None;
        for slot in &self.slots {
            let w = slot.state.load(Ordering::Acquire);
            if w & MASK != FULL || w >> 2 > newest_seq {
                continue;
            }
            if slot
                .state
                .compare_exchange(w, w & !MASK | TAKEN, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // SAFETY: the slot is `TAKEN` by this (the only) consumer; the producer's
            // `fill` finished writing before its `Release` store of `FULL`, which our
            // `AcqRel` compare-and-swap observed.
            let value = unsafe { (*slot.value.get()).take() };
            slot.state.store(EMPTY, Ordering::Release);
            if let Some(v) = value {
                if w >> 2 == newest_seq {
                    out = Some(v);
                } else {
                    replaced(v);
                }
            }
        }
        out
    }
}

impl<T> Drop for Mailbox<T> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.value.get_mut().take();
        }
    }
}

/// A reserved slot. [`Reservation::fill`] hands a value over; dropping it unfilled
/// returns the slot.
pub struct Reservation<T> {
    mailbox: Arc<Mailbox<T>>,
    index: usize,
    filled: bool,
}

impl<T> std::fmt::Debug for Reservation<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("index", &self.index)
            .finish()
    }
}

impl<T> Reservation<T> {
    /// True when this reservation belongs to `mailbox`.
    pub fn is_for(&self, mailbox: &Arc<Mailbox<T>>) -> bool {
        Arc::ptr_eq(&self.mailbox, mailbox)
    }

    /// Hands `value` over and returns its sequence number.
    pub fn fill(mut self, value: T) -> u64 {
        let slot = &self.mailbox.slots[self.index];
        // SAFETY: the slot is `RESERVED` by this reservation, which only this thread
        // holds; nobody else touches `value` until the `Release` store of `FULL` below.
        unsafe { *slot.value.get() = Some(value) };
        let seq = self.mailbox.next_seq.fetch_add(1, Ordering::Relaxed);
        slot.state.store((seq << 2) | FULL, Ordering::Release);
        self.filled = true;
        seq
    }
}

impl<T> Drop for Reservation<T> {
    fn drop(&mut self) {
        if !self.filled {
            self.mailbox.slots[self.index]
                .state
                .store(EMPTY, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn three_reservations_then_none() {
        let m = Mailbox::<u32>::new();
        let r: Vec<_> = (0..SLOTS).map(|_| m.reserve().unwrap()).collect();
        assert!(m.reserve().is_none());
        assert!(!m.has_room());
        drop(r);
        assert!(m.reserve().is_some());
    }

    #[test]
    fn the_newest_is_taken_and_older_ones_are_replaced_by_the_consumer() {
        let m = Mailbox::<u32>::new();
        for v in 1..=3 {
            m.reserve().unwrap().fill(v);
        }
        let mut replaced = vec![];
        assert_eq!(m.take_newest(|v| replaced.push(v)), Some(3));
        replaced.sort();
        assert_eq!(replaced, vec![1, 2]);
        assert_eq!(m.take_newest(|_| panic!()), None);
        assert!(m.has_room());
    }

    #[test]
    fn a_reservation_filled_out_of_order_keeps_sequence_order() {
        let m = Mailbox::<u32>::new();
        let a = m.reserve().unwrap();
        let b = m.reserve().unwrap();
        b.fill(20);
        a.fill(10); // filled later: newer
        let mut replaced = vec![];
        assert_eq!(m.take_newest(|v| replaced.push(v)), Some(10));
        assert_eq!(replaced, vec![20]);
    }

    #[test]
    fn a_reserved_slot_is_not_taken() {
        let m = Mailbox::<u32>::new();
        let held = m.reserve().unwrap();
        m.reserve().unwrap().fill(1);
        assert_eq!(m.take_newest(|_| {}), Some(1));
        assert_eq!(held.fill(2), 2);
        assert_eq!(m.take_newest(|_| {}), Some(2));
    }

    #[test]
    fn values_left_in_the_mailbox_are_dropped_with_it() {
        let marker = Arc::new(());
        let m = Mailbox::<Arc<()>>::new();
        m.reserve().unwrap().fill(Arc::clone(&marker));
        drop(m);
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    #[test]
    fn producer_and_consumer_threads_lose_nothing() {
        let m = Mailbox::<u64>::new();
        let consumer = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                let (mut taken, mut replaced, mut last) = (0u64, 0u64, 0u64);
                // Bounded: a hang guard.
                for _ in 0..10_000_000u64 {
                    if let Some(v) = m.take_newest(|_| replaced += 1) {
                        assert!(v > last);
                        last = v;
                        taken += 1;
                    }
                    if last == 10_000 {
                        break;
                    }
                }
                (taken, replaced)
            })
        };
        let mut sent = 0u64;
        let mut v = 1u64;
        while v <= 10_000 {
            if let Some(r) = m.reserve() {
                r.fill(v);
                v += 1;
                sent += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        let (taken, replaced) = consumer.join().unwrap();
        assert_eq!(taken + replaced, sent);
    }
}
