//! Copies in flight on the helper's GPU (ADR-0167 decisions 4 and 5).
//!
//! On macOS the helper records a copy into a claimed slot and returns from CEF's
//! callback; Metal finishes it later and calls a completed handler on its own thread.
//! The handler only marks the slot done in [`CopyDone`]. Every claim, publish and
//! abandon stays on CEF's UI thread ([`CopyQueue`]), so the slot protocol of
//! `gpu_slots` keeps its single writer, and pictures are published in claim order.
//! This is helper memory, not the shared section.

// Under `--cfg loom` only the model tests call this module.
#![cfg_attr(all(loom, test), allow(dead_code))]

#[cfg(all(loom, test))]
use loom::sync::atomic::AtomicU32;
#[cfg(not(all(loom, test)))]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use crate::MAX_GPU_SLOTS;
use crate::gpu_slots::{self, Words};

const IDLE: u32 = 0;
const PENDING: u32 = 1;
const OK: u32 = 2;
const FAILED: u32 = 3;

/// One word per slot that a completed handler sets. Shared with the handlers (any
/// thread); never waits, never allocates.
#[derive(Debug)]
pub struct CopyDone {
    words: [AtomicU32; MAX_GPU_SLOTS as usize],
}

impl Default for CopyDone {
    fn default() -> Self {
        Self::new()
    }
}

impl CopyDone {
    /// Every slot idle.
    pub fn new() -> Self {
        Self {
            words: std::array::from_fn(|_| AtomicU32::new(IDLE)),
        }
    }

    /// The copy into `slot` finished, `ok` or with an error. Any thread; called once
    /// per recorded copy (the completed handler).
    pub fn complete(&self, slot: u32, ok: bool) {
        if let Some(word) = self.words.get(slot as usize) {
            word.store(if ok { OK } else { FAILED }, Ordering::Release);
        }
    }

    fn word(&self, slot: u32) -> Option<&AtomicU32> {
        self.words.get(slot as usize)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    slot: u32,
    seq: u64,
    painted: u64,
    committed: u64,
}

/// What one [`CopyQueue::collect`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Collected {
    /// Pictures published, in claim order.
    pub published: u32,
    /// Copies that completed with an error; their slots were abandoned.
    pub failed: u32,
    /// The oldest copy has not completed within the limit (fallback `copy`).
    pub timed_out: bool,
}

/// The helper's copies in claim order. CEF's UI thread only.
#[derive(Debug)]
pub struct CopyQueue {
    entries: [Entry; MAX_GPU_SLOTS as usize],
    head: usize,
    len: usize,
    next_seq: u64,
}

impl Default for CopyQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl CopyQueue {
    /// No copy in flight; the first claim gets sequence 1.
    pub fn new() -> Self {
        Self {
            entries: [Entry::default(); MAX_GPU_SLOTS as usize],
            head: 0,
            len: 0,
            next_seq: 1,
        }
    }

    /// Copies recorded and not collected yet.
    pub fn in_flight(&self) -> usize {
        self.len
    }

    pub(crate) fn claim_in(
        &mut self,
        w: &impl Words,
        done: &CopyDone,
        painted: u64,
        now: u64,
    ) -> Option<(u32, u64)> {
        if self.len == self.entries.len() {
            return None;
        }
        let slot = gpu_slots::claim(w)?;
        let Some(word) = done.word(slot) else {
            gpu_slots::abandon(w, slot);
            return None;
        };
        word.store(PENDING, Ordering::Relaxed);
        let seq = self.next_seq;
        self.next_seq += 1;
        let at = (self.head + self.len) % self.entries.len();
        self.entries[at] = Entry {
            slot,
            seq,
            painted,
            committed: now,
        };
        self.len += 1;
        Some((slot, seq))
    }

    pub(crate) fn cancel_last_in(&mut self, w: &impl Words, done: &CopyDone) {
        if self.len == 0 {
            return;
        }
        self.len -= 1;
        let at = (self.head + self.len) % self.entries.len();
        let slot = self.entries[at].slot;
        if let Some(word) = done.word(slot) {
            word.store(IDLE, Ordering::Relaxed);
        }
        gpu_slots::abandon(w, slot);
    }

