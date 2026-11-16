//! The text source node: pictures drawn off the render thread, uploaded in place into
//! two reserved textures (ADR-0129 decision 5, spec `docs/specs/io-nodes.md`, "Text
//! source node").
//!
//! Hot path (`docs/architecture/hot-paths.md`): `process` with nothing new is one
//! atomic load and a flag test. A new picture costs one texture write and, only after a
//! box size change, one release and reserve.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use rbs_core::fault::SourceFault;
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    next_generation,
};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::ReservedUpload;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};
use rbs_text::{MAX_PIXELS, MAX_SIDE, TextPicture};

/// Node type id of text sources.
pub const TEXT_NODE_TYPE: &str = "org.rbs.text";

/// How long a failing texture reservation is retried before it becomes a fault.
pub const RESERVE_FAULT_AFTER: Duration = Duration::from_secs(5);

/// The text source's descriptor: kind `Source`, output "video".
pub fn text_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(TEXT_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// One drawn picture on its way to a text node.
///
/// Built on the text worker: the system frame that the upload reads shares the
/// picture's pixels, so the render thread neither copies nor allocates.
#[derive(Debug, Clone)]
pub struct TextFrame {
    /// Number of the render request this picture answers (edits, files and clock ticks).
    pub request: u64,
    /// The settings generation it was drawn from.
    pub settings_generation: u64,
    /// The picture, shared with the worker's cache of the source's last frame.
    pub picture: Arc<TextPicture>,
    system: SystemFrame,
}

/// The pixels of a shared picture, for a `Bytes` that does not copy them.
struct PictureBytes(Arc<TextPicture>);

impl AsRef<[u8]> for PictureBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.rgba()
    }
}

