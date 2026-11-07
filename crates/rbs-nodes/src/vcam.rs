//! The virtual camera node: the program canvas, scaled to the camera's size and rate, to
//! a virtual camera session, on the render partition (ADR-0066 decision 4; spec
//! `docs/specs/io-nodes.md`, "Virtual camera node").

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::PixelFormat;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::YuvFormat;
use rbs_gpu::download::{DownloadStats, YuvDownload};
use rbs_gpu::node::GpuRuntime;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};
use rbs_vcam::{SlotToken, VirtualCameraConfig, VirtualCameraSession};

/// Node type id of the virtual camera.
pub const VIRTUAL_CAMERA_NODE_TYPE: &str = "org.rbs.virtual-camera";
/// Index of the "canvas" input pin.
pub const CANVAS_PIN: usize = 0;

/// Slots of a session's hand-off (spec `virtual-camera.md`, "Session").
const SESSION_SLOTS: usize = 3;

/// Frame buffers of the node's download (ADR-0066 decision 3): one the node holds between
/// `take_ready` and `send`, the three session slots, and the one the sender thread copies
/// from. Readbacks in flight hold reserved slots, so they are within this count.
pub const CAMERA_BUFFERS: usize = 1 + SESSION_SLOTS + 1;

/// The virtual camera's descriptor: kind `Output`, input "canvas", no outputs.
pub fn virtual_camera_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(VIRTUAL_CAMERA_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("canvas", LinkKind::Video)],
        Vec::new(),
    )
}

/// Counters of a virtual camera node, readable from any thread.
#[derive(Debug, Default)]
pub struct VirtualCameraNodeStats {
    /// Camera frames handed to the session.
    pub sent: AtomicU64,
    /// Camera frames due but skipped before conversion because no session slot was
    /// free (the consumer is behind).
    pub consumer_busy: AtomicU64,
    /// Camera frames due but skipped because every readback was in flight.
    pub busy: AtomicU64,
    /// Converted camera frames lost before `send` (stale, no free buffer, a failed
    /// readback, or still in flight when the node was released).
    pub lost: AtomicU64,
    /// Camera frames the session refused (`VcamError::InvalidFrame` and the like).
    pub refused: AtomicU64,
    /// Conversions that failed on the GPU.
    pub failed: AtomicU64,
}

/// Sends the canvas on its input to a virtual camera: at the camera's rate (or the
/// canvas rate when lower), scaled into the camera's size with the aspect ratio kept,
/// as NV12 in system memory.
///
/// The node owns the session. It is not essential: nothing the camera does fails a tick.
/// The studio drops the node, and with it the session (whose drop joins the adapter's
/// sender thread), on the control thread (ADR-0066 decision 6).
pub struct VirtualCameraNode {
    descriptor: NodeDescriptor,
    session: Box<dyn VirtualCameraSession>,
    config: VirtualCameraConfig,
    canvas_width: u32,
    canvas_height: u32,
    download: Option<YuvDownload>,
    /// Reserved slots of the readbacks in flight, oldest first (at most
    /// [`SESSION_SLOTS`]; allocated once in `new`).
    tokens: VecDeque<SlotToken>,
    /// The camera's cadence, from the first canvas taken after `prepare`.
    cadence: Option<Cadence>,
    /// The first camera frame index not yet due.
    next: u64,
    seen: DownloadStats,
    stats: Arc<VirtualCameraNodeStats>,
}

