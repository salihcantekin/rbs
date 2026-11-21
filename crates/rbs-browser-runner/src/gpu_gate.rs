//! The supervisor's gate for GPU pages (ADR-0157 decision 2).
//!
//! A picture the helper published is handed to the studio only once the shared
//! fence's completed value has reached the picture's value. The studio never queues
//! a GPU wait on a value an untrusted helper may never signal. Two pictures may
//! wait, each holding its lease: the oldest, and the next one taken behind it. A
//! waiting picture is never replaced, so a GPU that finishes copies a frame or two
//! late (a game on the same card) still shows every taken picture in turn at the
//! page rate. A picture that does not finish within [`FENCE_WAIT_FRAMES`] page
//! frames of being taken, measured on the supervisor's clock, fails the gate.
//!
//! On macOS the helper publishes a slot only after its copy completed (ADR-0167
//! decision 6): the gate runs without a fence ([`GpuGate::fenceless`]), passes each
//! picture on at once, and checks that the fence word, the helper's claim sequence,
//! rises with every publish.

use std::time::{Duration, Instant};

use rbs_browser_shm::{FrameRing, GpuPicture};

/// The completed value of a page's shared fence, and the studio's failed imports of
/// the page's pictures. Reading them never waits.
pub trait FenceView: Send {
    /// The fence's completed value (`u64::MAX` when the device was removed).
    fn completed(&self) -> u64;

    /// Imports of this page's pictures that failed in a row on the render thread
    /// (ADR-0157 decision 9).
    fn failed_imports(&self) -> u32 {
        0
    }
}

/// Failed imports in a row that fall the tree back with reason `import`.
pub const IMPORT_FAILURES: u32 = 10;

/// Page frames a picture may wait for its fence (ADR-0157 decision 2).
pub const FENCE_WAIT_FRAMES: u32 = 30;

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

/// One page's gate. Holds at most two waiting pictures.
pub struct GpuGate {
    fence: Box<dyn FenceView>,
    seen: u64,
    /// The oldest picture waiting for its fence, and when it was taken.
    waiting: Option<(GpuPicture, Instant)>,
    /// The picture taken behind it, and when.
    next: Option<(GpuPicture, Instant)>,
    /// How long a picture may wait: [`FENCE_WAIT_FRAMES`] page frames.
    limit: Duration,
    /// The highest fence value published so far.
    newest: u64,
    /// Why the gate failed: `fence` or `import` (ADR-0157 decision 9).
    reason: Option<&'static str>,
    /// No fence: a published picture's copy has already finished (ADR-0167).
    fenceless: bool,
}

impl std::fmt::Debug for GpuGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuGate")
            .field("seen", &self.seen)
            .field("waiting", &self.waiting.map(|(p, _)| p))
            .field("next", &self.next.map(|(p, _)| p))
            .field("limit", &self.limit)
            .finish()
    }
}

impl GpuGate {
    /// A gate reading `fence` for a page painting at `fps`.
    pub fn new(fence: Box<dyn FenceView>, fps: u32) -> Self {
        Self {
            fence,
            seen: 0,
            waiting: None,
            next: None,
            limit: Duration::from_secs(1) * FENCE_WAIT_FRAMES / fps.clamp(1, 60),
            newest: 0,
            reason: None,
            fenceless: false,
        }
    }

    /// A gate for a page whose helper publishes only finished copies (macOS,
    /// ADR-0167 decision 6). `view` gives the failed imports; its fence value is not
    /// read. A picture passes on its first pass; a fence word (the claim sequence)
    /// that does not rise above the newest seen fails the gate with `fence`.
    pub fn fenceless(view: Box<dyn FenceView>, fps: u32) -> Self {
        Self {
            fenceless: true,
            ..Self::new(view, fps)
        }
    }

    /// One supervisor pass over `ring` at `now`. Never waits.
    pub fn pass(&mut self, ring: &FrameRing, now: Instant) -> GatePass {
        if self.fence.failed_imports() >= IMPORT_FAILURES {
            return self.fail(ring, "import");
        }
        if self.fenceless {
            let Some(new) = ring.take_gpu(&mut self.seen) else {
                return GatePass::Nothing;
            };
            if new.fence <= self.newest {
                ring.lease_sub(new.slot);
                return self.fail(ring, "fence");
            }
            self.newest = new.fence;
            return GatePass::Ready(new);
        }
        if self.next.is_none()
            && let Some(new) = ring.take_gpu(&mut self.seen)
        {
            self.newest = self.newest.max(new.fence);
            if self.waiting.is_none() {
                self.waiting = Some((new, now));
            } else {
                self.next = Some((new, now));
            }
        }
        let Some((picture, since)) = self.waiting else {
            return GatePass::Nothing;
        };
        let completed = self.fence.completed();
        if completed == u64::MAX || completed > self.newest.saturating_add(FENCE_JUMP) {
            return self.fail(ring, "fence");
        }
        if completed >= picture.fence {
            self.waiting = self.next.take();
            return GatePass::Ready(picture);
        }
        if now.saturating_duration_since(since) >= self.limit {
            return self.fail(ring, "fence");
        }
        GatePass::Nothing
    }

