//! The slide show source node (ADR-0171 decisions 7 and 8, spec `docs/specs/io-nodes.md`,
//! "Slide show source node"): pictures fitted on the decode thread, uploaded in place
//! into two reserved textures, and a change drawn by the slide pass only while it runs.
//!
//! Hot path (`docs/architecture/hot-paths.md`): between changes `process` is a few
//! atomic loads and a time compare. A new picture costs one texture write under the
//! upload budget, ahead of its change. A change costs one pass per tick for its length.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, TextureId, VideoFormat,
    next_generation,
};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::ReservedUpload;
use rbs_gpu::{ReservedTargets, SlideKind, SlidePass};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};

/// Node type id of slide show sources.
pub const SLIDESHOW_NODE_TYPE: &str = "org.rbs.slideshow";

/// The slide show source's descriptor: kind `Source`, output "video".
pub fn slideshow_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(SLIDESHOW_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// What the host, the node and the decode thread share (ADR-0171 decision 8): atomics
/// only. The host writes the live keys and `paused`; the node writes `shown`.
#[derive(Debug, Default)]
pub struct SlideshowShared {
    /// Set by the node from `warm` to `cool`: the decode thread works only for warm
    /// sources and keeps no pictures for others (ADR-0171 decision 6).
    pub warm: AtomicBool,
    /// The timer stops; a picture sent with `now` still shows.
    pub paused: AtomicBool,
    /// Milliseconds each picture shows; 0 for `advance = manual`.
    pub interval_ms: AtomicU64,
    /// Milliseconds of a change, before the half-interval cap.
    pub transition_ms: AtomicU32,
    /// Serial of the picture on the output, 0 before the first.
    pub shown: AtomicU64,
    /// When the picture on the output appeared, in master-clock nanoseconds.
    pub shown_at_ns: AtomicU64,
    /// Counters.
    pub stats: SlideshowStats,
}

impl SlideshowShared {
    /// The interval, `None` for manual.
    pub fn interval(&self) -> Option<Duration> {
        match self.interval_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// The length of a change: `transition_ms`, at most half the interval (decision 4).
    pub fn change_length(&self) -> Duration {
        let t = Duration::from_millis(u64::from(self.transition_ms.load(Ordering::Relaxed)));
        match self.interval() {
            Some(i) => t.min(i / 2),
            None => t,
        }
    }
}

/// Counters of a slide show node, read by the host.
#[derive(Debug, Default)]
pub struct SlideshowStats {
    /// Frames put on the video output.
    pub frames: AtomicU64,
    /// Pictures uploaded.
    pub uploaded: AtomicU64,
    /// Changes drawn with the slide pass (finished).
    pub changes: AtomicU64,
    /// Ticks a picture waited for the partition's upload budget.
    pub budget_deferred: AtomicU64,
    /// Ticks a picture waited because both picture textures were read.
    pub upload_deferred: AtomicU64,
    /// Change ticks skipped because both targets were read.
    pub draw_skipped: AtomicU64,
    /// Failed reservations or GPU writes.
    pub gpu_errors: AtomicU64,
}

/// One fitted picture on its way to the node. Built on the decode thread around shared
/// pixels, so the render thread neither copies nor frees them: a displaced frame goes
/// back to the sender through the parameter channel and drops on the decode thread.
#[derive(Debug, Clone)]
pub struct SlideFrame {
    /// Increases with every picture the decode thread sends; never 0.
    pub serial: u64,
    /// Show at once (a command, the first picture, a live `fit` change).
    pub now: bool,
    /// Show without a change pass (the first picture, a live `fit` change).
    pub cut: bool,
    system: SystemFrame,
}

/// The pixels of a shared picture, for a `Bytes` that does not copy them.
struct SharedPixels(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedPixels {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl SlideFrame {
    /// Wraps `rgba` (straight-alpha sRGB, `size`) for the node. Not real time. `None`
    /// when the buffer does not match the size.
    pub fn new(
        serial: u64,
        now: bool,
        cut: bool,
        size: (u32, u32),
        rgba: Arc<Vec<u8>>,
    ) -> Option<Self> {
        let format = VideoFormat::new(
            Size::new(size.0, size.1),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        if rgba.len() != size.0 as usize * size.1 as usize * 4 || serial == 0 {
            return None;
        }
        let planes = [PlaneLayout {
            offset: 0,
            stride: size.0 as usize * 4,
        }];
        let system =
            SystemFrame::new(format, Bytes::from_owner(SharedPixels(rgba)), &planes).ok()?;
        Some(Self {
            serial,
            now,
            cut,
            system,
        })
    }

    fn size(&self) -> (u32, u32) {
        let s = self.system.format().size;
        (s.width, s.height)
    }

    fn bytes(&self) -> u64 {
        let (w, h) = self.size();
        u64::from(w) * u64::from(h) * 4
    }
}

/// A change being drawn.
#[derive(Debug, Clone, Copy)]
struct Change {
    from: TextureId,
    to: TextureId,
    to_serial: u64,
    start: Timestamp,
    length: Duration,
}

/// The pass and targets of a slide show whose transition is not a cut.
struct ChangeGpu {
    pass: SlidePass,
    targets: ReservedTargets,
}

/// Shows a slide show's pictures on its "video" output.
pub struct SlideshowNode {
    descriptor: NodeDescriptor,
    size: (u32, u32),
    kind: Option<SlideKind>,
    frames: ParamReceiver<Option<SlideFrame>>,
    shared: Arc<SlideshowShared>,
    textures: Option<ReservedUpload>,
    gpu: Option<ChangeGpu>,
    /// The texture and serial on the output.
    current: Option<(TextureId, u64)>,
    /// The next picture, already uploaded: its texture and serial.
    incoming: Option<(TextureId, u64)>,
    /// The tick the waiting picture first asked the budget on.
    waiting_since: Option<u64>,
    change: Option<Change>,
    /// When the timer changes the picture next.
    next_at: Option<Timestamp>,
}

impl SlideshowNode {
    /// A node of output `size`; `kind` is `None` for a cut. Returns the sender the decode
    /// thread keeps.
    pub fn new(
        size: (u32, u32),
        kind: Option<SlideKind>,
        shared: Arc<SlideshowShared>,
        initial: Option<SlideFrame>,
    ) -> (Self, ParamSender<Option<SlideFrame>>) {
        let (tx, rx) = channel(initial);
        let node = Self {
            descriptor: slideshow_descriptor(),
            size,
            kind,
            frames: rx,
            shared,
            textures: None,
            gpu: None,
            current: None,
            incoming: None,
            waiting_since: None,
            change: None,
            next_at: None,
        };
        (node, tx)
    }

    /// What the node shares with the host and the decode thread.
    pub fn shared(&self) -> Arc<SlideshowShared> {
        Arc::clone(&self.shared)
    }

    fn format(&self) -> VideoFormat {
        VideoFormat::new(
            Size::new(self.size.0, self.size.1),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        )
    }

    /// Makes `to` the picture on the output, at `time`.
    fn shown(&mut self, to: (TextureId, u64), time: Timestamp) {
        self.current = Some(to);
        self.shared.shown.store(to.1, Ordering::Release);
        self.shared
            .shown_at_ns
            .store(time.as_nanos(), Ordering::Relaxed);
        self.next_at = self.shared.interval().map(|i| time + i);
    }
}

fn texture_of(frame: &rbs_core::media::VideoFrame) -> Option<TextureId> {
    match *frame.payload() {
        FramePayload::Texture(id) => Some(id),
        _ => None,
    }
}

impl Node<GpuRuntime> for SlideshowNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let (w, h) = self.size;
        let textures =
            ReservedUpload::reserve(ctx.shared, &mut ctx.local.pool, w, h, PixelFormat::Rgba8)
                .map_err(|_| NodeError::Backend("slide show textures"))?;
        self.textures = Some(textures);
        if self.kind.is_some() {
            let targets = ReservedTargets::reserve(ctx.shared, &mut ctx.local.pool, w, h)
                .map_err(|_| NodeError::Backend("slide show targets"))?;
            self.gpu = Some(ChangeGpu {
                pass: SlidePass::new(ctx.shared),
                targets,
            });
        }
        // A picture kept across Standby shows again, without a change (decision 6).
        self.current = None;
        self.incoming = None;
        self.change = None;
        self.shared.warm.store(true, Ordering::Release);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        // 1. A change in progress: draw it, or end it.
        if let Some(change) = self.change {
            let elapsed = tick.time.saturating_since(change.start);
            if elapsed >= change.length {
                self.change = None;
                let frame = ctx.local().pool.frame(change.to, self.format(), tick.time);
                if let Ok(frame) = frame {
                    drop(
                        ctx.outputs()
                            .set_video(0, Some(frame.with_sequence(next_generation())))?,
                    );
                    self.shared.stats.frames.fetch_add(1, Ordering::Relaxed);
                    self.shared.stats.changes.fetch_add(1, Ordering::Relaxed);
                }
                self.current = Some((change.to, change.to_serial));
                return Ok(());
            }
            let t = elapsed.as_secs_f32() / change.length.as_secs_f32().max(f32::EPSILON);
            let shared = ctx.shared();
            let (Some(gpu), Some(kind)) = (self.gpu.as_ref(), self.kind) else {
                return Ok(());
            };
            match gpu.pass.draw(
                shared,
                &ctx.local().pool,
                &gpu.targets,
                change.from,
                change.to,
                kind,
                t,
                tick.time,
            ) {
                Ok(Some(frame)) => {
                    drop(
                        ctx.outputs()
                            .set_video(0, Some(frame.with_sequence(next_generation())))?,
                    );
                    self.shared.stats.frames.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {
                    self.shared
                        .stats
                        .draw_skipped
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.shared.stats.gpu_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            return Ok(());
        }

        // 2. The timer: paused or manual holds; paused restarts the interval on resume.
        if self.shared.paused.load(Ordering::Relaxed) {
            self.next_at = self.shared.interval().map(|i| tick.time + i);
        }

        // 3. A new picture: upload it ahead of its change.
        self.frames.update();
        let pending = self.frames.get().as_ref().and_then(|f| {
            let known = self.current.map(|c| c.1) == Some(f.serial)
                || self.incoming.map(|c| c.1) == Some(f.serial);
            (!known).then_some((f.serial, f.bytes(), f.size()))
        });
        if let Some((serial, bytes, size)) = pending {
            if size != self.size {
                return Ok(());
            }
            if !ctx
                .local()
                .upload_budget
                .try_take(tick.index, bytes, self.waiting_since)
            {
                self.waiting_since.get_or_insert(tick.index);
                self.shared
                    .stats
                    .budget_deferred
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let shared = ctx.shared();
            let (Some(textures), Some(frame)) = (self.textures.as_mut(), self.frames.get()) else {
                return Ok(());
            };
            match textures.upload_into(shared, &mut ctx.local().pool, &frame.system, tick.time) {
                Ok(Some(uploaded)) => {
                    self.waiting_since = None;
                    self.shared.stats.uploaded.fetch_add(1, Ordering::Relaxed);
                    // Only the id is kept; the output slot's lease is what protects the
                    // picture on screen (ADR-0171 decision 7).
                    self.incoming = texture_of(&uploaded).map(|id| (id, serial));
                }
                Ok(None) => {
                    self.shared
                        .stats
                        .upload_deferred
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(_) => {
                    self.shared.stats.gpu_errors.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }

        // 4. Change to the uploaded picture when it is due.
        let Some(incoming) = self.incoming else {
            return Ok(());
        };
        let (now, cut) = self
            .frames
            .get()
            .as_ref()
            .filter(|f| f.serial == incoming.1)
            .map_or((false, false), |f| (f.now, f.cut));
        let timed = !self.shared.paused.load(Ordering::Relaxed)
            && self.next_at.is_some_and(|at| tick.time >= at);
        let first = self.current.is_none();
        if !(now || timed || first) {
            return Ok(());
        }
        self.incoming = None;
        let length = self.shared.change_length();
        match self.current {
            Some((from, _)) if !cut && self.gpu.is_some() && !length.is_zero() => {
                self.change = Some(Change {
                    from,
                    to: incoming.0,
                    to_serial: incoming.1,
                    start: tick.time,
                    length,
                });
                // The interval counts from the start of the change; the output still
                // shows `from` until the first draw (next tick at the latest).
                self.shared.shown.store(incoming.1, Ordering::Release);
                self.shared
                    .shown_at_ns
                    .store(tick.time.as_nanos(), Ordering::Relaxed);
                self.next_at = self.shared.interval().map(|i| tick.time + i);
            }
            _ => {
                let frame = ctx.local().pool.frame(incoming.0, self.format(), tick.time);
                if let Ok(frame) = frame {
                    drop(
                        ctx.outputs()
                            .set_video(0, Some(frame.with_sequence(next_generation())))?,
                    );
                    self.shared.stats.frames.fetch_add(1, Ordering::Relaxed);
                }
                self.shown(incoming, tick.time);
            }
        }
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.shared.warm.store(false, Ordering::Release);
        if let Some(t) = self.textures.take() {
            t.give_back(&mut ctx.local.pool);
        }
        if let Some(gpu) = self.gpu.take() {
            gpu.targets.give_back(&mut ctx.local.pool);
        }
        self.current = None;
        self.incoming = None;
        self.change = None;
        self.waiting_since = None;
        self.next_at = None;
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.shared.warm.store(false, Ordering::Release);
        self.textures = None;
        self.gpu = None;
    }
}
