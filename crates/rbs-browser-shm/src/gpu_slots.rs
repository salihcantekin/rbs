//! The GPU slot protocol of the frame section (ADR-0157 decision 4), over any
//! header words. The section maps them from shared memory; the `loom` tests give
//! model-checked words, so every interleaving of the helper's claim and publish
//! with the studio's lease is tried on this same code.

// Under `--cfg loom` only the model tests call this module.
#![cfg_attr(all(loom, test), allow(dead_code))]

#[cfg(all(loom, test))]
use loom::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::atomic::Ordering;
#[cfg(not(all(loom, test)))]
use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::{GpuPicture, MAX_GPU_SLOTS, WRITING};

/// GPU slot count.
pub(crate) const GPU_COUNT: usize = 128;
/// Slot index of the latest GPU publish.
pub(crate) const GPU_SLOT: usize = 132;
/// How many GPU pictures the helper has published.
pub(crate) const GPU_SEQ: usize = 136;
/// State word of GPU slot 0; the others follow.
pub(crate) const GPU_STATE: usize = 144;
/// Fence value of GPU slot 0; the others follow.
pub(crate) const GPU_FENCE: usize = 176;
/// Paint time of GPU slot 0; the others follow.
pub(crate) const GPU_PAINTED: usize = 240;
/// Frames the helper has published (both paths).
pub(crate) const FRAMES: usize = 32;

/// Header words, 4-byte ([`Words::word`]) and 8-byte ([`Words::dword`]) by offset.
pub(crate) trait Words {
    fn word(&self, offset: usize) -> &AtomicU32;
    fn dword(&self, offset: usize) -> &AtomicU64;
}

pub(crate) fn set_slots(w: &impl Words, count: u32) {
    w.word(GPU_COUNT)
        .store(count.min(MAX_GPU_SLOTS), Ordering::Release);
}

pub(crate) fn slots(w: &impl Words) -> u32 {
    w.word(GPU_COUNT).load(Ordering::Relaxed).min(MAX_GPU_SLOTS)
}

pub(crate) fn state(w: &impl Words, slot: u32) -> &AtomicU32 {
    w.word(GPU_STATE + 4 * slot.min(MAX_GPU_SLOTS - 1) as usize)
}