    /// The latest picture once more, for a page shown again, when its copy has
    /// finished. Its lease is the caller's.
    pub fn again(&mut self, ring: &FrameRing) -> Option<GpuPicture> {
        let picture = ring.lease_latest_gpu()?;
        if self.fenceless {
            // The same picture again is allowed; a value above the newest seen is a
            // publish `pass` has not taken yet, which is just as finished.
            return Some(picture);
        }
        let completed = self.fence.completed();
        // The same validity rule as [`GpuGate::pass`]: a removed device or an
        // implausible value lets nothing through.
        let valid = completed != u64::MAX
            && completed <= self.newest.max(picture.fence).saturating_add(FENCE_JUMP);
        if valid && completed >= picture.fence {
            return Some(picture);
        }
        ring.lease_sub(picture.slot);
        None
    }

    /// Gives back the leases of the waiting pictures (a stop or an exit).
    pub fn release(&mut self, ring: &FrameRing) {
        for (old, _) in [self.waiting.take(), self.next.take()]
            .into_iter()
            .flatten()
        {
            ring.lease_sub(old.slot);
        }
    }

    /// Why the gate failed, once it has: `fence` or `import`.
    pub fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    fn fail(&mut self, ring: &FrameRing, reason: &'static str) -> GatePass {
        self.reason = Some(reason);
        self.release(ring);
        GatePass::Failed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use rbs_browser_shm::SlotLayout;

    use super::*;

    const FPS: u32 = 30;
    /// One page frame at [`FPS`].
    const FRAME: Duration = Duration::from_nanos(1_000_000_000 / FPS as u64);

    struct Fence(Arc<AtomicU64>);

    impl FenceView for Fence {
        fn completed(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    struct Imports(Arc<AtomicU64>, Arc<AtomicU32>);

    impl FenceView for Imports {
        fn completed(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }

        fn failed_imports(&self) -> u32 {
            self.1.load(Ordering::Relaxed)
        }
    }

    /// Ten failed imports in a row fail the gate and give its leases back; nine do
    /// not (ADR-0157 decisions 9 and 11).
    #[test]
    fn ten_failed_imports_in_a_row_fail_the_gate() {
        let ring = FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, 4).unwrap();
        let value = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU32::new(0));
        let mut gate = GpuGate::new(
            Box::new(Imports(Arc::clone(&value), Arc::clone(&failed))),
            FPS,
        );
        let t0 = Instant::now();
        let slot = publish(&ring, 1).unwrap();
        failed.store(IMPORT_FAILURES - 1, Ordering::Relaxed);
        assert_eq!(
            gate.pass(&ring, t0),
            GatePass::Nothing,
            "nine are not enough"
        );
        assert_eq!(ring.lease(slot), 1);
        failed.store(IMPORT_FAILURES, Ordering::Relaxed);
        assert_eq!(gate.pass(&ring, t0), GatePass::Failed);
        assert_eq!(gate.reason(), Some("import"));
        assert_eq!(ring.lease(slot), 0, "the waiting lease is given back");
    }

    fn setup() -> (FrameRing, Arc<AtomicU64>, GpuGate, Instant) {
        let ring = FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, 4).unwrap();
        let value = Arc::new(AtomicU64::new(0));
        let gate = GpuGate::new(Box::new(Fence(Arc::clone(&value))), FPS);
        (ring, value, gate, Instant::now())
    }

    fn publish(ring: &FrameRing, fence: u64) -> Option<u32> {
        let slot = ring.claim_gpu()?;
        ring.publish_gpu(slot, fence);
        Some(slot)
    }