    pub(crate) fn collect_in(
        &mut self,
        w: &impl Words,
        done: &CopyDone,
        now: u64,
        limit: u64,
    ) -> Collected {
        let mut out = Collected::default();
        while self.len > 0 {
            let entry = self.entries[self.head];
            let Some(word) = done.word(entry.slot) else {
                break;
            };
            match word.load(Ordering::Acquire) {
                OK => {
                    gpu_slots::publish(w, entry.slot, entry.seq, entry.painted);
                    out.published += 1;
                }
                FAILED => {
                    gpu_slots::abandon(w, entry.slot);
                    out.failed += 1;
                }
                _ => {
                    out.timed_out = now.saturating_sub(entry.committed) > limit;
                    break;
                }
            }
            word.store(IDLE, Ordering::Relaxed);
            self.head = (self.head + 1) % self.entries.len();
            self.len -= 1;
        }
        out
    }
}

#[cfg(not(all(loom, test)))]
impl crate::FrameRing {
    /// Helper side: claims a GPU slot for a copy painted at `painted` and recorded at
    /// `now` (both OS monotonic nanoseconds). Returns the slot and its claim sequence,
    /// which the publish writes as the slot's fence word. `None` when no slot is free
    /// or `queue` is full. CEF's UI thread only; never waits.
    pub fn claim_copy(
        &self,
        queue: &mut CopyQueue,
        done: &CopyDone,
        painted: u64,
        now: u64,
    ) -> Option<(u32, u64)> {
        queue.claim_in(self, done, painted, now)
    }

    /// Helper side: the newest claim was never committed (recording failed): gives its
    /// slot up.
    pub fn cancel_copy(&self, queue: &mut CopyQueue, done: &CopyDone) {
        queue.cancel_last_in(self, done);
    }

