//! The supervisor's gate for GPU pages (ADR-0157 decision 2).
//!
//! A picture the helper published is handed to the studio only once the shared
//! fence's completed value has reached the picture's value. The studio never queues
//! a GPU wait on a value an untrusted helper may never signal. A picture that waits
//! holds its lease; a newer one replaces it without restarting the wait, so a helper
//! whose GPU stopped while it keeps publishing still falls back.

use rbs_browser_shm::{FrameRing, GpuPicture};

/// The completed value of a page's shared fence. Reading it never waits.
pub trait FenceView: Send {
    /// The fence's completed value (`u64::MAX` when the device was removed).
    fn completed(&self) -> u64;
}

/// Supervisor passes a picture may wait for its fence: 30 page frames at two
/// passes per frame (ADR-0157 decision 2).
pub const FENCE_WAIT_PASSES: u32 = 60;

/// A completed value this far past the newest published one is implausible.
pub const FENCE_JUMP: u64 = 1 << 20;

/// What one supervisor pass found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePass {
    /// Nothing to hand over yet.
    Nothing,
    /// A picture whose copy has finished; its lease is the caller's now.
    Ready(GpuPicture),
    /// The fence failed (never completed in time, device removed, or an
    /// implausible value): the tree falls back to shared memory.
    Failed,
}

/// One page's gate. Holds at most one waiting picture.
pub struct GpuGate {
    fence: Box<dyn FenceView>,
    seen: u64,
    waiting: Option<GpuPicture>,
    /// Passes since the oldest picture still waiting was taken.
    waited: u32,
    /// The highest fence value published so far.
    newest: u64,
}

impl std::fmt::Debug for GpuGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuGate")
            .field("seen", &self.seen)
            .field("waiting", &self.waiting)
            .field("waited", &self.waited)
            .finish()
    }
}

impl GpuGate {
    /// A gate reading `fence`.
    pub fn new(fence: Box<dyn FenceView>) -> Self {
        Self {
            fence,
            seen: 0,
            waiting: None,
            waited: 0,
            newest: 0,
        }
    }

    /// One supervisor pass over `ring`. Never waits.
    pub fn pass(&mut self, ring: &FrameRing) -> GatePass {
        if let Some(new) = ring.take_gpu(&mut self.seen) {
            self.newest = self.newest.max(new.fence);
            if let Some(old) = self.waiting.replace(new) {
                // Replaced, not finished: the wait keeps counting.
                ring.lease_sub(old.slot);
            } else {
                self.waited = 0;
            }
        }
        let Some(picture) = self.waiting else {
            return GatePass::Nothing;
        };
        let completed = self.fence.completed();
        if completed == u64::MAX || completed > self.newest.saturating_add(FENCE_JUMP) {
            return self.fail(ring);
        }
        if completed >= picture.fence {
            self.waiting = None;
            self.waited = 0;
            return GatePass::Ready(picture);
        }
        self.waited += 1;
        if self.waited >= FENCE_WAIT_PASSES {
            return self.fail(ring);
        }
        GatePass::Nothing
    }

    /// The latest picture once more, for a page shown again, when its copy has
    /// finished. Its lease is the caller's.
    pub fn again(&mut self, ring: &FrameRing) -> Option<GpuPicture> {
        let picture = ring.lease_latest_gpu()?;
        if self.fence.completed() >= picture.fence {
            return Some(picture);
        }
        ring.lease_sub(picture.slot);
        None
    }

    /// Gives back the lease of a waiting picture (a stop or an exit).
    pub fn release(&mut self, ring: &FrameRing) {
        if let Some(old) = self.waiting.take() {
            ring.lease_sub(old.slot);
        }
    }

    fn fail(&mut self, ring: &FrameRing) -> GatePass {
        self.release(ring);
        GatePass::Failed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rbs_browser_shm::SlotLayout;

    use super::*;

    struct Fence(Arc<AtomicU64>);

    impl FenceView for Fence {
        fn completed(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn setup() -> (FrameRing, Arc<AtomicU64>, GpuGate) {
        let ring = FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, 4).unwrap();
        let value = Arc::new(AtomicU64::new(0));
        let gate = GpuGate::new(Box::new(Fence(Arc::clone(&value))));
        (ring, value, gate)
    }

    fn publish(ring: &FrameRing, fence: u64) -> u32 {
        let slot = ring.claim_gpu().unwrap();
        ring.publish_gpu(slot, fence);
        slot
    }

    #[test]
    fn a_picture_waits_for_its_fence_then_goes_out_with_its_lease() {
        let (ring, value, mut gate) = setup();
        assert_eq!(gate.pass(&ring), GatePass::Nothing);
        let slot = publish(&ring, 1);
        assert_eq!(gate.pass(&ring), GatePass::Nothing, "copy not done");
        assert_eq!(ring.lease(slot), 1, "held while it waits");
        value.store(1, Ordering::Relaxed);
        match gate.pass(&ring) {
            GatePass::Ready(p) => assert_eq!((p.slot, p.fence), (slot, 1)),
            other => panic!("{other:?}"),
        }
        assert_eq!(ring.lease(slot), 1, "the lease went to the caller");
        assert_eq!(gate.pass(&ring), GatePass::Nothing);
    }

    #[test]
    fn a_newer_picture_replaces_a_waiting_one_and_gives_its_lease_back() {
        let (ring, value, mut gate) = setup();
        let first = publish(&ring, 1);
        gate.pass(&ring);
        let second = publish(&ring, 2);
        gate.pass(&ring);
        assert_eq!(ring.lease(first), 0);
        value.store(2, Ordering::Relaxed);
        match gate.pass(&ring) {
            GatePass::Ready(p) => assert_eq!(p.slot, second),
            other => panic!("{other:?}"),
        }
    }

    /// A helper whose GPU hangs keeps publishing; replacing the waiting picture
    /// does not restart the wait (round-3 review note).
    #[test]
    fn a_fence_that_never_moves_fails_even_while_the_helper_keeps_publishing() {
        let (ring, _value, mut gate) = setup();
        let mut fence = 0;
        let mut failed_at = None;
        for pass in 0..FENCE_WAIT_PASSES + 5 {
            fence += 1;
            publish(&ring, fence);
            if gate.pass(&ring) == GatePass::Failed {
                failed_at = Some(pass);
                break;
            }
        }
        assert_eq!(failed_at, Some(FENCE_WAIT_PASSES - 1));
        for slot in 0..4 {
            assert_eq!(ring.lease(slot), 0, "slot {slot} leased after the failure");
        }
    }

    #[test]
    fn a_removed_device_or_an_implausible_value_fails_at_once() {
        let (ring, value, mut gate) = setup();
        publish(&ring, 1);
        value.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(gate.pass(&ring), GatePass::Failed);
        let (ring, value, mut gate) = setup();
        publish(&ring, 3);
        value.store(3 + FENCE_JUMP + 1, Ordering::Relaxed);
        assert_eq!(gate.pass(&ring), GatePass::Failed);
    }

    #[test]
    fn a_page_shown_again_gets_its_finished_latest_picture() {
        let (ring, value, mut gate) = setup();
        let slot = publish(&ring, 1);
        assert_eq!(gate.again(&ring), None, "copy not done");
        assert_eq!(ring.lease(slot), 0);
        value.store(1, Ordering::Relaxed);
        assert_eq!(gate.again(&ring).map(|p| p.slot), Some(slot));
        assert_eq!(ring.lease(slot), 1);
    }
}
