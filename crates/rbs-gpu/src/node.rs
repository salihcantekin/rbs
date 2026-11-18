//! The render executor's runtime and the compositor node of the media graph
//! (spec `docs/specs/gpu-compositor.md`, "Graph runtime and compositor node"; ADR-0020).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, Size, TextureId, TransferFunction, VideoFormat,
    next_generation,
};
use rbs_core::time::Timestamp;
use rbs_graph::description::NodeId;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{self, ParamReceiver, ParamSender};
use rbs_graph::runtime::{Inputs, Node, NodeError, PrepareCtx, ProcessCtx, Runtime, Tick, WarmCtx};

use crate::compositor::{CanvasDesc, Compositor, Layer, LayerStyle, ScrollBinding, Transfer};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::Importer;
use crate::math::{Affine2, UvRect};
use crate::pool::{TextureDesc, TexturePool};
use crate::scroll::LayerScroll;

/// Node type id of the compositor (scene-model spec, "Graph binding").
pub const COMPOSITOR_NODE_TYPE: &str = "org.rbs.compositor";
/// Index of the many-link "layers" input pin.
pub const LAYERS_PIN: usize = 0;
/// Index of the "canvas" output pin.
pub const CANVAS_PIN: usize = 0;
/// Canvas textures kept on the pool's free list by `warm`.
const CANVAS_RING: usize = 3;
/// Ticks a texture description may go unused before the pool drops its free textures
/// (ADR-0062 decision 5): two seconds at 60 frames per second.
pub const POOL_IDLE_TICKS: u64 = 120;

/// The render executor's runtime: the GPU context is shared, the pool and importer are
/// owned by the render thread.
pub struct GpuRuntime;

/// Resources owned by the render executor thread.
#[derive(Default)]
pub struct RenderLocal {
    /// Textures of every node on this executor.
    pub pool: TexturePool,
    /// Native surface import for source wrappers.
    pub importer: Importer,
    /// Bytes source nodes may upload per tick (ADR-0129 decision 5). Rolled over by
    /// the nodes that use it, never by `end_tick`.
    pub upload_budget: crate::budget::UploadBudget,
}

impl Runtime for GpuRuntime {
    type Shared = GpuContext;
    type Local = RenderLocal;

    fn end_tick(shared: &GpuContext, local: &mut RenderLocal, tick: Tick) {
        local.importer.retire(shared, &mut local.pool, tick.time);
        local.pool.trim_idle(POOL_IDLE_TICKS);
        shared.poll();
    }
}

/// The compositor node's descriptor.
pub fn compositor_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(COMPOSITOR_NODE_TYPE),
        NodeKind::Compositor,
        vec![Pin::many("layers", LinkKind::Video)],
        vec![Pin::one("canvas", LinkKind::Video)],
    )
}

/// A crop in source pixels, taken from each edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelCrop {
    /// From the left edge.
    pub left: u32,
    /// From the top edge.
    pub top: u32,
    /// From the right edge.
    pub right: u32,
    /// From the bottom edge.
    pub bottom: u32,
}

/// How unit time `0..=1` is mapped for a slide or fade (ADR-0100).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Easing {
    /// Straight lerp.
    Linear,
    /// Smoothstep `u²(3−2u)`.
    #[default]
    EaseInOut,
}

impl Easing {
    /// Maps unit time `u` in `0..=1` to the interpolation weight.
    pub fn map(self, u: f64) -> f32 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => u as f32,
            Self::EaseInOut => (u * u * (3.0 - 2.0 * u)) as f32,
        }
    }
}

/// An opacity animation, evaluated by the node against tick time (ADR-0023,
/// decision 7): `from` before `start`, the layer's `opacity` after `start + duration`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fade {
    /// Opacity at and before `start`.
    pub from: f32,
    /// Engine time the fade starts.
    pub start: Timestamp,
    /// Length of the fade; zero jumps to the end value at `start`.
    pub duration: Duration,
    /// How unit time is mapped (scene-change fade-ins stay [`Easing::Linear`], ADR-0100).
    pub easing: Easing,
}

