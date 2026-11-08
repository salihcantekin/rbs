//! The still source node: one picture on a video link, uploaded once (ADR-0074, spec
//! `docs/specs/io-nodes.md`, "Still source node").

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};

use crate::capture::CaptureNodeStats;

/// Node type id of still sources (an image file, a colour or a gradient).
pub const STILL_NODE_TYPE: &str = "org.rbs.still";
/// Index of the "video" output pin.
pub const VIDEO_PIN: usize = 0;

/// The still source's descriptor: kind `Source`, output "video".
pub fn still_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(STILL_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Why pixels are not a valid picture.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StillError {
    /// A side is zero.
    Empty,
    /// The buffer is not `width * height * 4` bytes.
    WrongLength,
}

impl std::fmt::Display for StillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StillError::Empty => f.write_str("a picture needs a width and a height"),
            StillError::WrongLength => f.write_str("pixel data is not width x height x 4 bytes"),
        }
    }
}

impl std::error::Error for StillError {}

/// Puts one picture on its "video" output.
///
/// The pixels are straight-alpha sRGB `Rgba8`, made on the control thread. `warm`
/// uploads them (not real time), the first `process` publishes the texture, and every
/// later tick does nothing: the output slot keeps the frame (P13). Nothing is held on
/// the GPU in Standby.
pub struct StillSourceNode {
    descriptor: NodeDescriptor,
    format: VideoFormat,
    pixels: Bytes,
    upload: SourceUpload,
    /// The uploaded frame, until the next `process` publishes it.
    ready: Option<VideoFrame>,
    stats: Arc<CaptureNodeStats>,
}

impl StillSourceNode {
    /// A source of `width` x `height` straight-alpha `Rgba8` pixels.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, StillError> {
        if width == 0 || height == 0 {
            return Err(StillError::Empty);
        }
        let expected = u64::from(width) * u64::from(height) * 4;
        if u64::try_from(rgba.len()).ok() != Some(expected) {
            return Err(StillError::WrongLength);
        }
        let stats = Arc::new(CaptureNodeStats::default());
        stats.picture_size.store(
            (u64::from(width) << 32) | u64::from(height),
            Ordering::Relaxed,
        );
        Ok(Self {
            descriptor: still_descriptor(),
            format: VideoFormat::new(
                Size::new(width, height),
                PixelFormat::Rgba8,
                ColorSpace::SRGB,
            ),
            pixels: Bytes::from(rgba),
            upload: SourceUpload::new(),
            ready: None,
            stats,
        })
    }

    /// The picture's size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.format.size.width, self.format.size.height)
    }

    /// The node's counters (`frames`, `uploaded`, `picture_size`).
    pub fn stats(&self) -> Arc<CaptureNodeStats> {
        Arc::clone(&self.stats)
    }
}

impl Node<GpuRuntime> for StillSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        self.ready = None;
        let planes = [PlaneLayout {
            offset: 0,
            stride: self.format.size.width as usize * 4,
        }];
        let system = SystemFrame::new(self.format, self.pixels.clone(), &planes)
            .map_err(|_| NodeError::Permanent("still picture layout invalid"))?;
        let frame = VideoFrame::system(Timestamp::ZERO, system, None);
        let uploaded = self
            .upload
            .upload(ctx.shared, &mut ctx.local.pool, frame)
            .map_err(|_| NodeError::Backend("still picture upload failed"))?;
        self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
        self.ready = Some(uploaded);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Steady state: nothing to do, the slot keeps the frame.
        let Some(frame) = self.ready.take() else {
            return Ok(());
        };
        drop(ctx.outputs().set_video(VIDEO_PIN, Some(frame))?);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.ready = None;
        self.upload.release(&mut ctx.local.pool);
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.ready = None;
        self.upload = SourceUpload::new();
    }
}
