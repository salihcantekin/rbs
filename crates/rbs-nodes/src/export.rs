//! The video export node: canvas frames to encoder-ready frames, on the render partition
//! (spec `docs/specs/io-nodes.md`, "Video export node").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rbs_core::media::{ColorSpace, Size, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_gpu::download::{DownloadStats, YuvDownload};
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::{ScaleKernel, YuvFormat};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};

/// Node type id of the video export.
pub const VIDEO_EXPORT_NODE_TYPE: &str = "org.rbs.video-export";
/// Index of the "canvas" input pin.
pub const CANVAS_PIN: usize = 0;
/// Index of the "video" output pin.
pub const VIDEO_PIN: usize = 0;

/// The video export's descriptor: kind `VideoFilter`, input "canvas", output "video".
pub fn video_export_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(VIDEO_EXPORT_NODE_TYPE),
        NodeKind::VideoFilter,
        vec![Pin::one("canvas", LinkKind::Video)],
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Counters of a video export node, readable from any thread.
#[derive(Debug, Default)]
pub struct ExportNodeStats {
    /// Frames put on the video output.
    pub frames: AtomicU64,
    /// Canvas frames not converted because every readback was in flight.
    pub busy: AtomicU64,
    /// Converted frames lost before delivery (stale, no free buffer, map failure, or
    /// still in flight when the node was released).
    pub lost: AtomicU64,
    /// Timestamp in nanoseconds plus one of the last frame put on "video"; 0 before
    /// the first (ADR-0045: the studio compares it with the encoder's).
    pub last_frame: AtomicU64,
}

/// Ends a video export's input at a master-clock time (ADR-0045); shared between the
/// node and the control thread.
///
/// Until [`ExportEnd::end_at`] is called the export takes every canvas. Afterwards it
/// takes only canvases up to that time, keeps passing on the readbacks still in flight,
/// and reports [`ExportEnd::ended`] once every canvas it took is on its output. The
/// encoder then gets the last one on its own next tick, or on its final tick.
#[derive(Debug)]
pub struct ExportEnd {
    /// Nanoseconds of the last canvas time taken; `u64::MAX` while not ending.
    at: AtomicU64,
    ended: AtomicBool,
}

impl Default for ExportEnd {
    fn default() -> Self {
        Self {
            at: AtomicU64::new(u64::MAX),
            ended: AtomicBool::new(false),
        }
    }
}

impl ExportEnd {
    /// Takes no canvas later than `at`. An earlier call with an earlier time wins.
    pub fn end_at(&self, at: Timestamp) {
        // `u64::MAX` means "open", so the latest representable end is one below it.
        self.at
            .fetch_min(at.as_nanos().min(u64::MAX - 1), Ordering::Relaxed);
    }

    /// The time asked for by [`ExportEnd::end_at`], if any.
    pub fn requested(&self) -> Option<Timestamp> {
        match self.at.load(Ordering::Relaxed) {
            u64::MAX => None,
            at => Some(Timestamp::from_nanos(at)),
        }
    }

    /// True once the export, asked to end, has put every canvas it took on its output.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// Converts the canvas on its input to NV12 or P010 for an encoder and puts the result
/// on "video", one or two ticks later.
///
/// Today the result is a system-memory frame read back without blocking
/// ([`YuvDownload`]): the encoders on Windows (NVENC, AMF) take only system memory.
/// Native GPU export (ADR-0025) replaces it per encoder once the encoder and the queue
/// between partitions can carry native frames (ADR-0038).
pub struct VideoExportNode {
    descriptor: NodeDescriptor,
    format: YuvFormat,
    /// Colour the planes are encoded with and the frames are tagged with.
    color: ColorSpace,
    /// Raises only the darkest tones before the Rec. 709 encode. The tag stays `color`.
    open_shadows: bool,
    /// Canvas the input frames are.
    canvas_width: u32,
    canvas_height: u32,
    /// Encoded picture. Equal to the canvas: the 1:1 download. Otherwise one scale.
    width: u32,
    height: u32,
    download: Option<YuvDownload>,
    last: Option<(Timestamp, u64)>,
    seen: DownloadStats,
    stats: Arc<ExportNodeStats>,
    end: Arc<ExportEnd>,
}

impl VideoExportNode {
    /// An export of `canvas_width x canvas_height` canvases to `width x height`
    /// pictures (all even) in `format`, Rec. 709 limited range.
    ///
    /// Equal sizes are the 1:1 download. A different size scales once from the canvas
    /// (ADR-0102).
    pub fn new(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self::with_color(
            format,
            canvas_width,
            canvas_height,
            width,
            height,
            ColorSpace::BT709_LIMITED,
        )
    }