impl Fade {
    /// The opacity at `now` for a fade ending at `to`.
    pub fn at(&self, to: f32, now: Timestamp) -> f32 {
        if now <= self.start {
            return self.from;
        }
        let elapsed = now.saturating_since(self.start);
        if elapsed >= self.duration || self.duration.is_zero() {
            return to;
        }
        let u = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        let t = self.easing.map(u);
        self.from + (to - self.from) * t
    }
}

/// A move of a layer's box, evaluated against tick time the way [`Fade`] is.
/// `from` at and before `start`, the layer's `transform` after `start + duration`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slide {
    /// The box at and before `start`.
    pub from: Affine2,
    /// Engine time the move starts.
    pub start: Timestamp,
    /// Length of the move; zero jumps to the end box at `start`.
    pub duration: Duration,
    /// How unit time is mapped (ADR-0100).
    pub easing: Easing,
}

impl Slide {
    /// The box at `now` for a move ending at `to`.
    pub fn at(&self, to: Affine2, now: Timestamp) -> Affine2 {
        if now <= self.start || self.duration.is_zero() {
            return if now <= self.start { self.from } else { to };
        }
        let elapsed = now.saturating_since(self.start);
        if elapsed >= self.duration {
            return to;
        }
        let u = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        let t = self.easing.map(u);
        self.from.lerp(to, t)
    }
}

/// One layer of a compositor node, bottom first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeLayer {
    /// The graph node whose video this layer draws.
    pub producer: NodeId,
    /// Hint: index of the producer's link on the "layers" pin when the list was made.
    /// Checked against the schedule every tick; a stale hint costs one lookup.
    pub link: usize,
    /// Maps the unit square of the cropped source to canvas pixels.
    pub transform: Affine2,
    /// Crop in source pixels.
    pub crop: PixelCrop,
    /// 0.0..=1.0; the value a fade ends at.
    pub opacity: f32,
    /// Animates the opacity.
    pub fade: Option<Fade>,
    /// Animates the box from another place. Absent when the box is already `transform`.
    pub slide: Option<Slide>,
    /// Ignore the source's alpha (XRGB sources).
    pub opaque: bool,
    /// Shape and border (ADR-0053).
    pub style: LayerStyle,
    /// The box is a window onto a scrolling strip of the picture (ADR-0150), evaluated
    /// against tick time like `fade` and `slide`.
    pub scroll: Option<LayerScroll>,
}

impl NodeLayer {
    /// An uncropped, fully opaque layer of `producer` (link hint `link`) filling `rect`
    /// (x, y, w, h).
    pub fn new(producer: NodeId, link: usize, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            producer,
            link,
            transform: Affine2::rect(x, y, w, h),
            crop: PixelCrop::default(),
            opacity: 1.0,
            fade: None,
            slide: None,
            opaque: false,
            style: LayerStyle::default(),
            scroll: None,
        }
    }
}

/// The control-thread end of a compositor node's layer list.
pub type LayerSender = ParamSender<Vec<NodeLayer>>;

/// The control-thread end of a compositor node's scroll records (ADR-0150 decision 7):
/// send a [`ScrollBinding`] before the first list with a scrolling layer. Idempotent; a
/// skipped update only leaves a scrolling layer blank for a tick.
pub type ScrollSender = ParamSender<Option<Arc<ScrollBinding>>>;

/// The link of `layer` on the layers pin: the hint if it still names the producer,
/// otherwise a lookup.
fn link_of(inputs: &Inputs<'_>, layer: &NodeLayer) -> Option<usize> {
    if inputs.source(LAYERS_PIN, layer.link) == Some(layer.producer) {
        Some(layer.link)
    } else {
        inputs.find(LAYERS_PIN, layer.producer)
    }
}

/// Whether a compositor reports when it drew every layer (ADR-0128 decision 7). A type
/// parameter, so a compositor without a counter compiles the bookkeeping away and does
/// no more work per tick than before (P13).
pub trait ReadySignal: Send + 'static {
    /// False for [`NoReady`]: `process` keeps no bookkeeping at all.
    const TRACKS: bool;
    /// Called after a tick at which every linked layer had a texture frame.
    fn mark(&self, tick: u64);
    /// The same signal (shares the counter).
    fn clone_signal(&self) -> Self;
}

/// No ready counter: every compositor except a transition's incoming one.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoReady;

impl ReadySignal for NoReady {
    const TRACKS: bool = false;
    #[inline(always)]
    fn mark(&self, _: u64) {}
    fn clone_signal(&self) -> Self {
        Self
    }
}

