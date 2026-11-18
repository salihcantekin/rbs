//! The picture side of a sync offset measurement (ADR-0126 decision 3; spec
//! `io-nodes.md`, "Sync picture probe").
//!
//! [`SyncPictureProbeNode`] sits on the render partition after the reference source's
//! picture (its chain tail). On a tick where the picture's `(timestamp, sequence)` key
//! is the one it saw last (a camera slower than the canvas repeats its frame), it does
//! nothing more: one compare. For a new picture it dispatches a [`MotionProbe`] stamped
//! with the tick's time, the time the program shows that picture at. Completed values
//! go to a fixed, lock-free [`SyncPictureLog`] that the control thread reads. It exists
//! only while a measurement runs (ADR-0013).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::MotionProbe;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::yuv_decode::YuvToRgb;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};

/// Type id of [`SyncPictureProbeNode`].
pub const SYNC_PICTURE_PROBE_NODE_TYPE: &str = "org.rbs.sync-picture-probe";

/// New pictures per second the log is sized for (a 240 fps camera).
pub const SYNC_PICTURE_MAX_FPS: u64 = 240;

const PICTURE_PIN: usize = 0;

/// The descriptor of [`SyncPictureProbeNode`]: kind `Output`, input "picture".
pub fn sync_picture_probe_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(SYNC_PICTURE_PROBE_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("picture", LinkKind::Video)],
        vec![],
    )
}

/// What a picture probe logged: `(tick time in master ns, motion)` per new picture,
/// in time order, written by the render thread, read by any thread without locks.
#[derive(Debug)]
pub struct SyncPictureLog {
    times: Box<[AtomicU64]>,
    motions: Box<[AtomicU32]>,
    len: AtomicUsize,
    /// Values not logged because the log was full.
    full: AtomicU64,
    /// New pictures dropped because every readback slot was in flight.
    busy: AtomicU64,
    /// New pictures whose dispatch failed (not a pool texture, stale, not sampleable).
    failed: AtomicU64,
}

impl SyncPictureLog {
    fn new(capacity: usize) -> Self {
        Self {
            times: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            motions: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            len: AtomicUsize::new(0),
            full: AtomicU64::new(0),
            busy: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    /// Entries the log can hold.
    pub fn capacity(&self) -> usize {
        self.times.len()
    }

    /// Entries written so far.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// True before the first entry.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Values not logged because the log was full.
    pub fn full(&self) -> u64 {
        self.full.load(Ordering::Relaxed)
    }

    /// New pictures dropped because every readback slot was in flight.
    pub fn busy(&self) -> u64 {
        self.busy.load(Ordering::Relaxed)
    }

    /// New pictures whose dispatch failed.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Appends the entries from index `from` to the current length to `out` and
    /// returns the new length, the `from` of the next call. Off the render thread.
    pub fn read_from(&self, from: usize, out: &mut Vec<(i64, f32)>) -> usize {
        let len = self.len();
        for i in from.min(len)..len {
            let t = self.times[i].load(Ordering::Relaxed);
            let m = f32::from_bits(self.motions[i].load(Ordering::Relaxed));
            out.push((i64::try_from(t).unwrap_or(i64::MAX), m));
        }
        len
    }

    fn push(&self, time_ns: u64, motion: f32) {
        // Single writer: the probe's process.
        let i = self.len.load(Ordering::Relaxed);
        if i >= self.times.len() {
            self.full.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.times[i].store(time_ns, Ordering::Relaxed);
        self.motions[i].store(motion.to_bits(), Ordering::Relaxed);
        self.len.store(i + 1, Ordering::Release);
    }
}

/// Logs the motion of each new picture of a source while a measurement runs.
pub struct SyncPictureProbeNode {
    descriptor: NodeDescriptor,
    probe: Option<MotionProbe>,
    last: Option<(Timestamp, u64)>,
    log: Arc<SyncPictureLog>,
    /// Decodes a 4:2:0 camera picture for the probe (perf audit R-11); holds nothing
    /// until the first such frame.
    rgb: YuvToRgb,
}

impl SyncPictureProbeNode {
    /// A probe whose log holds `window` of pictures at up to
    /// [`SYNC_PICTURE_MAX_FPS`]. Allocates; call off the render thread.
    pub fn new(window: Duration) -> (Self, Arc<SyncPictureLog>) {
        let entries = window
            .as_millis()
            .saturating_mul(u128::from(SYNC_PICTURE_MAX_FPS))
            .div_ceil(1000)
            + 1;
        let log = Arc::new(SyncPictureLog::new(
            usize::try_from(entries).unwrap_or(usize::MAX),
        ));
        let node = Self {
            descriptor: sync_picture_probe_descriptor(),
            probe: None,
            last: None,
            log: Arc::clone(&log),
            rgb: YuvToRgb::new(),
        };
        (node, log)
    }
}

impl Node<GpuRuntime> for SyncPictureProbeNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let probe = MotionProbe::new(ctx.shared)
            .map_err(|e| NodeError::Failed(format!("sync picture probe: {e}")))?;
        self.probe = Some(probe);
        self.last = None;
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let (inputs, _, local) = ctx.parts();
        let probe = self.probe.as_mut().ok_or(NodeError::NotReady)?;
        let log = &self.log;
        probe.take(gpu, |t, m| log.push(t, m));

        let Some(picture) = inputs.video(PICTURE_PIN, 0) else {
            return Ok(());
        };
        let key = (picture.timestamp(), picture.sequence());
        // Steady state between a camera's frames: one compare.
        if self.last == Some(key) {
            return Ok(());
        }
        self.last = Some(key);
        // The probe samples RGB: a 4:2:0 picture is decoded once.
        let picture = if YuvToRgb::needs_decode(&local.pool, picture) {
            match self.rgb.rgb(gpu, &mut local.pool, picture) {
                Ok(rgb) => rgb,
                Err(_) => {
                    self.log.failed.fetch_add(1, Ordering::Relaxed);
                    return Err(NodeError::Backend("sync picture decode failed"));
                }
            }
        } else {
            picture
        };
        match probe.submit(gpu, &local.pool, picture, tick.time.as_nanos()) {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.log.busy.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => {
                self.log.failed.fetch_add(1, Ordering::Relaxed);
                Err(NodeError::Backend("sync picture probe dispatch failed"))
            }
        }
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.probe = None;
        self.last = None;
        self.rgb.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_a_valid_video_output() {
        let d = sync_picture_probe_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
        let (node, log) = SyncPictureProbeNode::new(Duration::from_secs(10));
        assert_eq!(node.descriptor, d);
        assert_eq!(log.capacity(), 2_401);
        assert!(log.is_empty());
    }

    #[test]
    fn the_log_reads_in_order_and_counts_what_did_not_fit() {
        let log = SyncPictureLog::new(2);
        for i in 0..4u64 {
            log.push(i, i as f32);
        }
        assert_eq!((log.len(), log.full()), (2, 2));
        let mut out = Vec::new();
        assert_eq!(log.read_from(0, &mut out), 2);
        assert_eq!(out, vec![(0, 0.0), (1, 1.0)]);
    }
}
