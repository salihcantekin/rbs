//! The render executor's runtime and the compositor node of the media graph
//! (spec `docs/specs/gpu-compositor.md`, "Graph runtime and compositor node"; ADR-0020).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, Size, TextureId, VideoFormat, VideoFrame,
    next_generation,
};
use rbs_core::time::Timestamp;
use rbs_graph::description::NodeId;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{self, ParamReceiver, ParamSender};
use rbs_graph::runtime::{
    Inputs, Node, NodeError, Outputs, PrepareCtx, ProcessCtx, Runtime, Tick, WarmCtx,
};

use crate::compositor::{
    CanvasDesc, Compositor, Layer, LayerStyle, MAX_SCROLL_LAYERS, ScrollBinding, Transfer,
};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::Importer;
use crate::math::{Affine2, UvRect};
use crate::pool::{TextureDesc, TexturePool};
use crate::scroll::{LayerScroll, ScrollFrame};

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
    /// The crop at and before `start`, moved to the layer's `crop` with the box, so a
    /// picture cropped in only one of the two scenes keeps its shape (ADR-0128 review, owner 2026-11-23).
    /// `None` keeps the layer's crop all along.
    pub from_crop: Option<PixelCrop>,
}

impl Slide {
    /// The box at `now` for a move ending at `to`.
    pub fn at(&self, to: Affine2, now: Timestamp) -> Affine2 {
        match self.progress(now) {
            Some(t) => self.from.lerp(to, t),
            None if now <= self.start => self.from,
            None => to,
        }
    }

    /// The eased progress strictly inside the move, `None` at and before `start`
    /// and at and after the end.
    fn progress(&self, now: Timestamp) -> Option<f32> {
        if now <= self.start || self.duration.is_zero() {
            return None;
        }
        let elapsed = now.saturating_since(self.start);
        if elapsed >= self.duration {
            return None;
        }
        let u = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        Some(self.easing.map(u))
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

/// The most group spans one compositor draws (ADR-0193 decision 4).
pub const MAX_SPANS: usize = 4;
/// The most pixels of one group span's intermediate (ADR-0193 decision 4).
pub const MAX_SPAN_PIXELS: u64 = 8_388_608;
/// The most pixels of one compositor's group span intermediates together (128 MiB of
/// `Rgba16Float`, ADR-0193 decision 4).
pub const MAX_SPANS_PIXELS: u64 = 16_777_216;

/// A group drawn as one picture (ADR-0193 decision 2): its members, a range of the layer
/// list, are drawn into an intermediate covering its window, which the canvas pass
/// samples as one scrolling layer where the members were. Needs the node's
/// [`ScrollBinding`] like any scrolling layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupSpan {
    /// Index of the first member layer.
    pub first: u32,
    /// Number of member layers, at least one.
    pub count: u32,
    /// The window in canvas pixels (x, y, width, height). Its whole-pixel cover is the
    /// intermediate; the fractional window is the clip.
    pub window: [f32; 4],
    /// The group's Scroll.
    pub scroll: LayerScroll,
    /// Moves the span's canvas layer by (x, y) canvas pixels, a push Swipe's offset;
    /// its members are drawn unmoved into the intermediate.
    pub offset: [f32; 2],
    /// Animates the span's canvas layer to its moved window, like [`NodeLayer::slide`].
    pub slide: Option<Slide>,
    /// The sender's id for the span; the node ignores it. It names the span whose slot
    /// [`CompositorStats::span_acquire_failing`] and `span_pass_failing` report.
    pub tag: u64,
}

impl GroupSpan {
    /// The intermediate: its origin (the window's corner, floored) and size in texels
    /// (to the far corner, ceiled). `None` for a window that is not finite or has no
    /// area.
    pub fn texels(&self) -> Option<(f32, f32, u32, u32)> {
        let [x, y, w, h] = self.window;
        if !(w > 0.0 && h > 0.0 && x.is_finite() && y.is_finite() && (x + w + y + h).is_finite()) {
            return None;
        }
        let (x0, y0) = (x.floor(), y.floor());
        // `as` saturates; a size past the limits is refused by the caller.
        Some((
            x0,
            y0,
            ((x + w).ceil() - x0) as u32,
            ((y + h).ceil() - y0) as u32,
        ))
    }

    fn end(&self) -> usize {
        (self.first as usize).saturating_add(self.count as usize)
    }

    /// The canvas layer's box at `now`: the window, moved by `offset` and `slide`.
    fn placed(&self, now: Timestamp) -> Affine2 {
        let [x, y, w, h] = self.window;
        let to = Affine2::rect(x + self.offset[0], y + self.offset[1], w, h);
        self.slide.map_or(to, |s| s.at(to, now))
    }
}

/// A compositor node's layer list: the layers, bottom first, and the group spans over
/// them, sorted by `first` and disjoint (ADR-0193 decision 2). A list without spans
/// takes the node's path without groups.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerList {
    /// The layers, bottom first.
    pub layers: Vec<NodeLayer>,
    /// The group spans.
    pub spans: Vec<GroupSpan>,
}

