//! Lock-free hand-off of captured surfaces from the capture callback thread to the
//! render thread.
//!
//! Platform independent so it is tested and benchmarked on every CI runner. The ring
//! holds a fixed number of slots; each slot names one shared surface by its handle
//! value. The producer may replace a slot's surface (after a resize) only while it has
//! the slot claimed, so consumers never see a handle change under them. A slot's reference count says who uses it:
//!
//! - `0`: free; the producer may claim it and write into its surface.
//! - `1` while claimed by the producer, then owned by `latest` once published.
//! - `n`: owned by consumers through leases (the importer releases the lease after its
//!   GPU read completes).
//!
//! `latest` holds the newest published slot (index + 1, or 0 for none). Publishing a
//! slot over an unconsumed one releases the older slot and counts it as dropped.
//! Taking the latest slot moves its reference to the consumer. No call allocates,
//! frees, locks or blocks.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Metadata of one published frame, written by the producer before publishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameMeta {
    /// Master-clock timestamp in nanoseconds.
    pub timestamp: u64,
    /// Sequence number: frames received from the OS.
    pub sequence: u64,
    /// The OS timestamp in nanoseconds, for diagnostics.
    pub source_time: u64,
    /// Fence value signalled after the copy into the slot's surface.
    pub fence_value: u64,
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
}

#[derive(Default)]
struct Slot {
    refs: AtomicU32,
    handle: AtomicU64,
    /// Whether a consumer took a frame of the current handle (it may be cached by
    /// value somewhere).
    handed_out: AtomicBool,
    timestamp: AtomicU64,
    sequence: AtomicU64,
    source_time: AtomicU64,
    fence_value: AtomicU64,
    size: AtomicU64,
}

/// The ring of slots shared by the producer, the session and every lease.
pub struct Ring {
    slots: Box<[Slot]>,
    latest: AtomicU64,
    dropped: AtomicU64,
}

