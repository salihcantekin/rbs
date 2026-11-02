//! The render executor's runtime and the compositor node of the media graph
//! (spec `docs/specs/gpu-compositor.md`, "Graph runtime and compositor node"; ADR-0020).

use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, TransferFunction, VideoFormat};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime, Tick, WarmCtx};

use crate::compositor::{CanvasDesc, Compositor, Layer, Transfer};
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

/// One layer of a compositor node, bottom first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeLayer {
    /// Index of the link on the "layers" pin (connection order).
    pub link: usize,
    /// Maps the unit square of the cropped source to canvas pixels.
    pub transform: Affine2,
    /// Crop in source pixels.
    pub crop: PixelCrop,
    /// 0.0..=1.0.
    pub opacity: f32,
    /// Ignore the source's alpha (XRGB sources).
    pub opaque: bool,
}

impl NodeLayer {
    /// An uncropped, opaque layer of link `link` filling `rect` (x, y, w, h).
    pub fn new(link: usize, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            link,
            transform: Affine2::rect(x, y, w, h),
            crop: PixelCrop::default(),
            opacity: 1.0,
            opaque: false,
        }
    }
}

/// Draws the frames on its "layers" links into a canvas and publishes it on "canvas".
pub struct CompositorNode {
    descriptor: NodeDescriptor,
    canvas: CanvasDesc,
    layers: Vec<NodeLayer>,
    /// Built in `prepare`, dropped in `release`.
    compositor: Option<Compositor>,
    /// Reused every tick; capacity reserved for `layers`.
    scratch: Vec<Layer>,
}

impl CompositorNode {
    /// A compositor for `canvas` drawing `layers`.
    pub fn new(canvas: CanvasDesc, layers: Vec<NodeLayer>) -> Self {
        Self {
            descriptor: compositor_descriptor(),
            canvas,
            layers,
            compositor: None,
            scratch: Vec::new(),
        }
    }

    /// Replaces the layer list. Only while the node is idle (control thread); a running
    /// node gets its list through the graph runtime's parameter channel.
    pub fn set_layers(&mut self, layers: Vec<NodeLayer>) {
        self.layers = layers;
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
        let max = u32::try_from(self.layers.len().max(1))
            .map_err(|_| NodeError::Failed("too many layers".into()))?;
        let compositor = Compositor::new(ctx.shared, self.canvas, max)
            .map_err(|e| NodeError::Failed(e.to_string()))?;
        self.compositor = Some(compositor);
        self.scratch = Vec::with_capacity(self.layers.len());
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

        self.scratch.clear();
        for spec in &self.layers {
            let Some(frame) = inputs.video(LAYERS_PIN, spec.link) else {
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
            // Capacity was reserved in `prepare`, so this push does not allocate.
            self.scratch.push(Layer {
                texture: id,
                transform: spec.transform,
                crop: uv(spec.crop, d.width, d.height),
                opacity: spec.opacity,
                source_transfer,
                opaque: spec.opaque,
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