/// The control-thread end of a compositor node's layer list.
pub struct LayerSender(ParamSender<LayerList>);

impl LayerSender {
    /// Sends `layers` without group spans; see [`ParamSender::send`].
    pub fn send(&mut self, layers: Vec<NodeLayer>) -> Option<Vec<NodeLayer>> {
        let spans = Vec::new();
        self.0.send(LayerList { layers, spans }).map(|l| l.layers)
    }

    /// Sends a list with group spans; see [`ParamSender::send`].
    pub fn send_list(&mut self, list: LayerList) -> Option<LayerList> {
        self.0.send(list)
    }

    /// The layers the node last replaced; see [`ParamSender::recycle`].
    pub fn recycle(&mut self) -> Option<Vec<NodeLayer>> {
        self.0.recycle().map(|l| l.layers)
    }

    /// The list the node last replaced, with its spans.
    pub fn recycle_list(&mut self) -> Option<LayerList> {
        self.0.recycle()
    }

    /// See [`ParamSender::pending`].
    pub fn pending(&self) -> bool {
        self.0.pending()
    }

    /// See [`ParamSender::is_closed`].
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

impl std::fmt::Debug for LayerSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The control-thread end of a compositor node's scroll records (ADR-0150 decision 7):
/// send a [`ScrollBinding`] before the first list with a scrolling layer. Idempotent; a
/// skipped update only leaves a scrolling layer blank for a tick.
pub type ScrollSender = ParamSender<Option<ScrollBinding>>;

/// The link of `layer` on the layers pin: the hint if it still names the producer,
/// otherwise a lookup.
fn link_of(inputs: &Inputs<'_>, layer: &NodeLayer) -> Option<usize> {
    if inputs.source(LAYERS_PIN, layer.link) == Some(layer.producer) {
        Some(layer.link)
    } else {
        inputs.find(LAYERS_PIN, layer.producer)
    }
}

/// The longest a compositor keeps drawing its list while a new one waits for a
/// producer the schedule has not linked yet (ADR-0199 decision 5).
const HOLD_LIMIT: Duration = Duration::from_secs(1);

/// Takes the newest layer list once the schedule that links it runs (ADR-0199 decision
/// 6). The list is sent before its apply, and a joining node is prepared before the
/// swap, so for a while the new list names a producer that is not linked yet: drawn,
/// its layer would vanish. Until then the current list is drawn. A producer that was
/// unlinked in the current list too (a source that failed) does not hold a list, and
/// no list waits longer than [`HOLD_LIMIT`]. Runs only when a list is waiting; no
/// allocation.
fn take_list(
    layers: &mut ParamReceiver<LayerList>,
    held: &mut Option<Timestamp>,
    inputs: &Inputs<'_>,
    capacity: u32,
    now: Timestamp,
) -> bool {
    let taken = layers.update_when(|current, next| {
        let since = *held.get_or_insert(now);
        now.saturating_since(since) >= HOLD_LIMIT || linked(inputs, current, next, capacity)
    });
    if taken {
        *held = None;
    }
    taken
}

/// True when every layer of `next` has its link, or had none in `current` either.
#[inline(never)]
fn linked(inputs: &Inputs<'_>, current: &LayerList, next: &LayerList, capacity: u32) -> bool {
    let n = capacity as usize;
    next.layers.iter().take(n).all(|spec| {
        link_of(inputs, spec).is_some()
            || current
                .layers
                .iter()
                .take(n)
                .any(|c| c.producer == spec.producer)
    })
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
    /// Group span intermediates drawn (ADR-0193).
    pub span_passes: AtomicU64,
    /// Group span intermediates acquired, retries after a failure included.
    pub span_acquires: AtomicU64,
    /// Groups drawn transparent at a draw (over a limit, no scroll record, a failed
    /// acquire or pass), and malformed spans ignored, once per list.
    pub span_failures: AtomicU64,
    /// Bit `s` set: span `s` of the list last drawn has no intermediate, its acquire
    /// failed (ADR-0193 decision 5). Written at every draw of a list with spans and
    /// cleared by a list without, so it is the current state, not a history. A
    /// standalone status word that publishes no other memory: relaxed.
    pub span_acquire_failing: AtomicU32,
    /// Bit `s` set: span `s` of the list last drawn failed its pass (no scroll record
    /// or a failed render), written like `span_acquire_failing`.
    pub span_pass_failing: AtomicU32,
}

// One bit per span slot.
const _: () = assert!(MAX_SPANS <= 32);

/// A group span's intermediate, by the span's index in the list.
#[derive(Debug, Clone, Copy, Default)]
struct SpanSlot {
    texture: Option<TextureId>,
    /// The size the span wants; `None` past a limit or without a span.
    size: Option<(u32, u32)>,
    /// The intermediate's origin on the canvas.
    origin: (f32, f32),
    /// The intermediate holds the members as last keyed.
    drawn: bool,
    /// The pool refused the last acquire for `size`; not tried again until the size
    /// changes (ADR-0193 decision 3: at most one acquire per span per event).
    acquire_failed: bool,
}

/// What a layer showed at the last draw: its texture and the picture's key, or `None`
/// for a layer that had no picture.
type LayerKey = Option<(TextureId, Timestamp, u64)>;

/// Draws the frames on its "layers" links into a canvas and publishes it on "canvas".
pub struct CompositorNode<R: ReadySignal = NoReady, D: Redraw = OnChange> {
    descriptor: NodeDescriptor,
    canvas: CanvasDesc,
    capacity: u32,
    layers: ParamReceiver<LayerList>,
    /// Built in `prepare`, dropped in `release`.
    compositor: Option<Compositor>,
    /// Reused every tick; room for `capacity` layers.
    scratch: Vec<Layer>,
    /// The canvas pass of a list with group spans; room for `capacity` layers.
    canvas_list: Vec<Layer>,
    /// Group span intermediates (ADR-0193 decision 3).
    spans: [SpanSlot; MAX_SPANS],
    /// An incoming compositor's ready counter (ADR-0128 decision 7): the last tick at
    /// which every layer of its list whose link resolves had a texture frame.
    /// [`NoReady`] for every other compositor, which then does no more work.
    ready: R,
    /// The key of the last draw, one entry per listed layer; room for `capacity` entries
    /// reserved in `prepare` ([`OnChange`] only).
    drawn_key: Vec<LayerKey>,
    /// This tick's key, compared before the draw list is built; swapped with
    /// `drawn_key` after a draw.
    key: Vec<LayerKey>,
    /// `drawn_key` describes the frame on "canvas" (false until the first draw and after
    /// a failed one).
    drawn_valid: bool,
    /// Tick time of the last draw: a fade or slide that had not ended then draws again.
    drawn_at: Timestamp,
    /// Scroll records, when the node was built [`CompositorNode::with_scroll`].
    scroll: Option<ParamReceiver<Option<ScrollBinding>>>,
    /// Tick time since which a new list waits for its links ([`take_list`]).
    held: Option<Timestamp>,
    stats: Arc<CompositorStats>,
    redraw: std::marker::PhantomData<D>,
}

impl CompositorNode {
    /// A compositor for `canvas` drawing at most `capacity` layers, starting with `layers`.
    /// The sender replaces the list while the node runs (latest list wins).
    pub fn new(canvas: CanvasDesc, capacity: u32, layers: Vec<NodeLayer>) -> (Self, LayerSender) {
        let spans = Vec::new();
        let (sender, receiver) = param::channel(LayerList { layers, spans });
        let node = Self {
            descriptor: compositor_descriptor(),
            canvas,
            capacity: capacity.max(1),
            layers: receiver,
            compositor: None,
            scratch: Vec::new(),
            canvas_list: Vec::new(),
            spans: Default::default(),
            ready: NoReady,
            drawn_key: Vec::new(),
            key: Vec::new(),
            drawn_valid: false,
            drawn_at: Timestamp::ZERO,
            scroll: None,
            held: None,
            stats: Arc::default(),
            redraw: std::marker::PhantomData,
        };
        (node, LayerSender(sender))
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
            canvas_list: self.canvas_list,
            spans: self.spans,
            ready,
            drawn_key: self.drawn_key,
            key: self.key,
            drawn_valid: false,
            drawn_at: Timestamp::ZERO,
            scroll: self.scroll,
            held: None,
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

/// Where a layer is drawn at `now` on a `width x height` source: its box, crop and
/// style, moved by its slide (ADR-0100). While a slide runs, the crop moves from the
/// slide's `from_crop` with the box, and the style's box size follows the box's own
/// scale on each axis, so a circle stays round and a border keeps its width on a box
/// whose aspect changes (ADR-0128 review, owner 2026-11-23). No allocation; a few multiplies per sliding layer.
#[inline(always)]
fn placement(
    spec: &NodeLayer,
    now: Timestamp,
    width: u32,
    height: u32,
) -> (Affine2, UvRect, LayerStyle) {
    match &spec.slide {
        None => (spec.transform, uv(spec.crop, width, height), spec.style),
        Some(slide) => sliding(spec, slide, now, width, height),
    }
}

/// [`placement`] of a layer with a slide; kept out of line so a layer without one
/// costs what it did before.
#[inline(never)]
fn sliding(
    spec: &NodeLayer,
    slide: &Slide,
    now: Timestamp,
    width: u32,
    height: u32,
) -> (Affine2, UvRect, LayerStyle) {
    let crop = uv(spec.crop, width, height);
    let Some(t) = slide.progress(now) else {
        // Ended: the layer's own place. Not started yet: where it comes from.
        if now > slide.start {
            return (spec.transform, crop, spec.style);
        }
        let from_crop = slide.from_crop.map_or(crop, |c| uv(c, width, height));
        return (slide.from, from_crop, spec.style);
    };
    let transform = slide.from.lerp(spec.transform, t);
    let crop = match slide.from_crop {
        Some(from) => {
            let f = uv(from, width, height);
            let m = |a: f32, b: f32| a + (b - a) * t;
            UvRect {
                u0: m(f.u0, crop.u0),
                v0: m(f.v0, crop.v0),
                u1: m(f.u1, crop.u1),
                v1: m(f.v1, crop.v1),
            }
        }
        None => crop,
    };
    let mut style = spec.style;
    let ratio = |x: f32, y: f32, to_x: f32, to_y: f32| {
        let end = to_x.hypot(to_y);
        if end > 0.0 && end.is_finite() {
            x.hypot(y) / end
        } else {
            1.0
        }
    };
    let to = &spec.transform;
    style.box_size[0] *= ratio(transform.a, transform.b, to.a, to.b);
    style.box_size[1] *= ratio(transform.c, transform.d, to.c, to.d);
    (transform, crop, style)
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
        // The canvas pass of a list with group spans (ADR-0193 decision 3).
        self.canvas_list = Vec::with_capacity(self.capacity as usize);
        if D::ON_CHANGE {
            self.drawn_key = Vec::with_capacity(self.capacity as usize);
            self.key = Vec::with_capacity(self.capacity as usize);
        }
        self.drawn_valid = false;
        self.held = None;
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
        let (now, inputs) = (ctx.tick().time, ctx.inputs());
        let listed = take_list(
            &mut self.layers,
            &mut self.held,
            &inputs,
            self.capacity,
            now,
        );
        // One emptiness check per tick: a list without group spans takes the path it
        // always had (ADR-0193 decision 2).
        if !self.layers.get().spans.is_empty() {
            return self.process_spans(ctx, listed);
        }
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let desc = self.canvas_desc();
        let compositor = self.compositor.as_mut().ok_or(NodeError::NotReady)?;
        let (inputs, outputs, local) = ctx.parts();
        if listed {
            // Spans that left the list give their intermediates back.
            free_spans(&mut self.spans, &mut local.pool);
            clear_failing(&self.stats);
        }
        // Scroll records arrive once; a change of binding needs no redraw by itself.
        let binding = bound(&mut self.scroll);
        // A linked layer without a picture this tick (ADR-0128 decision 7).
        let mut missing = false;
        if D::ON_CHANGE {
            self.key.clear();
            // A fade, slide or scroll that had not ended at the last draw still moves.
            let mut moving = false;
            for spec in self.layers.get().layers.iter().take(self.capacity as usize) {
                let Some(link) = link_of(&inputs, spec) else {
                    continue;
                };
                let picture = picture(&inputs, &local.pool, link);
                // Room for `capacity` entries was reserved in `prepare`: no allocation.
                self.key.push(key_of(picture));
                moving |= is_moving(spec, picture.map(|p| p.2), self.drawn_at, tick.time);
                missing |= R::TRACKS && picture.is_none();
            }
            // Render on change (R-03): compare the key before building the draw list.
            // Nothing new to draw, so the canvas frame of the last draw stays on
            // "canvas" with its timestamp and sequence, and every reader that keys on
            // them skips its work too. The canvas description is touched so its ring
            // is not trimmed while unchanged.
            if self.drawn_valid && !listed && !moving && self.key == self.drawn_key {
                local.pool.touch(&desc);
                self.stats.unchanged.fetch_add(1, Ordering::Relaxed);
                if R::TRACKS && !missing {
                    self.ready.mark(tick.index);
                }
                return Ok(());
            }
        }

        self.scratch.clear();
        for spec in self.layers.get().layers.iter().take(self.capacity as usize) {
            let Some(link) = link_of(&inputs, spec) else {
                continue;
            };
            let Some((frame, id, d)) = picture(&inputs, &local.pool, link) else {
                missing = R::TRACKS;
                continue;
            };
            // Room for `capacity` layers was reserved in `prepare`, so this push does not
            // allocate.
            if let Some(layer) = draw_layer(spec, frame, id, &d, tick.time) {
                self.scratch.push(layer);
            }
        }

        // A failed draw below leaves the key invalid, so the next tick draws again.
        self.drawn_valid = false;
        let pool = &mut local.pool;
        publish(
            compositor,
            gpu,
            pool,
            outputs,
            &self.scratch,
            binding,
            0,
            tick.time,
        )?;
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

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        // Leaving the schedule: group span intermediates go back to the pool (ADR-0193
        // decision 3). The canvas ring is the pool's to trim.
        free_spans(&mut self.spans, &mut ctx.local.pool);
        clear_failing(&self.stats);
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.compositor = None;
        self.scratch = Vec::new();
        self.canvas_list = Vec::new();
        self.spans = Default::default();
        self.drawn_key = Vec::new();
        self.key = Vec::new();
        self.drawn_valid = false;
    }
}

impl<R: ReadySignal, D: Redraw> CompositorNode<R, D> {
    /// [`Node::process`] for a list with group spans (ADR-0193 decisions 2 and 3): each
    /// span's members are drawn into its intermediate when they changed, then the canvas
    /// pass draws the other layers and one scrolling layer per span. Scroll records go
    /// bottom first in draw order, a span's before its members'; the span passes take
    /// the first slots and the canvas pass the rest.
    #[inline(never)]
    fn process_spans(
        &mut self,
        ctx: &mut ProcessCtx<'_, GpuRuntime>,
        listed: bool,
    ) -> Result<(), NodeError> {
        let (tick, gpu, desc) = (ctx.tick(), ctx.shared(), self.canvas_desc());
        let now = tick.time;
        let Self {
            layers,
            compositor,
            scratch,
            canvas_list,
            spans: slots,
            ready,
            drawn_key,
            key,
            drawn_valid,
            drawn_at,
            scroll,
            stats,
            capacity,
            ..
        } = self;
        let compositor = compositor.as_mut().ok_or(NodeError::NotReady)?;
        let (inputs, outputs, local) = ctx.parts();
        let pool = &mut local.pool;
        let list = layers.get();
        let n = list.layers.len().min(*capacity as usize);
        let spans = &list.spans[..well_formed(&list.spans, n)];
        if listed && spans.len() < list.spans.len() {
            let ignored = (list.spans.len() - spans.len()) as u64;
            stats.span_failures.fetch_add(ignored, Ordering::Relaxed);
        }
        size_spans(slots, spans, pool, gpu, stats);
        let binding = bound(scroll);
        let mut missing = false;
        // Per span: a member moves, so its intermediate is drawn again.
        let mut inner = [false; MAX_SPANS];
        let was_valid = *drawn_valid;
        if D::ON_CHANGE {
            key.clear();
            let (mut moving, mut at) = (false, 0);
            for (i, spec) in list.layers[..n].iter().enumerate() {
                let link = link_of(&inputs, spec);
                let picture = link.and_then(|link| picture(&inputs, pool, link));
                // One entry per layer, so a span compares its members' range. Room for
                // `capacity` entries was reserved in `prepare`: no allocation.
                key.push(key_of(picture));
                missing |= R::TRACKS && link.is_some() && picture.is_none();
                let m = is_moving(spec, picture.map(|p| p.2), *drawn_at, now);
                match member_of(spans, i, &mut at) {
                    Some(s) => inner.get_mut(s).into_iter().for_each(|f| *f |= m),
                    None => moving |= m,
                }
            }
            for ((span, slot), m) in spans.iter().zip(slots.iter()).zip(inner) {
                moving |= m || span_moving(span, slot, *drawn_at);
            }
            let waiting = slots.iter().any(|s| s.size.is_some() && !s.drawn);
            if was_valid && !listed && !moving && !waiting && key == drawn_key {
                pool.touch(&desc);
                stats.unchanged.fetch_add(1, Ordering::Relaxed);
                if R::TRACKS && !missing {
                    ready.mark(tick.index);
                }
                return Ok(());
            }
        }

        *drawn_valid = false;
        canvas_list.clear();
        // Scroll records given so far, and those of the span passes.
        let (mut used, mut base) = (0u32, 0u32);
        let (mut i, mut at, mut failed, mut retry) = (0, 0, 0u64, false);
        // Bit per span slot: a failed acquire, a failed pass.
        let (mut no_texture, mut no_pass) = (0u32, 0u32);
        let mut binding = binding;
        while i < n {
            let s = at;
            let Some(span) = spans.get(s).filter(|span| span.first as usize == i) else {
                let spec = &list.layers[i];
                i += 1;
                let Some(link) = link_of(&inputs, spec) else {
                    continue;
                };
                let Some((frame, id, d)) = picture(&inputs, pool, link) else {
                    missing = R::TRACKS;
                    continue;
                };
                if let Some(layer) = draw_layer(spec, frame, id, &d, now)
                    && (layer.scroll.is_none() || take_record(&mut used))
                {
                    // Room for `capacity` layers was reserved in `prepare`.
                    canvas_list.push(layer);
                }
                continue;
            };
            (i, at) = (span.end(), at + 1);
            // The span's record before its members' (ADR-0193 decision 2).
            let (wanted, texture, gave_up) = slots.get(s).map_or((false, None, false), |sl| {
                (sl.size.is_some(), sl.texture, sl.acquire_failed)
            });
            let (Some(slot), Some(texture)) = (slots.get_mut(s), texture) else {
                // Past a limit, or its acquire failed (tried again only when the size
                // changes, so a full pool costs no redraw per tick).
                (failed, retry) = (failed + 1, retry || (wanted && !gave_up));
                no_texture |= u32::from(wanted) << s;
                continue;
            };
            // A `Once` group whose picture has left draws nothing and needs no pass until
            // a new list (not a failure).
            let Some(layer) = span_layer(span, slot, texture, now) else {
                slot.drawn = true;
                continue;
            };
            if !take_record(&mut used) {
                failed += 1;
                no_pass |= 1 << s;
                continue;
            }
            scratch.clear();
            let (x0, y0) = slot.origin;
            let first_record = used;
            for spec in &list.layers[span.first as usize..span.end()] {
                let Some(link) = link_of(&inputs, spec) else {
                    continue;
                };
                let Some((frame, id, d)) = picture(&inputs, pool, link) else {
                    missing = R::TRACKS;
                    continue;
                };
                let Some(mut member) = draw_layer(spec, frame, id, &d, now) else {
                    continue;
                };
                if member.scroll.is_some() && !take_record(&mut used) {
                    continue;
                }
                // Into the span's window space.
                member.transform.tx -= x0;
                member.transform.ty -= y0;
                scratch.push(member);
            }
            let range = span.first as usize..span.end();
            let dirty = !D::ON_CHANGE
                || !was_valid
                || listed
                || !slot.drawn
                || inner.get(s).copied().unwrap_or(true)
                || key.get(range.clone()) != drawn_key.get(range);
            let records = binding.as_deref_mut();
            if dirty {
                slot.drawn = compositor
                    .render_intermediate(gpu, pool, scratch, texture, records, base)
                    .is_ok();
                if !slot.drawn {
                    // Drawn transparent; a later tick tries again.
                    (failed, retry) = (failed + 1, true);
                    no_pass |= 1 << s;
                    base += used - first_record;
                    continue;
                }
                stats.span_passes.fetch_add(1, Ordering::Relaxed);
            }
            base += used - first_record;
            canvas_list.push(layer);
        }
        if failed > 0 {
            stats.span_failures.fetch_add(failed, Ordering::Relaxed);
        }
        stats
            .span_acquire_failing
            .store(no_texture, Ordering::Relaxed);
        stats.span_pass_failing.store(no_pass, Ordering::Relaxed);
        publish(
            compositor,
            gpu,
            pool,
            outputs,
            canvas_list,
            binding,
            base,
            now,
        )?;
        stats.drawn.fetch_add(1, Ordering::Relaxed);
        if D::ON_CHANGE {
            std::mem::swap(key, drawn_key);
            *drawn_valid = !retry;
            *drawn_at = now;
        }
        if R::TRACKS && !missing {
            ready.mark(tick.index);
        }
        Ok(())
    }
}

/// The scroll records, once sent ([`CompositorNode::with_scroll`]).
fn bound(scroll: &mut Option<ParamReceiver<Option<ScrollBinding>>>) -> Option<&mut ScrollBinding> {
    let rx = scroll.as_mut()?;
    rx.update();
    rx.get_mut().as_mut()
}

/// The picture on layers link `link` this tick: frame, texture and its description.
#[inline(always)]
fn picture<'a>(
    inputs: &Inputs<'a>,
    pool: &TexturePool,
    link: usize,
) -> Option<(&'a VideoFrame, TextureId, TextureDesc)> {
    let frame = inputs.video(LAYERS_PIN, link)?;
    let FramePayload::Texture(id) = *frame.payload() else {
        return None;
    };
    Some((frame, id, *pool.get(id)?.desc()))
}

#[inline(always)]
fn key_of(picture: Option<(&VideoFrame, TextureId, TextureDesc)>) -> LayerKey {
    picture.map(|(f, id, _)| (id, f.timestamp(), f.sequence()))
}

/// Whether a fade, slide or scroll of `spec` had not ended at the last draw
/// (`drawn_at`). Scroll motion uses the same box as the draw (tick time `now`), so a
/// slide that changes the window still forces a draw.
#[inline(always)]
fn is_moving(
    spec: &NodeLayer,
    d: Option<TextureDesc>,
    drawn_at: Timestamp,
    now: Timestamp,
) -> bool {
    let ends = |start: Timestamp, length: Duration| start + length;
    let mut moving = spec
        .fade
        .is_some_and(|f| drawn_at <= ends(f.start, f.duration))
        || spec
            .slide
            .is_some_and(|m| drawn_at <= ends(m.start, m.duration));
    if let (Some(sc), Some(d)) = (&spec.scroll, d) {
        let (transform, crop, _) = placement(spec, now, d.width, d.height);
        let ended = sc
            .frame(&transform, &crop, d.width, d.height, drawn_at)
            .is_some_and(|f| f.ended);
        moving |= sc.moving_at(drawn_at, ended);
    }
    moving
}

/// The layer `spec` draws at `now` from its picture; `None` for a scroll with no length
/// or a `Once` picture that has left.
#[inline(always)]
fn draw_layer(
    spec: &NodeLayer,
    frame: &VideoFrame,
    texture: TextureId,
    d: &TextureDesc,
    now: Timestamp,
) -> Option<Layer> {
    let (transform, crop, style) = placement(spec, now, d.width, d.height);
    let scroll = match &spec.scroll {
        None => None,
        Some(sc) => match sc.frame(&transform, &crop, d.width, d.height, now) {
            Some(f) if !f.ended => Some(f.gpu(sc.direction, sc.repeat, sc.fade)),
            _ => return None,
        },
    };
    Some(Layer {
        texture,
        transform,
        crop,
        opacity: match &spec.fade {
            Some(fade) => fade.at(spec.opacity, now),
            None => spec.opacity,
        },
        source_transfer: Transfer::of(frame.format(), d.format),
        opaque: spec.opaque,
        style,
        // A 4:2:0 picture is sampled as it is, with its own colour space (R-11).
        ycbcr: frame.format().color,
        scroll,
    })
}

/// Draws `layers` into a new canvas and publishes it on "canvas", scroll records from
/// slot `base` on.
#[allow(clippy::too_many_arguments)] // The canvas pass of both paths.
fn publish(
    compositor: &mut Compositor,
    gpu: &GpuContext,
    pool: &mut TexturePool,
    outputs: &mut Outputs<'_>,
    layers: &[Layer],
    binding: Option<&mut ScrollBinding>,
    base: u32,
    time: Timestamp,
) -> Result<(), NodeError> {
    let canvas = compositor.canvas();
    let desc = TextureDesc::canvas(canvas.width, canvas.height);
    let canvas = pool.acquire(gpu, &desc).map_err(backend)?;
    let drawn = compositor.render_scrolled(gpu, pool, layers, canvas, binding, base);
    let format = VideoFormat::new(
        Size::new(desc.width, desc.height),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    let frame = drawn.and_then(|_| pool.frame(canvas, format, time));
    // The frame's lease keeps the slot alive; the pool reuses it once it is dropped.
    let released = pool.release(canvas);
    let frame = frame.map_err(backend)?;
    released.map_err(backend)?;
    // A new generation per draw: an unchanged canvas keeps its key, so the key changes
    // exactly when the picture does (ADR-0117 decision 4), and a node built anew never
    // repeats an earlier instance's key.
    drop(outputs.set_video(CANVAS_PIN, Some(frame.with_sequence(next_generation())))?);
    Ok(())
}

/// Takes one scroll record if any is left (ADR-0150 decision 7).
fn take_record(used: &mut u32) -> bool {
    let free = *used < MAX_SCROLL_LAYERS;
    *used += u32::from(free);
    free
}

/// How many leading spans are well formed: at least one member, sorted, disjoint and
/// within the first `n` layers. The rest are ignored (a binder bug).
fn well_formed(spans: &[GroupSpan], n: usize) -> usize {
    let mut end = 0;
    spans
        .iter()
        .take_while(|s| {
            let ok = s.count > 0 && s.first as usize >= end && s.end() <= n;
            end = s.end();
            ok
        })
        .count()
}

/// The span layer `i` is a member of, advancing `at` along the sorted spans.
fn member_of(spans: &[GroupSpan], i: usize, at: &mut usize) -> Option<usize> {
    while spans.get(*at).is_some_and(|s| s.end() <= i) {
        *at += 1;
    }
    spans
        .get(*at)
        .filter(|s| s.first as usize <= i)
        .map(|_| *at)
}

/// The span's window on its intermediate and its Scroll frame at `at`: the picture is
/// the window, never a fill (ADR-0193).
fn span_frame(span: &GroupSpan, slot: &SpanSlot, at: Timestamp) -> Option<(UvRect, ScrollFrame)> {
    let (tw, th) = slot.size?;
    let ((x0, y0), [x, y, w, h]) = (slot.origin, span.window);
    let (fw, fh) = (tw as f32, th as f32);
    let crop = UvRect {
        u0: (x - x0) / fw,
        v0: (y - y0) / fh,
        u1: (x + w - x0) / fw,
        v1: (y + h - y0) / fh,
    };
    let frame = span
        .scroll
        .frame(&Affine2::rect(x, y, w, h), &crop, tw, th, at)?;
    Some((crop, frame))
}

/// Whether the span's Scroll still moved at the last draw.
fn span_moving(span: &GroupSpan, slot: &SpanSlot, drawn_at: Timestamp) -> bool {
    span.slide.is_some_and(|m| drawn_at <= m.start + m.duration)
        || span_frame(span, slot, drawn_at)
            .is_some_and(|(_, f)| span.scroll.moving_at(drawn_at, f.ended))
}

/// The canvas pass's layer for a span: its intermediate as one scrolling layer.
fn span_layer(
    span: &GroupSpan,
    slot: &SpanSlot,
    texture: TextureId,
    now: Timestamp,
) -> Option<Layer> {
    let (crop, f) = span_frame(span, slot, now).filter(|(_, f)| !f.ended)?;
    let sc = &span.scroll;
    Some(Layer {
        crop,
        scroll: Some(f.gpu(sc.direction, sc.repeat, sc.fade)),
        transform: span.placed(now),
        ..Layer::new(texture, 0.0, 0.0, 1.0, 1.0)
    })
}

/// Gives back the intermediates of spans that left or changed size and acquires those
/// of spans that appeared or changed size, or failed to before (ADR-0193 decision 3:
/// the real-time exception of ADR-0062 decision 5). Spans past a limit get none,
/// bottom-most first keeping theirs (decision 4).
fn size_spans(
    slots: &mut [SpanSlot; MAX_SPANS],
    spans: &[GroupSpan],
    pool: &mut TexturePool,
    gpu: &GpuContext,
    stats: &CompositorStats,
) {
    let mut total = 0;
    for (s, slot) in slots.iter_mut().enumerate() {
        let want = spans.get(s).and_then(GroupSpan::texels).filter(|t| {
            let px = u64::from(t.2) * u64::from(t.3);
            let fits = px <= MAX_SPAN_PIXELS && total + px <= MAX_SPANS_PIXELS;
            total += if fits { px } else { 0 };
            fits
        });
        let size = want.map(|t| (t.2, t.3));
        if slot.size != size {
            free_span(slot, pool);
            slot.size = size;
            slot.acquire_failed = false;
        }
        slot.origin = want.map_or((0.0, 0.0), |t| (t.0, t.1));
        if let (Some((w, h)), None, false) = (slot.size, slot.texture, slot.acquire_failed) {
            stats.span_acquires.fetch_add(1, Ordering::Relaxed);
            slot.texture = pool.acquire(gpu, &TextureDesc::canvas(w, h)).ok();
            slot.acquire_failed = slot.texture.is_none();
            slot.drawn = false;
        }
    }
}

/// Gives a span's intermediate back to the pool: forgotten first, so the release drops
/// it instead of keeping it free. Does not allocate.
fn free_span(slot: &mut SpanSlot, pool: &mut TexturePool) {
    if let Some(id) = slot.texture.take() {
        pool.forget_texture(id);
        // Only fails for an id the pool no longer knows, which is then gone already.
        let _ = pool.release(id);
    }
    slot.drawn = false;
}

/// No span fails: a list without spans, or none drawn.
fn clear_failing(stats: &CompositorStats) {
    stats.span_acquire_failing.store(0, Ordering::Relaxed);
    stats.span_pass_failing.store(0, Ordering::Relaxed);
}

fn free_spans(slots: &mut [SpanSlot; MAX_SPANS], pool: &mut TexturePool) {
    for slot in slots {
        free_span(slot, pool);
        slot.size = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::LayerShape;

    fn ms(v: u64) -> Timestamp {
        Timestamp::from_nanos(v * 1_000_000)
    }

    /// A node id for a layer that is never linked.
    fn some_node() -> NodeId {
        let mut graph = rbs_graph::description::GraphDescription::new();
        graph
            .add_node(Arc::new(compositor_descriptor()))
            .expect("an empty graph takes a node")
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
            from_crop: None,
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
            from_crop: None,
        };
        let to = Affine2::rect(200.0, 0.0, 100.0, 100.0);
        assert!((slide.at(to, ms(200)).tx - 100.0).abs() < 1e-3);
        assert!((slide.at(to, ms(100)).tx - 50.0).abs() < 1e-3);
    }

    #[test]
    fn a_sliding_circle_follows_the_box_aspect_and_the_crop_moves_with_it() {
        // ADR-0128 review (owner 2026-11-23): from a 16:9 full box, uncropped, to a square box cropped at the
        // sides. Halfway the shape's box has the drawn box's aspect, not the end box's.
        let to_box = Affine2::rect(1500.0, 800.0, 300.0, 300.0);
        let mut spec = NodeLayer::new(some_node(), 0, 1500.0, 800.0, 300.0, 300.0);
        spec.crop = PixelCrop {
            left: 400,
            top: 0,
            right: 400,
            bottom: 0,
        };
        spec.style = LayerStyle::new(LayerShape::Circle, 0.0, [0; 4], [300.0, 300.0]);
        spec.slide = Some(Slide {
            from: Affine2::rect(0.0, 0.0, 1920.0, 1080.0),
            start: ms(0),
            duration: Duration::from_millis(400),
            easing: Easing::Linear,
            from_crop: Some(PixelCrop::default()),
        });
        let (w, h) = (1920, 1080);

        let (t, crop, style) = placement(&spec, ms(0), w, h);
        assert_eq!(t, Affine2::rect(0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(crop, UvRect::FULL);
        assert_eq!(style.box_size, [300.0, 300.0]);

        let (t, crop, style) = placement(&spec, ms(200), w, h);
        let [bw, bh] = style.box_size;
        assert!(
            (bw / bh - t.a / t.d).abs() < 1e-4,
            "{bw}x{bh} vs {}x{}",
            t.a,
            t.d
        );
        // The box units per canvas pixel stay those of the end box.
        assert!((bw / t.a - 300.0 / 300.0).abs() < 1e-4);
        assert!((crop.u0 - 200.0 / 1920.0).abs() < 1e-6);
        assert!((crop.u1 - (1.0 - 200.0 / 1920.0)).abs() < 1e-6);

        let (t, crop, style) = placement(&spec, ms(400), w, h);
        assert_eq!(t, to_box);
        assert_eq!(crop, uv(spec.crop, w, h));
        assert_eq!(style, spec.style);
    }

    #[test]
    fn a_mirrored_box_keeps_a_positive_shape_size_while_it_slides() {
        // A mirrored item (negative width) has a negative column; the shape's box
        // follows the length, so its size stays as the item's own.
        let mut spec = NodeLayer::new(some_node(), 0, 600.0, 0.0, -300.0, 300.0);
        spec.style = LayerStyle::new(LayerShape::Circle, 0.0, [0; 4], [300.0, 300.0]);
        spec.slide = Some(Slide {
            from: Affine2::rect(1920.0, 0.0, -1920.0, 1080.0),
            start: ms(0),
            duration: Duration::from_millis(400),
            easing: Easing::Linear,
            from_crop: None,
        });
        let (t, crop, style) = placement(&spec, ms(200), 1920, 1080);
        assert_eq!(crop, UvRect::FULL);
        let [bw, bh] = style.box_size;
        assert!(bw > 0.0 && bh > 0.0);
        assert!((bw / bh - t.a.abs() / t.d).abs() < 1e-4);
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