impl VirtualCameraNode {
    /// A camera fed from `canvas_width x canvas_height` canvases (both even) through
    /// `session`, opened with `config` (NV12, system memory).
    pub fn new(
        session: Box<dyn VirtualCameraSession>,
        config: VirtualCameraConfig,
        canvas_width: u32,
        canvas_height: u32,
    ) -> Self {
        Self {
            descriptor: virtual_camera_descriptor(),
            session,
            config,
            canvas_width,
            canvas_height,
            download: None,
            tokens: VecDeque::with_capacity(SESSION_SLOTS),
            cadence: None,
            next: 0,
            seen: DownloadStats::default(),
            stats: Arc::default(),
        }
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<VirtualCameraNodeStats> {
        Arc::clone(&self.stats)
    }

    /// The session's counters (`replaced`, `consumer_absent`, OS errors, ...).
    pub fn session_stats(&self) -> rbs_vcam::VcamStats {
        self.session.stats()
    }
}

/// Whether a camera frame is due at `t` on the camera's cadence (started at the first
/// canvas taken); advances `next` past it when it is. Cadence 60 to 30 fps takes every
/// other canvas; a canvas slower than the camera is taken every time.
fn due(cadence: &mut Option<Cadence>, next: &mut u64, rate: FrameRate, t: Timestamp) -> bool {
    let cadence = *cadence.get_or_insert_with(|| Cadence::frames(t, rate));
    match cadence.index_at_or_before(t) {
        Some(i) if i >= *next => {
            *next = i + 1;
            true
        }
        _ => false,
    }
}

impl Node<GpuRuntime> for VirtualCameraNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if self.config.format.pixel != PixelFormat::Nv12 {
            return Err(NodeError::Failed(
                "virtual camera: only NV12 is supported".into(),
            ));
        }
        let f = &self.config.format;
        let download = YuvDownload::scaled(
            ctx.shared,
            YuvFormat::Nv12,
            self.canvas_width,
            self.canvas_height,
            f.width,
            f.height,
            CAMERA_BUFFERS,
        )
        .map_err(|e| NodeError::Failed(format!("virtual camera: {e}")))?;
        self.seen = download.stats();
        self.download = Some(download);
        self.tokens.clear();
        self.cadence = None;
        self.next = 0;
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let (inputs, _outputs, local) = ctx.parts();
        let Some(download) = self.download.as_mut() else {
            return Err(NodeError::NotReady);
        };

        // Completed readbacks first, so their staging buffers and slots are free. Every
        // submission that ends (delivered, stale, no buffer, failed) gives its token back
        // in submission order; only the delivered one, the newest, uses it.
        let ready = download.take_ready(gpu);
        let now = download.stats();
        let ended = (now.delivered - self.seen.delivered)
            + (now.stale - self.seen.stale)
            + (now.no_buffer - self.seen.no_buffer)
            + (now.failed - self.seen.failed);
        let delivered = now.delivered - self.seen.delivered;
        for _ in 0..ended.saturating_sub(delivered) {
            // Dropping a token frees its slot.
            drop(self.tokens.pop_front());
        }
        match ready {
            Ok(Some(frame)) => match self.tokens.pop_front() {
                Some(slot) => match self.session.send(slot, frame) {
                    Ok(()) => {
                        self.stats.sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        self.stats.refused.fetch_add(1, Ordering::Relaxed);
                    }
                },
                // Unreachable while every submission holds a token; counted, never a
                // panic on the render thread.
                None => {
                    self.stats.lost.fetch_add(1, Ordering::Relaxed);
                }
            },
            Ok(None) => {}
            // A failed readback is counted in `failed` below and frees its token above.
            Err(_) => {}
        }

        if let Some(canvas) = inputs.video(CANVAS_PIN, 0)
            && due(
                &mut self.cadence,
                &mut self.next,
                self.config.format.rate,
                canvas.timestamp(),
            )
        {
            // Reserve before any GPU work: a full session skips the conversion.
            if self.tokens.len() >= SESSION_SLOTS {
                self.stats.consumer_busy.fetch_add(1, Ordering::Relaxed);
            } else if let Some(slot) = self.session.reserve() {
                match download.submit(gpu, &local.pool, canvas) {
                    Ok(true) => self.tokens.push_back(slot),
                    // The staging ring is full (counted in `busy` below), or the
                    // conversion failed: the token is dropped and frees its slot.
                    Ok(false) => drop(slot),
                    Err(_) => {
                        drop(slot);
                        self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                self.stats.consumer_busy.fetch_add(1, Ordering::Relaxed);
            }
        }

        let now = download.stats();
        self.stats
            .busy
            .fetch_add(now.busy - self.seen.busy, Ordering::Relaxed);
        self.stats.lost.fetch_add(
            (now.stale - self.seen.stale)
                + (now.no_buffer - self.seen.no_buffer)
                + (now.failed - self.seen.failed),
            Ordering::Relaxed,
        );
        self.seen = now;
        // The camera never fails a tick (ADR-0066 decision 4).
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        if let Some(download) = self.download.take() {
            self.stats
                .lost
                .fetch_add(download.in_flight() as u64, Ordering::Relaxed);
        }
        self.tokens.clear();
    }
}
