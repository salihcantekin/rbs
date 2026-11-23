//! One frame of a running source for its Region preview (ADR-0176 decision 2; spec
//! `io-nodes.md`, "Source grab node").
//!
//! [`SourceGrabNode`] sits on the render partition behind a normalisation pass of the
//! source's own picture. It reads the first picture it sees back once with
//! [`RgbaDownload`], puts the frame in its [`GrabSlot`] and wakes the control thread.
//! After that every tick is one compare. The host removes the node once it took the
//! frame; nothing here runs while no Region section is open (P13).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rbs_core::media::VideoFrame;
use rbs_gpu::RgbaDownload;
use rbs_gpu::node::GpuRuntime;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{ControlWake, Node, NodeError, PrepareCtx, ProcessCtx};

/// Type id of [`SourceGrabNode`].
pub const SOURCE_GRAB_NODE_TYPE: &str = "org.rbs.source-grab";

const PICTURE_PIN: usize = 0;

/// The descriptor of [`SourceGrabNode`]: input "picture", no outputs.
pub fn source_grab_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(SOURCE_GRAB_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("picture", LinkKind::Video)],
        Vec::new(),
    )
}

/// Where a [`SourceGrabNode`] leaves its one frame, readable from any thread.
#[derive(Debug, Default)]
pub struct GrabSlot {
    frame: Mutex<Option<VideoFrame>>,
    /// The frame is in `frame` (set once, after it was stored).
    ready: AtomicBool,
    /// The readback or conversion failed; no frame comes.
    failed: AtomicBool,
}

impl GrabSlot {
    /// Takes the frame once it arrived: a `System` frame, `Rgba8`, sRGB, straight
    /// alpha, rows packed tightly. `None` before that, and after it was taken.
    pub fn take(&self) -> Option<VideoFrame> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        self.frame.lock().ok().and_then(|mut f| f.take())
    }

    /// Whether the frame arrived (taken or not).
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Whether the grab failed: no frame will come.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// Reads one picture of its input back as RGBA8, once.
pub struct SourceGrabNode {
    descriptor: NodeDescriptor,
    width: u32,
    height: u32,
    download: Option<RgbaDownload>,
    /// A readback was started.
    started: bool,
    /// The frame was stored or the grab failed: every later tick returns at once.
    done: bool,
    /// The frame read back while the slot's lock was taken; stored on a later tick.
    held: Option<VideoFrame>,
    slot: Arc<GrabSlot>,
    wake: Option<Arc<ControlWake>>,
}

impl SourceGrabNode {
    /// A grab of a `width x height` picture (the normalisation pass's size).
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            descriptor: source_grab_descriptor(),
            width,
            height,
            download: None,
            started: false,
            done: false,
            held: None,
            slot: Arc::default(),
            wake: None,
        }
    }

    /// Unparks `wake`'s thread once the frame is stored or the grab failed.
    #[must_use]
    pub fn waking(mut self, wake: Arc<ControlWake>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// The slot the frame goes to.
    pub fn slot(&self) -> Arc<GrabSlot> {
        Arc::clone(&self.slot)
    }

    fn finish(&mut self, failed: bool) {
        self.done = true;
        if failed {
            self.slot.failed.store(true, Ordering::Release);
        }
        if let Some(wake) = &self.wake {
            wake.unpark();
        }
    }

    /// Stores `frame` in the slot; keeps it for a later tick when the host holds the
    /// lock right now (never waits on the render thread).
    fn store(&mut self, frame: VideoFrame) {
        let stored = match self.slot.frame.try_lock() {
            Ok(mut slot) => {
                *slot = Some(frame);
                None
            }
            Err(_) => Some(frame),
        };
        match stored {
            None => {
                self.slot.ready.store(true, Ordering::Release);
                self.finish(false);
            }
            held => self.held = held,
        }
    }
}

impl Node<GpuRuntime> for SourceGrabNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let download = RgbaDownload::new(ctx.shared, self.width, self.height)
            .map_err(|e| NodeError::Failed(format!("source grab: {e}")))?;
        self.download = Some(download);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Steady state after the one frame: one compare.
        if self.done {
            return Ok(());
        }
        if let Some(frame) = self.held.take() {
            self.store(frame);
            return Ok(());
        }
        let gpu = ctx.shared();
        let (inputs, _, local) = ctx.parts();
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;
        if self.started {
            return match download.take(gpu) {
                Ok(Some(frame)) => {
                    self.store(frame);
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(_) => {
                    self.finish(true);
                    Err(NodeError::Backend("source grab readback failed"))
                }
            };
        }
        let Some(picture) = inputs.video(PICTURE_PIN, 0) else {
            return Ok(());
        };
        let (timestamp, sequence) = (picture.timestamp(), picture.sequence());
        match download.submit_at(gpu, &local.pool, picture, timestamp, sequence) {
            Ok(true) => self.started = true,
            Ok(false) => {}
            Err(_) => {
                self.finish(true);
                return Err(NodeError::Backend("source grab conversion failed"));
            }
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.download = None;
        self.held = None;
    }
}