pub(crate) fn claim(w: &impl Words) -> Option<u32> {
    let count = slots(w);
    let latest = (w.dword(GPU_SEQ).load(Ordering::Acquire) > 0)
        .then(|| w.word(GPU_SLOT).load(Ordering::Relaxed));
    (0..count)
        .filter(|&slot| Some(slot) != latest)
        .find(|&slot| {
            state(w, slot)
                .compare_exchange(0, WRITING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        })
}

pub(crate) fn abandon(w: &impl Words, slot: u32) {
    if slot < slots(w) {
        state(w, slot).fetch_and(!WRITING, Ordering::Release);
    }
}

pub(crate) fn publish(w: &impl Words, slot: u32, fence: u64, painted: u64) {
    if slot >= slots(w) {
        return;
    }
    let index = slot as usize;
    w.dword(GPU_PAINTED + 8 * index)
        .store(painted, Ordering::Relaxed);
    w.dword(GPU_FENCE + 8 * index)
        .store(fence, Ordering::Relaxed);
    w.word(GPU_SLOT).store(slot, Ordering::Relaxed);
    w.dword(GPU_SEQ).fetch_add(1, Ordering::Release);
    state(w, slot).fetch_and(!WRITING, Ordering::Release);
    w.dword(FRAMES).fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn lease_latest(w: &impl Words, seen: Option<u64>) -> Option<GpuPicture> {
    let seq = w.dword(GPU_SEQ).load(Ordering::Acquire);
    if seq == 0 || seen == Some(seq) {
        return None;
    }
    let slot = w.word(GPU_SLOT).load(Ordering::Relaxed);
    if slot >= slots(w) {
        return None;
    }
    let old = state(w, slot).fetch_add(1, Ordering::AcqRel);
    if old & WRITING != 0 {
        state(w, slot).fetch_sub(1, Ordering::Release);
        return None;
    }
    let fence = w
        .dword(GPU_FENCE + 8 * slot as usize)
        .load(Ordering::Relaxed);
    let painted = w
        .dword(GPU_PAINTED + 8 * slot as usize)
        .load(Ordering::Relaxed);
    if w.dword(GPU_SEQ).load(Ordering::Acquire) != seq {
        state(w, slot).fetch_sub(1, Ordering::Release);
        return None;
    }
    Some(GpuPicture {
        slot,
        fence,
        painted,
        seq,
    })
}

pub(crate) fn lease(w: &impl Words, slot: u32) -> u32 {
    state(w, slot).load(Ordering::Acquire) & !WRITING
}

pub(crate) fn lease_add(w: &impl Words, slot: u32) {
    if slot < slots(w) {
        state(w, slot).fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn lease_sub(w: &impl Words, slot: u32) {
    if slot < slots(w) {
        state(w, slot).fetch_sub(1, Ordering::Release);
    }
}

/// Every interleaving of the helper and the studio on this protocol, model-checked
/// with `loom` (ADR-0157 decisions 4 and 11). Run with
/// `RUSTFLAGS="--cfg loom" cargo test -p rbs-browser-shm --release --lib loom`.
#[cfg(all(test, loom))]
mod loom_tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use loom::thread;

    use super::*;

    /// Runs `f` under every interleaving loom finds. Each test runs the studio on a
    /// spawned thread and the helper on the model's own thread: loom 0.7's DPOR keeps
    /// only the last load of a word, so a thread that loads a word and then writes it
    /// (the helper's claim reads the sequence its publish increments) hides an
    /// earlier load by the other thread, and the other order explored only one
    /// iteration (2026-11-21). The studio never writes a word it loaded first.
    fn model(f: impl Fn() + Sync + Send + 'static) {
        loom::model(f);
    }

    /// Header words in model-checked atomics, indexed as the section is.
    struct Model {
        words: Vec<AtomicU32>,
        dwords: Vec<AtomicU64>,
    }

    impl Model {
        fn new(slots: u32) -> Arc<Self> {
            let m = Self {
                words: (0..crate::HEADER / 4).map(|_| AtomicU32::new(0)).collect(),
                dwords: (0..crate::HEADER / 8).map(|_| AtomicU64::new(0)).collect(),
            };
            set_slots(&m, slots);
            Arc::new(m)
        }
    }

    impl Words for Model {
        fn word(&self, offset: usize) -> &AtomicU32 {
            &self.words[offset / 4]
        }

        fn dword(&self, offset: usize) -> &AtomicU64 {
            &self.dwords[offset / 8]
        }
    }

    /// The helper "writes" a slot by storing its fence value in the slot's marker
    /// while it holds `WRITING`. A leased picture's marker must equal its fence: a
    /// texture is never written under a lease.
    fn write_and_publish(m: &Model, markers: &[AtomicU64], fence: u64) -> Option<u32> {
        let slot = claim(m)?;
        markers[slot as usize].store(fence, Ordering::Relaxed);
        publish(m, slot, fence, fence);
        Some(slot)
    }

    fn markers(n: usize) -> Arc<Vec<AtomicU64>> {
        Arc::new((0..n).map(|_| AtomicU64::new(0)).collect())
    }

    /// Three publishes on two slots race one take. Whatever the studio leases is the
    /// picture the helper finished in that slot, paired with its own sequence, and it
    /// is not written while leased (claim before lease, lease before claim, publish
    /// between the studio's two sequence reads).
    #[test]
    fn loom_a_leased_slot_is_never_written() {
        model(|| {
            let m = Model::new(2);
            let marks = markers(2);
            let studio = {
                let (m, marks) = (Arc::clone(&m), Arc::clone(&marks));
                thread::spawn(move || {
                    let p = lease_latest(&*m, Some(0))?;
                    assert_eq!(marks[p.slot as usize].load(Ordering::Relaxed), p.fence);
                    assert_eq!(p.painted, p.fence, "fence and paint time of one publish");
                    // Fence n is publish n: never paired with another publish's
                    // sequence (the second sequence read).
                    assert_eq!(p.seq, p.fence, "sequence of another publish");
                    Some(p)
                })
            };
            let mut fence = 0;
            for _ in 0..3 {
                if write_and_publish(&m, &marks, fence + 1).is_some() {
                    fence += 1;
                }
            }
            if let Some(p) = studio.join().unwrap() {
                assert_eq!(
                    marks[p.slot as usize].load(Ordering::Relaxed),
                    p.fence,
                    "written under its lease"
                );
                lease_sub(&*m, p.slot);
            }
        });
    }

    /// The slot published last is never claimed, so a page shown again always has
    /// its latest picture, even with a lease racing the claim.
    #[test]
    fn loom_the_latest_published_slot_is_never_claimed() {
        model(|| {
            let m = Model::new(2);
            let marks = markers(2);
            let first = write_and_publish(&m, &marks, 1).unwrap();
            let studio = {
                let (m, marks) = (Arc::clone(&m), Arc::clone(&marks));
                thread::spawn(move || {
                    let p = lease_latest(&*m, None)?;
                    assert_eq!(marks[p.slot as usize].load(Ordering::Relaxed), p.fence);
                    lease_sub(&*m, p.slot);
                    Some(p)
                })
            };
            let second = write_and_publish(&m, &marks, 2);
            assert_ne!(second, Some(first), "claimed the latest slot");
            let third = write_and_publish(&m, &marks, 3);
            if let Some(second) = second {
                assert_ne!(third, Some(second), "claimed the latest slot");
            }
            studio.join().unwrap();
        });
    }

    /// A lease that lands on a slot being written backs off and leaves the count as
    /// it was; after the publish the newest picture can be taken and no lease leaks.
    #[test]
    fn loom_a_lease_on_a_slot_being_written_backs_off() {
        model(|| {
            let m = Model::new(2);
            let marks = markers(2);
            write_and_publish(&m, &marks, 1).unwrap();
            let studio = {
                let (m, marks) = (Arc::clone(&m), Arc::clone(&marks));
                thread::spawn(move || {
                    let mut seen = 0;
                    if let Some(p) = lease_latest(&*m, Some(seen)) {
                        assert_eq!(marks[p.slot as usize].load(Ordering::Relaxed), p.fence);
                        seen = p.seq;
                        lease_sub(&*m, p.slot);
                    }
                    seen
                })
            };
            write_and_publish(&m, &marks, 2).unwrap();
            let seen = studio.join().unwrap();
            for slot in 0..2 {
                assert_eq!(lease(&*m, slot), 0, "a lease leaked on slot {slot}");
                assert_eq!(state(&*m, slot).load(Ordering::Relaxed) & WRITING, 0);
            }
            // The newest publish is there to take, unless the studio took it.
            if seen != 2 {
                let p = lease_latest(&*m, Some(seen)).unwrap();
                assert_eq!(p.fence, 2);
                lease_sub(&*m, p.slot);
            }
        });
    }

    /// A helper that dies while it holds `WRITING` never lets the studio lease that
    /// slot, and the studio's other leases are unaffected. (The two sides touch no
    /// word both write, so loom finds a single order.)
    #[test]
    fn loom_a_slot_left_writing_is_never_leased() {
        model(|| {
            let m = Model::new(2);
            let marks = markers(2);
            write_and_publish(&m, &marks, 1).unwrap();
            let studio = {
                let m = Arc::clone(&m);
                thread::spawn(move || lease_latest(&*m, None))
            };
            // Claims and "dies": never publishes or abandons.
            let stuck = claim(&*m);
            if let Some(p) = studio.join().unwrap() {
                assert_ne!(Some(p.slot), stuck);
                lease_sub(&*m, p.slot);
            }
            if let Some(stuck) = stuck {
                assert_ne!(state(&*m, stuck).load(Ordering::Relaxed) & WRITING, 0);
                assert_eq!(
                    lease_latest(&*m, Some(1)),
                    None,
                    "the stuck slot is not taken"
                );
            }
        });
    }
}
