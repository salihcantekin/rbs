//! The render executor's runtime and the compositor node of the media graph
//! (spec `docs/specs/gpu-compositor.md`, "Graph runtime and compositor node"; ADR-0020).

use std::time::Duration;

use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, TransferFunction, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_graph::description::NodeId;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{self, ParamReceiver, ParamSender};
use rbs_graph::runtime::{Inputs, Node, NodeError, PrepareCtx, ProcessCtx, Runtime, Tick, WarmCtx};

use crate::compositor::{CanvasDesc, Compositor, Layer, LayerStyle, Transfer};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::Importer;
use crate::math::{Affine2, UvRect};
use crate::pool::{TextureDesc, TexturePool};

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

/// A linear opacity animation, evaluated by the node against tick time (ADR-0023,
/// decision 7): `from` before `start`, the layer's `opacity` after `start + duration`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fade {
    /// Opacity at and before `start`.
    pub from: f32,
    /// Engine time the fade starts.
    pub start: Timestamp,
    /// Length of the fade; zero jumps to the end value at `start`.
    pub duration: Duration,
}

impl Fade {
    /// The opacity at `now` for a fade ending at `to`.
    pub fn at(&self, to: f32, now: Timestamp) -> f32 {
        if now <= self.start {
            return self.from;
        }
        let elapsed = now.saturating_since(self.start);
        if elapsed >= self.duration {
            return to;
        }
        let t = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        self.from + (to - self.from) * t as f32
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
    /// Ignore the source's alpha (XRGB sources).
    pub opaque: bool,
    /// Shape and border (ADR-0053).
    pub style: LayerStyle,
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
            opaque: false,
            style: LayerStyle::default(),
        }
    }
}

/// The control-thread end of a compositor node's layer list.
pub type LayerSender = ParamSender<Vec<NodeLayer>>;

/// The link of `layer` on the layers pin: the hint if it still names the producer,
/// otherwise a lookup.
fn link_of(inputs: &Inputs<'_>, layer: &NodeLayer) -> Option<usize> {
    if inputs.source(LAYERS_PIN, layer.link) == Some(layer.producer) {
        Some(layer.link)
    } else {
        inputs.find(LAYERS_PIN, layer.producer)
    }
}

/// Draws the frames on its "layers" links into a canvas and publishes it on "canvas".
pub struct CompositorNode {
    descriptor: NodeDescriptor,
    canvas: CanvasDesc,
    capacity: u32,
    layers: ParamReceiver<Vec<NodeLayer>>,
    /// Built in `prepare`, dropped in `release`.
    compositor: Option<Compositor>,
    /// Reused every tick; room for `capacity` layers.
    scratch: Vec<Layer>,
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
        };
        (node, sender)
    }

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

impl Node<GpuRuntime> for CompositorNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let compositor = Compositor::new(ctx.shared, self.canvas, self.capacity)
            .map_err(|e| NodeError::Failed(e.to_string()))?;
        self.compositor = Some(compositor);
        self.scratch = Vec::with_capacity(self.capacity as usize);
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

        self.layers.update();
        self.scratch.clear();
        for spec in self.layers.get().iter().take(self.capacity as usize) {
            let Some(link) = link_of(&inputs, spec) else {
                continue;
            };
            let Some(frame) = inputs.video(LAYERS_PIN, link) else {
                continue;
            };
            let FramePayload::Texture(id) = *frame.payload() else {
                continue;
            };
            let Some(tex) = local.pool.get(id) else {
                continue;
            };
            let d = tex.desc();
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
                transform: spec.transform,
                crop: uv(spec.crop, d.width, d.height),
                opacity: match &spec.fade {
                    Some(fade) => fade.at(spec.opacity, tick.time),
                    None => spec.opacity,
                },
                source_transfer,
                opaque: spec.opaque,
                style: spec.style,
            });
        }

        let canvas = local.pool.acquire(gpu, &desc).map_err(backend)?;
        let drawn = compositor.render(gpu, &local.pool, &self.scratch, canvas);
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
        // Dropping the previous frame only releases its lease.
        drop(outputs.set_video(CANVAS_PIN, Some(frame.with_sequence(tick.index)))?);
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.compositor = None;
        self.scratch = Vec::new();
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
        };
        assert_eq!(fade.at(1.0, ms(0)), 0.2);
        assert_eq!(fade.at(1.0, ms(100)), 0.2);
        assert!((fade.at(1.0, ms(200)) - 0.6).abs() < 1e-6);
        assert_eq!(fade.at(1.0, ms(300)), 1.0);
        assert_eq!(fade.at(1.0, ms(10_000)), 1.0);
    }

    #[test]
    fn zero_length_fade_jumps_at_start() {
        let fade = Fade {
            from: 1.0,
            start: ms(50),
            duration: Duration::ZERO,
        };
        assert_eq!(fade.at(0.0, ms(50)), 1.0);
        assert_eq!(fade.at(0.0, ms(51)), 0.0);
    }
}