impl Ring {
    /// A ring over the given surface handles, one slot per handle.
    pub fn new(handles: &[u64]) -> Self {
        Self {
            slots: handles
                .iter()
                .map(|h| Slot {
                    handle: AtomicU64::new(*h),
                    ..Slot::default()
                })
                .collect(),
            latest: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Number of slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True for a ring without slots.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The surface handle of `slot`.
    pub fn handle(&self, slot: usize) -> Option<u64> {
        self.slots
            .get(slot)
            .map(|s| s.handle.load(Ordering::Acquire))
    }

    /// Producer: replaces the surface of a slot it has claimed (after a resize).
    pub fn set_handle(&self, slot: usize, handle: u64) {
        if let Some(s) = self.slots.get(slot) {
            s.handed_out.store(false, Ordering::Relaxed);
            s.handle.store(handle, Ordering::Release);
        }
    }

    /// Frames replaced before delivery or not delivered because every slot was held.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Producer: counts a frame that could not be written because no slot was free.
    pub fn count_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Producer: claims a free slot for writing, or `None` when consumers hold all.
    pub fn claim(&self) -> Option<usize> {
        self.slots.iter().position(|s| {
            s.refs
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        })
    }

    /// Producer: gives up a claimed slot without publishing it.
    pub fn abandon(&self, slot: usize) {
        self.release(slot as u64);
    }

    /// Producer: publishes a claimed slot as the newest frame. Its reference moves to
    /// `latest`. An older slot still waiting in `latest` is released and counted as
    /// dropped.
    pub fn publish(&self, slot: usize, meta: FrameMeta) {
        let Some(s) = self.slots.get(slot) else {
            return;
        };
        s.timestamp.store(meta.timestamp, Ordering::Relaxed);
        s.sequence.store(meta.sequence, Ordering::Relaxed);
        s.source_time.store(meta.source_time, Ordering::Relaxed);
        s.fence_value.store(meta.fence_value, Ordering::Relaxed);
        s.size.store(
            (u64::from(meta.width) << 32) | u64::from(meta.height),
            Ordering::Relaxed,
        );
        // Release: the metadata stores above happen before a consumer's Acquire swap.
        let previous = self.latest.swap(slot as u64 + 1, Ordering::AcqRel);
        if previous != 0 {
            self.release(previous - 1);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Consumer: takes the newest published slot and its metadata. The caller now owns
    /// one reference to the slot and must release it (through a lease).
    pub fn take(&self) -> Option<(usize, FrameMeta)> {
        let taken = self.latest.swap(0, Ordering::AcqRel);
        if taken == 0 {
            return None;
        }
        let slot = (taken - 1) as usize;
        let s = self.slots.get(slot)?;
        // Ordered before the producer's next claim of this slot by the lease's Release
        // decrement and the claim's Acquire.
        s.handed_out.store(true, Ordering::Relaxed);
        let size = s.size.load(Ordering::Relaxed);
        Some((
            slot,
            FrameMeta {
                timestamp: s.timestamp.load(Ordering::Relaxed),
                sequence: s.sequence.load(Ordering::Relaxed),
                source_time: s.source_time.load(Ordering::Relaxed),
                fence_value: s.fence_value.load(Ordering::Relaxed),
                width: (size >> 32) as u32,
                height: size as u32,
            },
        ))
    }

    /// Drops a frame still waiting in `latest` (used when stopping). Returns whether
    /// one was waiting; it is not counted in [`Ring::dropped`].
    pub fn clear(&self) -> bool {
        let previous = self.latest.swap(0, Ordering::AcqRel);
        if previous != 0 {
            self.release(previous - 1);
        }
        previous != 0
    }

    /// Adds a reference to `slot` (a lease was cloned). Unknown slots are ignored.
    pub fn retain(&self, slot: u64) {
        if let Some(s) = self.slots.get(slot as usize) {
            s.refs.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Removes a reference from `slot`. Unknown slots are ignored.
    ///
    /// The ring only counts; it owns no surfaces. The adapter's lease owner wraps it
    /// together with the surfaces and is the one that makes the `LeaseOwner` promises.
    pub fn release(&self, slot: u64) {
        if let Some(s) = self.slots.get(slot as usize) {
            // Release pairs with the Acquire in `claim`: the consumer's GPU-side reads
            // are ordered by the importer before it drops the lease.
            let previous = s.refs.fetch_sub(1, Ordering::Release);
            debug_assert!(previous > 0, "released slot {slot} with no reference");
        }
    }

    /// Whether a frame of the slot's current handle was ever taken by a consumer. A
    /// surface never handed out can be closed at once; its handle value never left the
    /// adapter.
    pub fn handed_out(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|s| s.handed_out.load(Ordering::Relaxed))
    }

    /// Whether nobody references `slot`.
    pub fn is_free(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|s| s.refs.load(Ordering::Acquire) == 0)
    }

    /// True when no slot is referenced by anyone.
    pub fn is_idle(&self) -> bool {
        self.slots
            .iter()
            .all(|s| s.refs.load(Ordering::Acquire) == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn some<T>(value: Option<T>) -> T {
        match value {
            Some(v) => v,
            None => panic!("expected a value"),
        }
    }

    fn meta(sequence: u64) -> FrameMeta {
        FrameMeta {
            timestamp: sequence * 10,
            sequence,
            source_time: sequence * 7,
            fence_value: sequence + 1,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn newest_wins_and_replaced_frames_are_dropped() {
        let ring = Ring::new(&[11, 12, 13]);
        for seq in 0..3 {
            let slot = some(ring.claim());
            ring.publish(slot, meta(seq));
        }
        assert_eq!(ring.dropped(), 2);
        let (slot, m) = some(ring.take());
        assert_eq!(m, meta(2));
        assert!(ring.handle(slot).is_some());
        assert!(ring.take().is_none());
        assert!(ring.handed_out(slot));
        ring.release(slot as u64);
        assert!(ring.is_idle());
        assert!(ring.is_free(slot));
        // Replaced frames were never handed out.
        assert_eq!((0..3).filter(|s| ring.handed_out(*s)).count(), 1);
        ring.set_handle(slot, 99);
        assert!(!ring.handed_out(slot));
    }

    #[test]
    fn held_slots_are_not_reused() {
        let ring = Ring::new(&[1, 2]);
        let a = some(ring.claim());
        ring.publish(a, meta(0));
        let (held, _) = some(ring.take());
        let b = some(ring.claim());
        assert_ne!(b, held);
        ring.publish(b, meta(1));
        // Both slots are referenced now: one by the consumer, one by `latest`.
        assert!(ring.claim().is_none());
        ring.release(held as u64);
        assert!(ring.claim().is_some());
    }

    #[test]
    fn clear_releases_the_waiting_frame() {
        let ring = Ring::new(&[1]);
        let a = some(ring.claim());
        ring.publish(a, meta(0));
        assert!(ring.clear());
        assert!(!ring.clear());
        assert!(ring.is_idle());
    }

    #[test]
    fn concurrent_producer_and_consumer_keep_counts_exact() {
        let ring = Arc::new(Ring::new(&[1, 2, 3, 4]));
        let producer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut produced = 0u64;
                let mut skipped = 0u64;
                for seq in 0..20_000 {
                    match ring.claim() {
                        Some(slot) => {
                            ring.publish(slot, meta(seq));
                            produced += 1;
                        }
                        None => {
                            ring.count_drop();
                            skipped += 1;
                        }
                    }
                }
                (produced, skipped)
            })
        };
        let mut taken = 0u64;
        let mut last = None;
        let mut held = Vec::with_capacity(4);
        while !producer.is_finished() || taken == 0 {
            if let Some((slot, m)) = ring.take() {
                assert!(
                    last.is_none_or(|l| m.sequence > l),
                    "sequence must increase"
                );
                assert_eq!(m.timestamp, m.sequence * 10, "metadata is consistent");
                last = Some(m.sequence);
                taken += 1;
                held.push(slot);
                if held.len() > 2 {
                    ring.release(held.remove(0) as u64);
                }
            }
        }
        let Ok((produced, skipped)) = producer.join() else {
            panic!("producer panicked")
        };
        if let Some((slot, _)) = ring.take() {
            taken += 1;
            held.push(slot);
        }
        for slot in held {
            ring.release(slot as u64);
        }
        assert!(ring.is_idle());
        assert_eq!(produced + skipped, 20_000);
        assert_eq!(
            taken + ring.dropped(),
            20_000,
            "every frame delivered or dropped"
        );
    }
}