    fn fenceless() -> (FrameRing, Arc<AtomicU32>, GpuGate, Instant) {
        let ring = FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, 4).unwrap();
        let failed = Arc::new(AtomicU32::new(0));
        // A fence that never moves: the fence-less gate must not read it.
        let view = Imports(Arc::new(AtomicU64::new(0)), Arc::clone(&failed));
        let gate = GpuGate::fenceless(Box::new(view), FPS);
        (ring, failed, gate, Instant::now())
    }

    /// Without a fence a published picture goes out on the first pass with its
    /// lease, and shown again it is leased once more (ADR-0167 decision 6).
    #[test]
    fn a_fenceless_gate_passes_each_picture_at_once() {
        let (ring, _, mut gate, t0) = fenceless();
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing);
        let slot = publish(&ring, 1).unwrap();
        let GatePass::Ready(p) = gate.pass(&ring, t0) else {
            panic!("not passed at once")
        };
        assert_eq!((p.slot, p.fence), (slot, 1));
        assert_eq!(ring.lease(slot), 1);
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing, "taken once");
        // Long after, still no timeout: nothing waits.
        assert_eq!(gate.pass(&ring, t0 + FRAME * 1_000), GatePass::Nothing);
        let again = gate.again(&ring).unwrap();
        assert_eq!(again.slot, slot);
        assert_eq!(ring.lease(slot), 2);
        let second = publish(&ring, 2).unwrap();
        assert!(matches!(gate.pass(&ring, t0), GatePass::Ready(p) if p.slot == second));
    }

    /// A claim sequence that does not rise fails the gate with `fence`.
    #[test]
    fn a_fenceless_gate_fails_on_a_sequence_that_does_not_rise() {
        let (ring, _, mut gate, t0) = fenceless();
        publish(&ring, 5).unwrap();
        let GatePass::Ready(p) = gate.pass(&ring, t0) else {
            panic!()
        };
        ring.lease_sub(p.slot);
        let slot = publish(&ring, 5).unwrap();
        assert_eq!(gate.pass(&ring, t0), GatePass::Failed);
        assert_eq!(gate.reason(), Some("fence"));
        assert_eq!(ring.lease(slot), 0, "the lease is given back");
    }

    /// Failed imports still fall a fence-less page back with `import`.
    #[test]
    fn a_fenceless_gate_keeps_the_import_fallback() {
        let (ring, failed, mut gate, t0) = fenceless();
        failed.store(IMPORT_FAILURES, Ordering::Relaxed);
        publish(&ring, 1).unwrap();
        assert_eq!(gate.pass(&ring, t0), GatePass::Failed);
        assert_eq!(gate.reason(), Some("import"));
    }

    #[test]
    fn a_picture_waits_for_its_fence_then_goes_out_with_its_lease() {
        let (ring, value, mut gate, t0) = setup();
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing);
        let slot = publish(&ring, 1).unwrap();
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing, "copy not done");
        assert_eq!(ring.lease(slot), 1, "held while it waits");
        value.store(1, Ordering::Relaxed);
        match gate.pass(&ring, t0 + FRAME) {
            GatePass::Ready(p) => assert_eq!((p.slot, p.fence), (slot, 1)),
            other => panic!("{other:?}"),
        }
        assert_eq!(ring.lease(slot), 1, "the lease went to the caller");
        assert_eq!(gate.pass(&ring, t0 + FRAME), GatePass::Nothing);
    }

    #[test]
    fn a_waiting_picture_is_not_replaced_and_the_newer_one_follows() {
        let (ring, value, mut gate, t0) = setup();
        let first = publish(&ring, 1).unwrap();
        gate.pass(&ring, t0);
        let second = publish(&ring, 2).unwrap();
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing);
        let third = publish(&ring, 3).unwrap();
        assert_eq!(gate.pass(&ring, t0), GatePass::Nothing);
        assert_eq!(ring.lease(first), 1, "still waiting, still held");
        assert_eq!(ring.lease(second), 1, "taken behind it, held");
        assert_eq!(ring.lease(third), 0, "not taken while two wait");
        value.store(2, Ordering::Relaxed);
        match gate.pass(&ring, t0 + FRAME) {
            GatePass::Ready(p) => assert_eq!(p.slot, first),
            other => panic!("{other:?}"),
        }
        match gate.pass(&ring, t0 + FRAME) {
            GatePass::Ready(p) => assert_eq!(p.slot, second),
            other => panic!("{other:?}"),
        }
    }

    /// Runs 300 page frames where each copy completes `lag` after its publish (on
    /// the clock, not on the publish count) and the studio holds each ready lease
    /// for two frames, as the importer and compositor do. Returns the pictures that
    /// went out; panics if the gate fails.
    fn late_fence_run(slots: u32, lag: Duration, poll: Duration) -> u32 {
        let ring = FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, slots).unwrap();
        let value = Arc::new(AtomicU64::new(0));
        let mut gate = GpuGate::new(Box::new(Fence(Arc::clone(&value))), FPS);
        let t0 = Instant::now();
        let mut now = t0;
        let mut fence = 0_u64;
        let mut next_publish = t0;
        let mut in_flight: std::collections::VecDeque<(u64, Instant)> = Default::default();
        let mut held: std::collections::VecDeque<(u32, Instant)> = Default::default();
        let mut ready = 0;
        while now < t0 + FRAME * 300 {
            while held.front().is_some_and(|&(_, until)| until <= now) {
                let (slot, _) = held.pop_front().unwrap();
                ring.lease_sub(slot);
            }
            if now >= next_publish {
                next_publish += FRAME;
                if publish(&ring, fence + 1).is_some() {
                    fence += 1;
                    in_flight.push_back((fence, now + lag));
                }
            }
            while in_flight.front().is_some_and(|&(_, done)| done <= now) {
                let (done, _) = in_flight.pop_front().unwrap();
                value.store(done, Ordering::Relaxed);
            }
            match gate.pass(&ring, now) {
                GatePass::Ready(p) => {
                    ready += 1;
                    held.push_back((p.slot, now + FRAME * 2));
                }
                GatePass::Failed => panic!("failed: {slots} slots, poll {poll:?}"),
                GatePass::Nothing => {}
            }
            now += poll;
        }
        ready
    }

    /// A GPU shared with a game finishes each copy two frames late while the studio
    /// holds each picture two frames. The gate never fails, at the 5 ms sound poll and
    /// at the plain half-frame poll (round-4 review). With the default four slots the
    /// waiting pictures and the held leases leave the helper short of free slots, so
    /// about four in five pictures go out; six slots show nearly all (round-5 review,
    /// ADR-0157 decision 5).
    #[test]
    fn a_fence_two_frames_behind_keeps_pictures_flowing_and_never_fails() {
        let sound = Duration::from_millis(5);
        for slots in [4, 6] {
            assert!(late_fence_run(slots, FRAME * 2, FRAME / 2) > 150);
        }
        let four = late_fence_run(4, FRAME * 2, sound);
        assert!(four > 220, "only {four} of 300 pictures with four slots");
        let six = late_fence_run(6, FRAME * 2, sound);
        assert!(six > 290, "only {six} of 300 pictures with six slots");
    }

    /// A copy that finishes within the frame loses nothing with four slots.
    #[test]
    fn a_fence_on_time_shows_every_picture_with_four_slots() {
        let ready = late_fence_run(4, FRAME / 4, Duration::from_millis(5));
        assert!(ready > 295, "only {ready} of 300 pictures");
    }

    /// A helper whose GPU hangs keeps publishing; the waiting picture's timer runs
    /// on the clock, not on passes (round-3 and round-4 review notes).
    #[test]
    fn a_fence_that_never_moves_fails_after_thirty_frames_whatever_the_poll() {
        for poll in [
            Duration::from_millis(5),
            FRAME / 2,
            Duration::from_millis(500),
        ] {
            let (ring, _value, mut gate, t0) = setup();
            let mut now = t0;
            let mut fence = 0;
            let failed_at = loop {
                fence += 1;
                publish(&ring, fence);
                if gate.pass(&ring, now) == GatePass::Failed {
                    break now - t0;
                }
                assert!(now - t0 < FRAME * 100, "never failed at poll {poll:?}");
                now += poll;
            };
            assert!(
                failed_at >= FRAME * FENCE_WAIT_FRAMES,
                "{failed_at:?} at {poll:?}"
            );
            assert!(
                failed_at < FRAME * FENCE_WAIT_FRAMES + poll + poll,
                "{failed_at:?} at {poll:?}"
            );
            for slot in 0..4 {
                assert_eq!(ring.lease(slot), 0, "slot {slot} leased after the failure");
            }
        }
    }

    #[test]
    fn a_removed_device_or_an_implausible_value_fails_at_once() {
        let (ring, value, mut gate, t0) = setup();
        publish(&ring, 1);
        value.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(gate.pass(&ring, t0), GatePass::Failed);
        let (ring, value, mut gate, t0) = setup();
        publish(&ring, 3);
        value.store(3 + FENCE_JUMP + 1, Ordering::Relaxed);
        assert_eq!(gate.pass(&ring, t0), GatePass::Failed);
    }

    #[test]
    fn a_page_shown_again_gets_its_finished_latest_picture() {
        let (ring, value, mut gate, _t0) = setup();
        let slot = publish(&ring, 1).unwrap();
        assert_eq!(gate.again(&ring), None, "copy not done");
        assert_eq!(ring.lease(slot), 0);
        value.store(1, Ordering::Relaxed);
        assert_eq!(gate.again(&ring).map(|p| p.slot), Some(slot));
        assert_eq!(ring.lease(slot), 1);
    }
}