    /// Helper side: publishes the completed copies in claim order and abandons the
    /// failed ones, stopping at the first still in flight; that one counts as timed out
    /// when it was recorded more than `limit` nanoseconds before `now`. CEF's UI thread
    /// only; never waits.
    pub fn collect_copies(
        &self,
        queue: &mut CopyQueue,
        done: &CopyDone,
        now: u64,
        limit: u64,
    ) -> Collected {
        queue.collect_in(self, done, now, limit)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::{FrameRing, SlotLayout};

    fn ring(slots: u32) -> FrameRing {
        FrameRing::create_with_gpu(16, 16, SlotLayout::Bgra8Premultiplied, slots).unwrap()
    }

    #[test]
    fn completions_out_of_order_publish_in_claim_order() {
        let ring = ring(4);
        let (mut q, done) = (CopyQueue::new(), CopyDone::new());
        let (a, sa) = ring.claim_copy(&mut q, &done, 10, 10).unwrap();
        let (b, sb) = ring.claim_copy(&mut q, &done, 20, 20).unwrap();
        assert_eq!((sa, sb), (1, 2));
        // The second copy finishes first: nothing is published until the first does.
        done.complete(b, true);
        assert_eq!(ring.collect_copies(&mut q, &done, 30, 1_000).published, 0);
        assert_eq!(ring.lease_latest_gpu(), None);
        done.complete(a, true);
        let c = ring.collect_copies(&mut q, &done, 40, 1_000);
        assert_eq!((c.published, c.failed, c.timed_out), (2, 0, false));
        let p = ring.lease_latest_gpu().unwrap();
        assert_eq!((p.slot, p.fence, p.painted), (b, 2, 20));
        ring.lease_sub(p.slot);
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    fn a_failed_copy_is_abandoned_and_counted() {
        let ring = ring(4);
        let (mut q, done) = (CopyQueue::new(), CopyDone::new());
        let (a, _) = ring.claim_copy(&mut q, &done, 1, 1).unwrap();
        done.complete(a, false);
        let c = ring.collect_copies(&mut q, &done, 2, 1_000);
        assert_eq!((c.published, c.failed), (0, 1));
        assert_eq!(ring.lease_latest_gpu(), None);
        // The slot is free again.
        assert_eq!(ring.lease(a), 0);
        assert!(ring.claim_copy(&mut q, &done, 3, 3).is_some());
    }

    #[test]
    fn a_copy_that_never_completes_times_out() {
        let ring = ring(4);
        let (mut q, done) = (CopyQueue::new(), CopyDone::new());
        ring.claim_copy(&mut q, &done, 0, 100).unwrap();
        assert!(!ring.collect_copies(&mut q, &done, 1_100, 1_000).timed_out);
        assert!(ring.collect_copies(&mut q, &done, 1_101, 1_000).timed_out);
    }

    #[test]
    fn a_cancelled_claim_frees_its_slot() {
        let ring = ring(1);
        let (mut q, done) = (CopyQueue::new(), CopyDone::new());
        ring.claim_copy(&mut q, &done, 0, 0).unwrap();
        assert!(ring.claim_copy(&mut q, &done, 0, 0).is_none(), "one slot");
        ring.cancel_copy(&mut q, &done);
        assert_eq!(q.in_flight(), 0);
        assert!(ring.claim_copy(&mut q, &done, 0, 0).is_some());
    }

    #[test]
    fn the_latest_published_slot_is_not_claimed_while_copies_fly() {
        let ring = ring(2);
        let (mut q, done) = (CopyQueue::new(), CopyDone::new());
        let (a, _) = ring.claim_copy(&mut q, &done, 0, 0).unwrap();
        done.complete(a, true);
        ring.collect_copies(&mut q, &done, 0, 1);
        let (b, _) = ring.claim_copy(&mut q, &done, 0, 0).unwrap();
        assert_ne!(a, b);
        // Two slots: the latest (a) is kept, b is in flight, nothing else is free.
        assert!(ring.claim_copy(&mut q, &done, 0, 0).is_none());
    }
}

/// The claim on one thread with completions on others, model-checked (ADR-0167
/// decision 10). Run with
/// `RUSTFLAGS="--cfg loom" cargo test -p rbs-browser-shm --release --lib loom`.
#[cfg(all(test, loom))]
mod loom_tests {
    #![allow(clippy::unwrap_used)]

    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::thread;

    use super::*;
    use crate::gpu_slots::{lease, lease_latest, lease_sub, set_slots};

    struct Model {
        words: Vec<loom::sync::atomic::AtomicU32>,
        dwords: Vec<AtomicU64>,
    }

    impl Words for Model {
        fn word(&self, offset: usize) -> &loom::sync::atomic::AtomicU32 {
            &self.words[offset / 4]
        }
        fn dword(&self, offset: usize) -> &AtomicU64 {
            &self.dwords[offset / 8]
        }
    }

    fn model_words(slots: u32) -> Arc<Model> {
        let m = Model {
            words: (0..crate::HEADER / 4)
                .map(|_| loom::sync::atomic::AtomicU32::new(0))
                .collect(),
            dwords: (0..crate::HEADER / 8).map(|_| AtomicU64::new(0)).collect(),
        };
        set_slots(&m, slots);
        Arc::new(m)
    }

    /// Two copies complete on two GPU threads in either order while the studio
    /// leases. The UI thread publishes them in claim order; a leased slot holds the
    /// picture of its own publish (the GPU wrote it before completing), and the slot
    /// published last is never claimed again while copies fly.
    /// Two copies complete on two GPU threads in either order, racing the UI
    /// thread's collect. The UI thread publishes them in claim order, each published
    /// slot holds its own copy (the GPU wrote it before completing), and the slot
    /// published last is not claimed again.
    #[test]
    fn loom_completions_on_other_threads_keep_one_writer_and_claim_order() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let m = model_words(3);
            let done = Arc::new(CopyDone::new());
            let marks: Arc<Vec<AtomicU64>> = Arc::new((0..3).map(|_| AtomicU64::new(0)).collect());
            let mut q = CopyQueue::new();
            let (a, sa) = q.claim_in(&*m, &done, 1, 0).unwrap();
            let (b, sb) = q.claim_in(&*m, &done, 2, 0).unwrap();
            let gpu = |slot: u32, seq: u64| {
                let (done, marks) = (Arc::clone(&done), Arc::clone(&marks));
                thread::spawn(move || {
                    marks[slot as usize].store(seq, Ordering::Relaxed);
                    done.complete(slot, true);
                })
            };
            let ga = gpu(a, sa);
            let gb = gpu(b, sb);
            let mut seen = Vec::new();
            let mut take = |q: &mut CopyQueue| {
                q.collect_in(&*m, &done, 0, u64::MAX);
                if let Some(p) = lease_latest(&*m, None) {
                    assert_eq!(marks[p.slot as usize].load(Ordering::Relaxed), p.fence);
                    seen.push(p.fence);
                    lease_sub(&*m, p.slot);
                }
            };
            take(&mut q);
            ga.join().unwrap();
            gb.join().unwrap();
            take(&mut q);
            assert!(
                seen.windows(2).all(|w| w[0] <= w[1]),
                "out of order: {seen:?}"
            );
            assert_eq!(seen.last(), Some(&sb));
            let next = q.claim_in(&*m, &done, 3, 0);
            assert_ne!(next.map(|(s, _)| s), Some(b), "claimed the latest slot");
            for slot in 0..3 {
                assert_eq!(lease(&*m, slot), 0);
            }
        });
    }
}
