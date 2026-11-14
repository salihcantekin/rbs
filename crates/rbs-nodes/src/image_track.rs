//! The image track of a still source (ADR-0117 decision 5; spec `io-nodes.md`, "Image
//! track node").
//!
//! [`ImageTrackNode`] sits on the render partition after the source's picture (its own
//! node, or its chain tail when filters are asked for). On a tick where the picture's
//! `(timestamp, sequence)` key is the one it last took, it does nothing more: one
//! compare. When the key changed, the interval allows it, the previous readback is
//! done and the track does not give way, it starts one [`RgbaDownload`]. The completed
//! picture goes on "video" as a `System` frame stamped with the tick's time and index,
//! for the PNG writer on another partition. Nothing here encodes or writes a file.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::RgbaDownload;
use rbs_gpu::node::GpuRuntime;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick};

use crate::export::MasterDrops;

/// Type id of [`ImageTrackNode`].
pub const IMAGE_TRACK_NODE_TYPE: &str = "org.rbs.image-track";

/// The shortest time between two images of one track (ADR-0117 decision 3). A change
/// inside the window is taken at its end, showing the picture as it is then.
pub const MIN_IMAGE_INTERVAL: Duration = Duration::from_millis(100);

/// Most images one track writes in one take (ADR-0117 decision 3). Later changes are
/// counted in [`ImageTrackStats::over_cap`] and not written.
pub const MAX_IMAGES_PER_TRACK: u64 = 1_000;

const PICTURE_PIN: usize = 0;
const VIDEO_PIN: usize = 0;

/// The descriptor of [`ImageTrackNode`]: input "picture", output "video".
pub fn image_track_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(IMAGE_TRACK_NODE_TYPE),
        NodeKind::VideoFilter,
        vec![Pin::one("picture", LinkKind::Video)],
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Counters of an image track, readable from any thread.
#[derive(Debug, Default)]
pub struct ImageTrackStats {
    /// Pictures whose readback started; each becomes one image.
    pub taken: AtomicU64,
    /// Pictures put on "video".
    pub frames: AtomicU64,
    /// Changes taken later than the tick they were seen on (the interval, a readback
    /// still in flight, or giving way). Counted once per change.
    pub coalesced: AtomicU64,
    /// Ticks on which a change waited because the track gave way (ADR-0086 decision 9).
    pub gave_way: AtomicU64,
    /// Changes not taken because the track reached [`MAX_IMAGES_PER_TRACK`].
    pub over_cap: AtomicU64,
    /// Pictures whose conversion or readback failed, and readbacks still in flight
    /// when the node was released.
    pub failed: AtomicU64,
}

/// Writes a still source's picture as images: one readback per change.
pub struct ImageTrackNode {
    descriptor: NodeDescriptor,
    /// The picture's size at the start; a larger one later grows the download once.
    width: u32,
    height: u32,
    download: Option<RgbaDownload>,
    /// The key of the picture last taken.
    last: Option<(Timestamp, u64)>,
    /// The key of a change seen but not taken yet (counted in `coalesced` once).
    waiting: Option<(Timestamp, u64)>,
    /// The tick time of the last picture taken.
    last_taken: Option<Timestamp>,
    give_way_ns: u64,
    master_drops: Option<MasterDrops>,
    seen_drops: u64,
    stats: Arc<ImageTrackStats>,
}

impl ImageTrackNode {
    /// A track for a picture of `width x height` at the start.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            descriptor: image_track_descriptor(),
            width,
            height,
            download: None,
            last: None,
            waiting: None,
            last_taken: None,
            give_way_ns: u64::MAX,
            master_drops: None,
            seen_drops: 0,
            stats: Arc::default(),
        }
    }

    /// Waits with a change while the render partition's previous tick ran longer than
    /// `budget` (ADR-0086 decision 9): the change is taken on a later tick.
    #[must_use]
    pub fn give_way_after(mut self, budget: Duration) -> Self {
        self.give_way_ns = u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX);
        self
    }

    /// Waits with a change on a tick on which `master_drops` grew (ADR-0086 decision
    /// 9). Read once per tick while a change waits; atomics only.
    #[must_use]
    pub fn yield_to(mut self, master_drops: MasterDrops) -> Self {
        self.seen_drops = master_drops();
        self.master_drops = Some(master_drops);
        self
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<ImageTrackStats> {
        Arc::clone(&self.stats)
    }

    fn gives_way(&mut self, tick: &Tick) -> bool {
        let previous_ns = u64::try_from(tick.previous_busy.as_nanos()).unwrap_or(u64::MAX);
        let mut master_dropped = false;
        if let Some(master_drops) = &self.master_drops {
            let drops = master_drops();
            master_dropped = drops > self.seen_drops;
            self.seen_drops = drops;
        }
        previous_ns > self.give_way_ns || master_dropped
    }
}

impl Node<GpuRuntime> for ImageTrackNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let download = RgbaDownload::new(ctx.shared, self.width, self.height)
            .map_err(|e| NodeError::Failed(format!("image track: {e}")))?;
        self.download = Some(download);
        self.last = None;
        self.waiting = None;
        self.last_taken = None;
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let (inputs, outputs, local) = ctx.parts();
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;

        // A picture read back on an earlier tick goes to the writer first.
        let mut result = Ok(());
        match download.take(gpu) {
            Ok(Some(frame)) => {
                drop(outputs.set_video(VIDEO_PIN, Some(frame))?);
                self.stats.frames.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(_) => {
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                result = Err(NodeError::Backend("image track readback failed"));
            }
        }

        let Some(picture) = inputs.video(PICTURE_PIN, 0) else {
            return result;
        };
        let key = (picture.timestamp(), picture.sequence());
        // Steady state: the picture did not change. One compare.
        if self.last == Some(key) {
            return result;
        }
        if self.stats.taken.load(Ordering::Relaxed) >= MAX_IMAGES_PER_TRACK {
            self.last = Some(key);
            self.waiting = None;
            self.stats.over_cap.fetch_add(1, Ordering::Relaxed);
            return result;
        }
        let too_soon = self.last_taken.is_some_and(|at| {
            tick.time.as_nanos().saturating_sub(at.as_nanos())
                < u64::try_from(MIN_IMAGE_INTERVAL.as_nanos()).unwrap_or(u64::MAX)
        });
        let busy = download.in_flight();
        let gives_way = !too_soon && !busy && self.gives_way(&tick);
        if too_soon || busy || gives_way {
            if gives_way {
                self.stats.gave_way.fetch_add(1, Ordering::Relaxed);
            }
            if self.waiting.is_none() {
                self.stats.coalesced.fetch_add(1, Ordering::Relaxed);
            }
            // A newer change replaces the one waiting: it is written as it is when
            // taken, and counted once.
            self.waiting = Some(key);
            return result;
        }
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;
        match download.submit_at(gpu, &local.pool, picture, tick.time, tick.index) {
            Ok(true) => {
                self.stats.taken.fetch_add(1, Ordering::Relaxed);
                self.last = Some(key);
                self.waiting = None;
                self.last_taken = Some(tick.time);
            }
            // Checked above; kept for a later tick if it happens.
            Ok(false) => {}
            Err(_) => {
                // Not retried: the same picture would fail again.
                self.last = Some(key);
                self.waiting = None;
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                result = Err(NodeError::Backend("image track conversion failed"));
            }
        }
        result
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        if let Some(download) = self.download.take()
            && download.in_flight()
        {
            self.stats.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