/// A ready counter: stores the tick index (at least 1) with `Release`.
#[derive(Debug, Clone)]
pub struct ReadyCounter(Arc<AtomicU64>);

impl ReadySignal for ReadyCounter {
    const TRACKS: bool = true;
    #[inline]
    fn mark(&self, tick: u64) {
        self.0.store(tick.max(1), Ordering::Release);
    }
    fn clone_signal(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// When a compositor draws (R-03 of the November 2026 performance audit; spec
/// `docs/specs/gpu-compositor.md`, "Render on change"). A type parameter, so the hot loop
/// never tests a flag: [`OnChange`] compiles the key check in, [`EveryTick`] compiles it
/// away and draws every tick as before.
pub trait Redraw: Send + 'static {
    /// True for [`OnChange`]: an unchanged tick leaves the canvas slot as it is.
    const ON_CHANGE: bool;
}

/// Draws only when a layer's picture, the layer list or an animation changed; otherwise
/// the canvas frame of the last draw stays on "canvas" with its timestamp and sequence.
/// The default.
#[derive(Debug, Default, Clone, Copy)]
pub struct OnChange;

impl Redraw for OnChange {
    const ON_CHANGE: bool = true;
}

/// Draws every tick (diagnostics and A/B measurements: `RBS_RENDER_EVERY_TICK=1`).
#[derive(Debug, Default, Clone, Copy)]
pub struct EveryTick;

impl Redraw for EveryTick {
    const ON_CHANGE: bool = false;
}

/// The environment variable that makes [`CompositorNode::into_node`] build compositors
/// that draw every tick: `1` (anything but empty or `0`).
pub const RENDER_EVERY_TICK_ENV: &str = "RBS_RENDER_EVERY_TICK";

/// Whether [`RENDER_EVERY_TICK_ENV`] is set. Read once per process, on the first call
/// (the control thread building a graph), never on the render thread.
pub fn render_every_tick() -> bool {
    static EVERY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var_os(RENDER_EVERY_TICK_ENV).is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// Counters of one compositor node, read by diagnostics. One relaxed add per tick.
#[derive(Debug, Default)]
pub struct CompositorStats {
    /// Ticks that drew the canvas.
    pub drawn: AtomicU64,
    /// Ticks that found every input unchanged and kept the last canvas ([`OnChange`]).
    pub unchanged: AtomicU64,
}

/// What a layer showed at the last draw: its texture and the picture's key, or `None`
/// for a layer that had no picture.
type LayerKey = Option<(TextureId, Timestamp, u64)>;

/// Draws the frames on its "layers" links into a canvas and publishes it on "canvas".
pub struct CompositorNode<R: ReadySignal = NoReady, D: Redraw = OnChange> {
    descriptor: NodeDescriptor,
    canvas: CanvasDesc,
    capacity: u32,
    layers: ParamReceiver<Vec<NodeLayer>>,
    /// Built in `prepare`, dropped in `release`.
    compositor: Option<Compositor>,
    /// Reused every tick; room for `capacity` layers.
    scratch: Vec<Layer>,
    /// An incoming compositor's ready counter (ADR-0128 decision 7): the last tick at
    /// which every layer of its list whose link resolves had a texture frame.
    /// [`NoReady`] for every other compositor, which then does no more work.
    ready: R,
    /// The key of the last draw, one entry per listed layer; room for `capacity` entries
    /// reserved in `prepare` ([`OnChange`] only).
    drawn_key: Vec<LayerKey>,
    /// This tick's key, built beside `scratch`; swapped with `drawn_key` after a draw.
    key: Vec<LayerKey>,
    /// `drawn_key` describes the frame on "canvas" (false until the first draw and after
    /// a failed one).
    drawn_valid: bool,
    /// Tick time of the last draw: a fade or slide that had not ended then draws again.
    drawn_at: Timestamp,
    /// Scroll records, when the node was built [`CompositorNode::with_scroll`].
    scroll: Option<ParamReceiver<Option<Arc<ScrollBinding>>>>,
    stats: Arc<CompositorStats>,
    redraw: std::marker::PhantomData<D>,
}

impl CompositorNode {
    /// A compositor for `canvas` drawing at most `capacity` layers, starting with `layers`.
    /// The sender replaces the list while the node runs (latest list wins).
    pub fn new(canvas: CanvasDesc, capacity: u32, layers: Vec<NodeLayer>) -> (Self, LayerSender) {
        let (sender, receiver) = param::channel(layers);
        let node = Self {
            descriptor: compositor_descriptor(),
            canvas,
            capacity: capacity.max(1),
            layers: receiver,
            compositor: None,
            scratch: Vec::new(),
            ready: NoReady,
            drawn_key: Vec::new(),
            key: Vec::new(),
            drawn_valid: false,
            drawn_at: Timestamp::ZERO,
            scroll: None,
            stats: Arc::default(),
            redraw: std::marker::PhantomData,
        };
        (node, sender)
    }

    /// This compositor with a ready counter (ADR-0128 decision 7): `process` stores the
    /// tick index (Release) on each tick at which every listed layer whose link resolves
    /// had a texture frame; an empty list counts as ready. 0 means never.
    pub fn with_ready(self, ready: Arc<AtomicU64>) -> CompositorNode<ReadyCounter> {
        self.rebuild(ReadyCounter(ready))
    }
}

impl<R: ReadySignal> CompositorNode<R> {
    /// This compositor drawing every tick, as before render on change ([`EveryTick`]).
    pub fn every_tick(self) -> CompositorNode<R, EveryTick> {
        let ready = self.ready.clone_signal();
        self.rebuild(ready)
    }

    /// The node for the graph: [`OnChange`], or [`EveryTick`] when
    /// [`render_every_tick`] says so (read once per process).
    pub fn into_node(self) -> Box<dyn Node<GpuRuntime>> {
        if render_every_tick() {
            Box::new(self.every_tick())
        } else {
            Box::new(self)
        }
    }
}

impl<R: ReadySignal, D: Redraw> CompositorNode<R, D> {
    /// The same node with another ready signal and redraw policy.
    fn rebuild<R2: ReadySignal, D2: Redraw>(self, ready: R2) -> CompositorNode<R2, D2> {
        CompositorNode {
            descriptor: self.descriptor,
            canvas: self.canvas,
            capacity: self.capacity,
            layers: self.layers,
            compositor: self.compositor,
            scratch: self.scratch,
            ready,
            drawn_key: self.drawn_key,
            key: self.key,
            drawn_valid: false,
            drawn_at: Timestamp::ZERO,
            scroll: self.scroll,
            stats: self.stats,
            redraw: std::marker::PhantomData,
        }
    }

    /// This node with a channel for its scroll records (ADR-0150 decision 7). No GPU
    /// object exists until a [`ScrollBinding`] is sent; a node built without it draws
    /// scrolling layers blank.
    pub fn with_scroll(mut self) -> (Self, ScrollSender) {
        let (sender, receiver) = param::channel(None);
        self.scroll = Some(receiver);
        (self, sender)
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<CompositorStats> {
        Arc::clone(&self.stats)
    }
}

impl<R: ReadySignal, D: Redraw> CompositorNode<R, D> {
    /// The most layers this node draws.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    fn canvas_desc(&self) -> TextureDesc {
        TextureDesc::canvas(self.canvas.width, self.canvas.height)
    }
}

fn backend(e: GpuError) -> NodeError {
    NodeError::Backend(match e {
        GpuError::PoolFull { .. } => "texture pool full",
        GpuError::StaleTexture(_) | GpuError::UnknownTexture(_) => "stale texture",
        GpuError::TargetMismatch(_) => "canvas does not match",
        GpuError::TooManyLayers { .. } => "too many layers",
        _ => "gpu error",
    })
}

/// Texture coordinates of `crop` on a `width x height` source.
fn uv(crop: PixelCrop, width: u32, height: u32) -> UvRect {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    UvRect {
        u0: crop.left as f32 / w,
        v0: crop.top as f32 / h,
        u1: 1.0 - crop.right as f32 / w,
        v1: 1.0 - crop.bottom as f32 / h,
    }
}

impl<R: ReadySignal, D: Redraw> Node<GpuRuntime> for CompositorNode<R, D> {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let compositor = Compositor::new(ctx.shared, self.canvas, self.capacity)
            .map_err(|e| NodeError::Failed(e.to_string()))?;
        self.compositor = Some(compositor);
        self.scratch = Vec::with_capacity(self.capacity as usize);
        if D::ON_CHANGE {
            self.drawn_key = Vec::with_capacity(self.capacity as usize);
            self.key = Vec::with_capacity(self.capacity as usize);
        }
        self.drawn_valid = false;
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let desc = self.canvas_desc();
        let mut ring = [None; CANVAS_RING];
        for slot in &mut ring {
            *slot = Some(ctx.local.pool.acquire(ctx.shared, &desc).map_err(backend)?);
        }
        for id in ring.into_iter().flatten() {
            ctx.local.pool.release(id).map_err(backend)?;
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let desc = self.canvas_desc();
        let compositor = self.compositor.as_mut().ok_or(NodeError::NotReady)?;
        let (inputs, outputs, local) = ctx.parts();

        let listed = self.layers.update();
        // Scroll records arrive once; a change of binding needs no redraw by itself.
        let binding = match self.scroll.as_mut() {
            Some(rx) => {
                rx.update();
                rx.get().as_deref()
            }
            None => None,
        };
        self.scratch.clear();
        if D::ON_CHANGE {
            self.key.clear();
        }
        // A fade or slide that had not ended at the last draw: this tick moves it.
        let mut moving = false;
        // A linked layer without a picture this tick (ADR-0128 decision 7).
        let mut missing = false;
        for spec in self.layers.get().iter().take(self.capacity as usize) {
            let Some(link) = link_of(&inputs, spec) else {
                continue;
            };
            let picture = inputs.video(LAYERS_PIN, link).and_then(|frame| {
                let FramePayload::Texture(id) = *frame.payload() else {
                    return None;
                };
                let tex = local.pool.get(id)?;
                Some((frame, id, tex.desc()))
            });
            if D::ON_CHANGE {
                // Room for `capacity` entries was reserved in `prepare`: no allocation.
                self.key
                    .push(picture.map(|(f, id, _)| (id, f.timestamp(), f.sequence())));
                let ends = |start: Timestamp, length: Duration| start + length;
                moving |= spec
                    .fade
                    .is_some_and(|f| self.drawn_at <= ends(f.start, f.duration))
                    || spec
                        .slide
                        .is_some_and(|m| self.drawn_at <= ends(m.start, m.duration));
            }
            let Some((frame, id, d)) = picture else {
                missing = R::TRACKS;
                continue;
            };
            let transform = match &spec.slide {
                Some(slide) => slide.at(spec.transform, tick.time),
                None => spec.transform,
            };
            let crop = uv(spec.crop, d.width, d.height);
            let scroll = match &spec.scroll {
                None => None,
                Some(sc) => {
                    if D::ON_CHANGE {
                        let ended = sc
                            .frame(&transform, &crop, d.width, d.height, self.drawn_at)
                            .is_some_and(|f| f.ended);
                        moving |= sc.moving_at(self.drawn_at, ended);
                    }
                    // No length, or a Once picture that has left: nothing to draw.
                    match sc.frame(&transform, &crop, d.width, d.height, tick.time) {
                        Some(f) if !f.ended => Some(f.gpu(sc.direction, sc.repeat, sc.fade)),
                        _ => continue,
                    }
                }
            };
            let source_transfer =
                if frame.format().color.transfer == TransferFunction::Srgb && !d.format.is_srgb() {
                    Transfer::Srgb
                } else {
                    Transfer::Linear
                };
            // Room for `capacity` layers was reserved in `prepare`, so this push does not
            // allocate.
            self.scratch.push(Layer {
                texture: id,
                transform,
                crop,
                opacity: match &spec.fade {
                    Some(fade) => fade.at(spec.opacity, tick.time),
                    None => spec.opacity,
                },
                source_transfer,
                opaque: spec.opaque,
                style: spec.style,
                // A 4:2:0 picture is sampled as it is, with its own colour space (R-11).
                ycbcr: frame.format().color,
                scroll,
            });
        }

        // Render on change (R-03): nothing new to draw, so the canvas frame of the last
        // draw stays on "canvas" with its timestamp and sequence, and every reader that
        // keys on them skips its work too. The canvas description is touched so its
        // ring is not trimmed while unchanged.
        if D::ON_CHANGE && self.drawn_valid && !listed && !moving && self.key == self.drawn_key {
            local.pool.touch(&desc);
            self.stats.unchanged.fetch_add(1, Ordering::Relaxed);
            if R::TRACKS && !missing {
                self.ready.mark(tick.index);
            }
            return Ok(());
        }
        // A failed draw below leaves the key invalid, so the next tick draws again.
        self.drawn_valid = false;

        let canvas = local.pool.acquire(gpu, &desc).map_err(backend)?;
        let drawn = compositor.render_scrolled(gpu, &local.pool, &self.scratch, canvas, binding);
        let format = VideoFormat::new(
            Size::new(desc.width, desc.height),
            PixelFormat::Rgba16Float,
            ColorSpace::LINEAR_BT709,
        );
        let frame = drawn.and_then(|_| local.pool.frame(canvas, format, tick.time));
        // The frame's lease keeps the slot alive; the pool reuses it once it is dropped.
        let released = local.pool.release(canvas);
        let frame = frame.map_err(backend)?;
        released.map_err(backend)?;
        // A new generation per draw: an unchanged canvas keeps its key, so the key
        // changes exactly when the picture does (ADR-0117 decision 4), and a node built
        // anew never repeats an earlier instance's key.
        drop(outputs.set_video(CANVAS_PIN, Some(frame.with_sequence(next_generation())))?);
        self.stats.drawn.fetch_add(1, Ordering::Relaxed);
        if D::ON_CHANGE {
            std::mem::swap(&mut self.key, &mut self.drawn_key);
            self.drawn_valid = true;
            self.drawn_at = tick.time;
        }
        if R::TRACKS && !missing {
            self.ready.mark(tick.index);
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.compositor = None;
        self.scratch = Vec::new();
        self.drawn_key = Vec::new();
        self.key = Vec::new();
        self.drawn_valid = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Timestamp {
        Timestamp::from_nanos(v * 1_000_000)
    }

    #[test]
    fn fade_is_linear_between_start_and_end() {
        let fade = Fade {
            from: 0.2,
            start: ms(100),
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        };
        assert_eq!(fade.at(1.0, ms(0)), 0.2);
        assert_eq!(fade.at(1.0, ms(100)), 0.2);
        assert!((fade.at(1.0, ms(200)) - 0.6).abs() < 1e-6);
        assert_eq!(fade.at(1.0, ms(300)), 1.0);
        assert_eq!(fade.at(1.0, ms(10_000)), 1.0);
    }

    #[test]
    fn a_slide_is_halfway_at_half_time_and_done_after() {
        let slide = Slide {
            from: Affine2::rect(0.0, 0.0, 100.0, 100.0),
            start: ms(0),
            duration: Duration::from_millis(400),
            easing: Easing::EaseInOut,
        };
        let to = Affine2::rect(200.0, 0.0, 100.0, 100.0);
        assert_eq!(slide.at(to, ms(0)).tx, 0.0);
        // Smoothstep is still halfway at mid-time (symmetric ease).
        assert!((slide.at(to, ms(200)).tx - 100.0).abs() < 1e-3);
        // Early on it is behind linear; late it is ahead of linear's mid pace.
        assert!(slide.at(to, ms(100)).tx < 50.0);
        assert!(slide.at(to, ms(300)).tx > 150.0);
        assert_eq!(slide.at(to, ms(400)).tx, 200.0);
    }

    #[test]
    fn a_linear_slide_is_halfway_at_half_time() {
        let slide = Slide {
            from: Affine2::rect(0.0, 0.0, 100.0, 100.0),
            start: ms(0),
            duration: Duration::from_millis(400),
            easing: Easing::Linear,
        };
        let to = Affine2::rect(200.0, 0.0, 100.0, 100.0);
        assert!((slide.at(to, ms(200)).tx - 100.0).abs() < 1e-3);
        assert!((slide.at(to, ms(100)).tx - 50.0).abs() < 1e-3);
    }

    #[test]
    fn zero_length_fade_jumps_at_start() {
        let fade = Fade {
            from: 1.0,
            start: ms(50),
            duration: Duration::ZERO,
            easing: Easing::Linear,
        };
        assert_eq!(fade.at(0.0, ms(50)), 1.0);
        assert_eq!(fade.at(0.0, ms(51)), 0.0);
    }
}