impl TextFrame {
    /// Wraps `picture` for the node. Not real time (allocates the shared handle).
    /// `None` only if the picture's buffer does not match its size, which
    /// `TextPicture`'s constructors rule out.
    pub fn new(request: u64, settings_generation: u64, picture: Arc<TextPicture>) -> Option<Self> {
        let (width, height) = (picture.width(), picture.height());
        let format = VideoFormat::new(
            Size::new(width, height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        let planes = [PlaneLayout {
            offset: 0,
            stride: width as usize * 4,
        }];
        let data = Bytes::from_owner(PictureBytes(Arc::clone(&picture)));
        let system = SystemFrame::new(format, data, &planes).ok()?;
        Some(Self {
            request,
            settings_generation,
            picture,
            system,
        })
    }

    fn bytes(&self) -> u64 {
        u64::from(self.picture.width()) * u64::from(self.picture.height()) * 4
    }

    fn within_caps(&self) -> bool {
        let (w, h) = (self.picture.width(), self.picture.height());
        w <= MAX_SIDE && h <= MAX_SIDE && u64::from(w) * u64::from(h) <= MAX_PIXELS
    }
}

/// Counters of a text node, read by the host.
#[derive(Debug, Default)]
pub struct TextNodeStats {
    /// Frames put on the video output.
    pub frames: AtomicU64,
    /// Pictures uploaded.
    pub uploaded: AtomicU64,
    /// The `request` of the last uploaded picture; the host waits on it (decision 7).
    pub uploaded_request: AtomicU64,
    /// Ticks a picture waited because both textures were still read
    /// (`text_upload_deferred`).
    pub upload_deferred: AtomicU64,
    /// Ticks a picture waited for the partition's upload budget.
    pub budget_deferred: AtomicU64,
    /// Pictures over the size caps, held without upload.
    pub over_cap: AtomicU64,
    /// Failed texture reservations.
    pub reserve_failures: AtomicU64,
    /// Failed texture writes.
    pub gpu_errors: AtomicU64,
    /// Width in the high 32 bits, height in the low 32 bits of the uploaded picture.
    pub picture_size: AtomicU64,
    /// [`SourceFault::code`] of a persisting failure; 0 while it runs.
    pub fault: AtomicU8,
}

impl TextNodeStats {
    /// Width and height of the picture on the output, once one was uploaded.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        let v = self.picture_size.load(Ordering::Relaxed);
        (v != 0).then_some(((v >> 32) as u32, v as u32))
    }

    /// The fault the node reports, if any.
    pub fn fault(&self) -> Option<SourceFault> {
        SourceFault::from_code(self.fault.load(Ordering::Relaxed))
    }
}

/// Puts the newest text picture on its "video" output.
///
/// The worker owns the only [`ParamSender`] of the node's channel. Two box-sized
/// textures stay acquired from `warm` to `cool`; each new picture goes into the one no
/// reader leases, under the partition's upload budget (`rbs_gpu::budget`).
pub struct TextSourceNode {
    descriptor: NodeDescriptor,
    frames: ParamReceiver<Option<TextFrame>>,
    textures: Option<ReservedUpload>,
    /// The channel's picture is not on the output yet.
    pending: bool,
    /// The tick the pending picture first waited on for the budget.
    waiting_since: Option<u64>,
    /// After a failed reservation: the first failure's time and when to try again.
    reserve_failed: Option<(Timestamp, Timestamp)>,
    /// A frame `warm` uploaded, published by the next `process`.
    ready: Option<VideoFrame>,
    stats: Arc<TextNodeStats>,
}

impl TextSourceNode {
    /// A node showing `initial` (the worker's cached last frame, or `None`) and the
    /// sender its worker keeps.
    pub fn new(initial: Option<TextFrame>) -> (Self, ParamSender<Option<TextFrame>>) {
        let pending = initial.is_some();
        let (tx, rx) = channel(initial);
        let node = Self {
            descriptor: text_descriptor(),
            frames: rx,
            textures: None,
            pending,
            waiting_since: None,
            reserve_failed: None,
            ready: None,
            stats: Arc::new(TextNodeStats::default()),
        };
        (node, tx)
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<TextNodeStats> {
        Arc::clone(&self.stats)
    }

    /// Makes sure two textures of the frame's size are reserved. Not real time on a
    /// size change (ADR-0062 decision 5).
    fn ensure_textures(
        &mut self,
        shared: &<GpuRuntime as rbs_graph::runtime::Runtime>::Shared,
        local: &mut <GpuRuntime as rbs_graph::runtime::Runtime>::Local,
        size: (u32, u32),
        now: Timestamp,
    ) -> bool {
        if self.textures.as_ref().is_some_and(|t| t.size() == size) {
            return true;
        }
        if let Some(old) = self.textures.take() {
            old.give_back(&mut local.pool);
        }
        match ReservedUpload::reserve(shared, &mut local.pool, size.0, size.1, PixelFormat::Rgba8) {
            Ok(t) => {
                self.textures = Some(t);
                self.reserve_failed = None;
                self.stats.fault.store(0, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.stats.reserve_failures.fetch_add(1, Ordering::Relaxed);
                let first = self.reserve_failed.map_or(now, |(first, _)| first);
                if now.saturating_since(first) >= RESERVE_FAULT_AFTER {
                    self.stats
                        .fault
                        .store(SourceFault::Other.code(), Ordering::Relaxed);
                }
                self.reserve_failed = Some((first, now + RESERVE_FAULT_AFTER));
                false
            }
        }
    }

    fn uploaded(&mut self, frame: VideoFrame, request: u64, size: (u32, u32)) -> VideoFrame {
        self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
        self.stats
            .uploaded_request
            .store(request, Ordering::Release);
        self.stats.picture_size.store(
            (u64::from(size.0) << 32) | u64::from(size.1),
            Ordering::Relaxed,
        );
        self.pending = false;
        self.waiting_since = None;
        // A new generation per picture, so a reader never sees the same texture and
        // time twice for different pixels (ADR-0117 decision 4).
        frame.with_sequence(next_generation())
    }
}

impl Node<GpuRuntime> for TextSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        self.ready = None;
        if self.frames.update() {
            self.pending = true;
        }
        let Some(frame) = self.frames.get() else {
            return Ok(());
        };
        if !frame.within_caps() {
            self.stats.over_cap.fetch_add(1, Ordering::Relaxed);
            self.pending = false;
            return Ok(());
        }
        let (request, size) = (
            frame.request,
            (frame.picture.width(), frame.picture.height()),
        );
        if !self.ensure_textures(ctx.shared, ctx.local, size, Timestamp::ZERO) {
            // `process` tries again; the layer is not drawn until a picture is up.
            return Ok(());
        }
        let (Some(textures), Some(frame)) = (self.textures.as_mut(), self.frames.get()) else {
            return Ok(());
        };
        match textures.upload_into(
            ctx.shared,
            &mut ctx.local.pool,
            &frame.system,
            Timestamp::ZERO,
        ) {
            Ok(Some(uploaded)) => {
                let frame = self.uploaded(uploaded, request, size);
                self.ready = Some(frame);
            }
            Ok(None) => {}
            Err(_) => {
                self.stats.gpu_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        if let Some(frame) = self.ready.take() {
            drop(
                ctx.outputs()
                    .set_video(0, Some(frame.with_timestamp(tick.time)))?,
            );
            self.stats.frames.fetch_add(1, Ordering::Relaxed);
        }
        if self.frames.update() {
            self.pending = true;
            self.waiting_since = None;
            self.reserve_failed = self.reserve_failed.map(|(first, _)| (first, tick.time));
        }
        if !self.pending {
            return Ok(());
        }
        let Some(frame) = self.frames.get() else {
            self.pending = false;
            return Ok(());
        };
        if !frame.within_caps() {
            self.stats.over_cap.fetch_add(1, Ordering::Relaxed);
            self.pending = false;
            return Ok(());
        }
        if let Some((_, retry_at)) = self.reserve_failed
            && tick.time < retry_at
        {
            return Ok(());
        }
        let (request, bytes) = (frame.request, frame.bytes());
        let size = (frame.picture.width(), frame.picture.height());
        let shared = ctx.shared();
        if !self.ensure_textures(shared, ctx.local(), size, tick.time) {
            return Ok(());
        }
        if !ctx
            .local()
            .upload_budget
            .try_take(tick.index, bytes, self.waiting_since)
        {
            self.waiting_since.get_or_insert(tick.index);
            self.stats.budget_deferred.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let (Some(textures), Some(frame)) = (self.textures.as_mut(), self.frames.get()) else {
            return Ok(());
        };
        match textures.upload_into(shared, &mut ctx.local().pool, &frame.system, tick.time) {
            Ok(Some(uploaded)) => {
                let frame = self.uploaded(uploaded, request, size);
                drop(ctx.outputs().set_video(0, Some(frame))?);
                self.stats.frames.fetch_add(1, Ordering::Relaxed);
            }
            // Both textures are still read (an export's held frame): keep the picture
            // for the next tick; never a third texture, never a wait.
            Ok(None) => {
                self.stats.upload_deferred.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.stats.gpu_errors.fetch_add(1, Ordering::Relaxed);
                self.pending = false;
            }
        }
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.ready = None;
        if let Some(t) = self.textures.take() {
            t.give_back(&mut ctx.local.pool);
        }
        // `warm` uploads the current picture again.
        self.pending = self.frames.get().is_some();
        self.waiting_since = None;
        self.reserve_failed = None;
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.ready = None;
        self.textures = None;
    }
}