    /// An export that encodes and tags frames as `color`.
    ///
    /// `YuvDownload` accepts [`ColorSpace::BT709_LIMITED`] (the default) and
    /// [`ColorSpace::SRGB_VIDEO`] (sRGB, full range). Anything else fails in `prepare`.
    /// `width x height` is the encoded picture. Equal to the canvas: 1:1.
    pub fn with_color(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
        color: ColorSpace,
    ) -> Self {
        Self::with_tone(
            format,
            canvas_width,
            canvas_height,
            width,
            height,
            color,
            false,
        )
    }

    /// [`Self::with_color`] with the open-shadows grade. The frames stay tagged `color`.
    /// The grade applies only to Rec. 709 limited range and fails in `prepare` otherwise.
    #[allow(clippy::too_many_arguments)]
    pub fn with_tone(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Self {
        Self {
            descriptor: video_export_descriptor(),
            format,
            color,
            open_shadows,
            canvas_width,
            canvas_height,
            width,
            height,
            download: None,
            last: None,
            seen: DownloadStats::default(),
            stats: Arc::default(),
            end: Arc::default(),
        }
    }

    /// The format of the frames on "video": the encoder's input format (the output size).
    pub fn frame_format(&self) -> VideoFormat {
        VideoFormat::new(
            Size::new(self.width, self.height),
            self.format.pixel_format(),
            self.color,
        )
    }

    /// The kernel [`Self::prepare`] builds.
    ///
    /// Before prepare this is [`rbs_gpu::yuv::scale_kernel`] of the canvas and the
    /// output. After, it is the download's converter (`YuvConverter::kernel`).
    pub fn kernel(&self) -> ScaleKernel {
        match &self.download {
            Some(download) => download.kernel(),
            None => rbs_gpu::yuv::scale_kernel(
                self.canvas_width,
                self.canvas_height,
                self.width,
                self.height,
            ),
        }
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<ExportNodeStats> {
        Arc::clone(&self.stats)
    }

    /// The handle that ends the export's input (ADR-0045).
    pub fn end(&self) -> Arc<ExportEnd> {
        Arc::clone(&self.end)
    }
}

impl Node<GpuRuntime> for VideoExportNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Equal size: the 1:1 `textureLoad` path. A different size: one scaled
        // download from this canvas (ADR-0102). The two outputs of one canvas each
        // read that canvas; neither reads the other's planes.
        let same = self.canvas_width == self.width && self.canvas_height == self.height;
        let download = if same {
            YuvDownload::with_tone(
                ctx.shared,
                self.format,
                self.width,
                self.height,
                self.color,
                self.open_shadows,
            )
        } else {
            YuvDownload::scaled_tone(
                ctx.shared,
                self.format,
                self.canvas_width,
                self.canvas_height,
                self.width,
                self.height,
                rbs_gpu::download::DOWNLOAD_BUFFERS,
                self.color,
                self.open_shadows,
            )
        }
        .map_err(|e| NodeError::Failed(format!("video export: {e}")))?;
        self.seen = download.stats();
        self.download = Some(download);
        self.last = None;
        // A restarted export has nothing in flight; it reports the end again once it
        // has passed on what it takes from now on.
        self.end.ended.store(false, Ordering::Release);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let ctx_time = ctx.tick().time.as_nanos();
        let gpu = ctx.shared();
        let (inputs, outputs, local) = ctx.parts();
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;

        // `u64::MAX` (the run path) takes every canvas: one load and one compare.
        let end = self.end.at.load(Ordering::Relaxed);
        // Frames submitted on earlier ticks first, so their staging buffers are free.
        // Ending, every frame is passed on in order (ADR-0045); otherwise the newest.
        let ready = if end == u64::MAX {
            download.take_ready(gpu)
        } else {
            download.take_oldest(gpu)
        };
        let mut result = Ok(());
        match ready {
            Ok(Some(frame)) => {
                // Dropping the previous frame hands its buffer back for reuse.
                let at = frame.timestamp().as_nanos().saturating_add(1);
                drop(outputs.set_video(VIDEO_PIN, Some(frame))?);
                self.stats.frames.fetch_add(1, Ordering::Relaxed);
                self.stats.last_frame.store(at, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(_) => result = Err(NodeError::Backend("video export readback failed")),
        }

        if let Some(canvas) = inputs.video(CANVAS_PIN, 0) {
            let key = (canvas.timestamp(), canvas.sequence());
            if self.last != Some(key) && canvas.timestamp().as_nanos() <= end {
                self.last = Some(key);
                if download.submit(gpu, &local.pool, canvas).is_err() {
                    result = Err(NodeError::Backend("video export conversion failed"));
                }
            }
        }
        // Ending (ADR-0045): done on a tick past the end once nothing taken is still
        // being read back (a tick at or before the end may still bring a canvas).
        // Readbacks that fail, are busy or find no buffer are counted as on the run path.
        if end != u64::MAX && ctx_time > end && download.in_flight() == 0 && !self.end.ended() {
            self.end.ended.store(true, Ordering::Release);
        }

        let now = download.stats();
        let lost = (now.stale - self.seen.stale)
            + (now.no_buffer - self.seen.no_buffer)
            + (now.failed - self.seen.failed);
        self.stats
            .busy
            .fetch_add(now.busy - self.seen.busy, Ordering::Relaxed);
        self.stats.lost.fetch_add(lost, Ordering::Relaxed);
        self.seen = now;
        result
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        // Readbacks still in flight never reach the encoder (a stop without
        // `ExportEnd`, or past the studio's tail limit): counted, never silent.
        if let Some(download) = self.download.take() {
            self.stats
                .lost
                .fetch_add(download.in_flight() as u64, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_tag_is_bt709_and_screen_is_srgb_video() {
        let video = VideoExportNode::new(YuvFormat::Nv12, 16, 8, 16, 8);
        assert_eq!(video.frame_format().color, ColorSpace::BT709_LIMITED);
        assert_eq!(video.frame_format().size, Size::new(16, 8));
        assert_eq!(video.kernel(), ScaleKernel::OneToOne);
        let screen =
            VideoExportNode::with_color(YuvFormat::Nv12, 16, 8, 16, 8, ColorSpace::SRGB_VIDEO);
        assert_eq!(screen.frame_format().color, ColorSpace::SRGB_VIDEO);
        assert_eq!(
            screen.frame_format().color.range,
            rbs_core::media::ColorRange::Full
        );
        let opened = VideoExportNode::with_tone(
            YuvFormat::Nv12,
            16,
            8,
            16,
            8,
            ColorSpace::BT709_LIMITED,
            true,
        );
        assert_eq!(opened.frame_format().color, ColorSpace::BT709_LIMITED);
        assert!(opened.open_shadows);
    }

    #[test]
    fn the_kernel_follows_the_canvas_and_the_output() {
        let one = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 64, 36);
        assert_eq!(one.kernel(), ScaleKernel::OneToOne);
        assert_eq!(one.frame_format().size, Size::new(64, 36));

        // Integer 2× downscale: box. A non-integer downscale, including 1080p to 720p,
        // stays bilinear. An upscale is Catmull-Rom. A different aspect is not refused
        // here; the scaled download letterboxes it.
        let half = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 32, 18);
        assert_eq!(half.kernel(), ScaleKernel::Box(2));
        assert_eq!(half.frame_format().size, Size::new(32, 18));
        let hd = VideoExportNode::new(YuvFormat::Nv12, 1920, 1080, 1280, 720);
        assert_eq!(hd.kernel(), ScaleKernel::Bilinear);
        let up = VideoExportNode::new(YuvFormat::Nv12, 1280, 720, 1920, 1080);
        assert_eq!(up.kernel(), ScaleKernel::CatmullRom);
        let bars = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 64, 64);
        assert_eq!(bars.kernel(), ScaleKernel::Bilinear);
    }
}
