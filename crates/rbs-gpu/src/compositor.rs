//! Draws a flat list of layers into a linear `Rgba16Float` canvas.
//!
//! Hot path: [`Compositor::render`] records one render pass with one draw call per
//! layer. RBS code in it does not allocate; all pipelines, layouts and buffers are
//! created in [`Compositor::new`] and [`Compositor::reserve_layers`].
//!
//! A layer whose texture is a 4:2:0 slot (NV12 or P010, a multi-planar texture or a
//! plane pair) is sampled as it is: its own pipelines read Y' and CbCr and apply the
//! layer's [`Layer::ycbcr`] colour space, so no decoded RGB copy exists (perf audit R-11
//! step b).

use bytemuck::{Pod, Zeroable};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::math::{Affine2, UvRect};
use crate::pool::TexturePool;
use crate::scroll::ScrollGpu;
use crate::yuv_decode::{Params as YuvGpu, params as yuv_params};
use rbs_core::media::{ColorSpace, TextureId};

/// How a source texture encodes colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// Already linear light (for example an `*Srgb` texture format, decoded by the sampler).
    #[default]
    Linear,
    /// sRGB-encoded values in a non-sRGB format; decoded to linear in the shader.
    Srgb,
}

/// The size of a canvas. The format is always [`crate::CANVAS_FORMAT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasDesc {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// The outline of a layer's picture inside its box (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LayerShape {
    /// The box as it is (square corners).
    #[default]
    Box,
    /// The box with corners rounded by `radius` canvas pixels; a radius past half the
    /// shorter side is drawn as half the shorter side.
    Rounded {
        /// Corner radius in canvas pixels.
        radius: f32,
    },
    /// The largest circle centred in the box.
    Circle,
    /// The ellipse that fills the box.
    Ellipse,
    /// An isosceles triangle with its tip on the top edge and its base on the bottom.
    Triangle,
    /// An arrow pointing right: a shaft and a head, filling the box.
    Arrow,
}

/// Shape and border of a layer (ADR-0053). Lengths are in the layer's own box units:
/// the shader evaluates the shape on the unit square scaled by `box_size`, so it
/// rotates, flips, scales and shears with the picture. A layer whose style draws
/// nothing extra ([`LayerStyle::is_plain`]) is drawn by the plain pipeline, exactly as
/// without a style.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LayerStyle {
    /// The outline.
    pub shape: LayerShape,
    /// Width of the border inside the outline's edge, in box units; 0 for none.
    pub border_width: f32,
    /// Border colour, linear light, premultiplied alpha.
    pub border_color: [f32; 4],
    /// The box size in its own units (the item's layout size, before nesting).
    pub box_size: [f32; 2],
}

impl LayerStyle {
    /// A style with `shape` and a border of `width` box units in `srgb8` (sRGB,
    /// straight alpha), converted to the canvas's linear premultiplied colour, for a box
    /// of `box_size` units.
    pub fn new(shape: LayerShape, width: f32, srgb8: [u8; 4], box_size: [f32; 2]) -> Self {
        Self {
            shape,
            border_width: width,
            border_color: linear_premultiplied(srgb8),
            box_size,
        }
    }

    /// True if it draws nothing extra: a box without a border, or values that are not
    /// finite or a box with no area.
    pub fn is_plain(&self) -> bool {
        self.gpu().is_none()
    }

    /// The shader's style record, `None` when the plain pipeline draws the layer.
    fn gpu(&self) -> Option<StyleGpu> {
        let [w, h] = self.box_size;
        if !(w.is_finite() && h.is_finite() && w.abs() > 0.0 && h.abs() > 0.0) {
            return None;
        }
        let width = if self.border_width.is_finite() {
            self.border_width.max(0.0)
        } else {
            0.0
        };
        let (kind, radius) = match self.shape {
            LayerShape::Box => (1.0, 0.0),
            LayerShape::Rounded { radius } if radius.is_finite() => (1.0, radius.max(0.0)),
            LayerShape::Rounded { .. } => (1.0, 0.0),
            LayerShape::Circle => (2.0, 0.0),
            LayerShape::Ellipse => (3.0, 0.0),
            LayerShape::Triangle => (4.0, 0.0),
            LayerShape::Arrow => (5.0, 0.0),
        };
        if kind == 1.0 && radius == 0.0 && width == 0.0 {
            return None;
        }
        Some(StyleGpu {
            shape: [kind, radius, width, 0.0],
            size: [w.abs(), h.abs(), 0.0, 0.0],
            border: self.border_color,
        })
    }
}

/// An sRGB colour with straight alpha as linear light with premultiplied alpha.
pub fn linear_premultiplied(srgb8: [u8; 4]) -> [f32; 4] {
    let lin = |v: u8| {
        let c = f32::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let a = f32::from(srgb8[3]) / 255.0;
    [lin(srgb8[0]) * a, lin(srgb8[1]) * a, lin(srgb8[2]) * a, a]
}

/// One layer to draw. Source textures hold premultiplied alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    /// The source texture.
    pub texture: TextureId,
    /// Maps the unit square to canvas pixels.
    pub transform: Affine2,
    /// The part of the source to show.
    pub crop: UvRect,
    /// 0.0..=1.0; values outside are clamped.
    pub opacity: f32,
    /// How the source encodes colour.
    pub source_transfer: Transfer,
    /// The source's alpha channel carries no coverage (XRGB/BGRX layouts, whose fourth
    /// byte is undefined): every texel is drawn as fully opaque.
    pub opaque: bool,
    /// Shape and border (ADR-0053).
    pub style: LayerStyle,
    /// The colour space of a 4:2:0 texture (matrix, range, transfer, as
    /// [`crate::YuvDecoder::decode`] takes it). Ignored for RGB textures, whose colour is
    /// `source_transfer`. A colour space the decode cannot handle skips the layer.
    pub ycbcr: ColorSpace,
    /// The box is a window onto a scrolling strip of the picture (ADR-0150). Drawn by the
    /// wrap pipelines; without a [`ScrollBinding`] or the device's wrap pipelines the
    /// layer draws nothing for that call (counted), never the squeezed picture.
    pub scroll: Option<ScrollGpu>,
}

impl Layer {
    /// A fully opaque, uncropped layer filling `rect` (x, y, w, h in canvas pixels).
    pub fn new(texture: TextureId, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            texture,
            transform: Affine2::rect(x, y, w, h),
            crop: UvRect::FULL,
            opacity: 1.0,
            source_transfer: Transfer::Linear,
            opaque: false,
            style: LayerStyle::default(),
            ycbcr: ColorSpace::BT709_LIMITED,
            scroll: None,
        }
    }
}

/// What one [`Compositor::render`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// Layers drawn.
    pub drawn_layers: u32,
    /// Layers skipped because their texture was stale, unknown or not sampleable, or a
    /// 4:2:0 texture's colour space cannot be decoded (P7).
    pub skipped_layers: u32,
    /// Layers skipped because their crop leaves no picture (`u1 <= u0` or `v1 <= v0`,
    /// ADR-0058).
    pub empty_crop_layers: u32,
    /// Scrolling layers not drawn: no [`ScrollBinding`], no wrap pipelines yet, or more
    /// than [`MAX_SCROLL_LAYERS`] (ADR-0150 decision 7).
    pub unscrolled_layers: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LayerGpu {
    m: [f32; 4],
    t: [f32; 4],
    crop: [f32; 4],
    params: [f32; 4],
}

/// A styled layer's record (ADR-0053): kind (1 rectangle, 2 circle, 3 ellipse), radius
/// and border width in box units; box size; border colour (linear, premultiplied).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct StyleGpu {
    shape: [f32; 4],
    size: [f32; 4],
    border: [f32; 4],
}

const STYLE_GPU_SIZE: u64 = std::mem::size_of::<StyleGpu>() as u64;

const LAYER_GPU_SIZE: u64 = std::mem::size_of::<LayerGpu>() as u64;

const YUV_GPU_SIZE: u64 = std::mem::size_of::<YuvGpu>() as u64;

/// A drawn layer's pipeline: bit 0 styled, bit 1 4:2:0, bit 2 scrolling (wrap).
const STYLED: u8 = 1;
const YUV: u8 = 2;
const WRAP: u8 = 4;

/// The most scrolling layers one compositor draws per call (ADR-0150 decision 7).
pub const MAX_SCROLL_LAYERS: u32 = 64;

/// Bytes between two scroll records: the dynamic offset alignment WebGPU guarantees.
const SCROLL_STRIDE: u64 = 256;

const SCROLL_GPU_SIZE: u64 = std::mem::size_of::<ScrollGpu>() as u64;

/// The compositor of one canvas.
pub struct Compositor {
    canvas: CanvasDesc,
    /// Draws layers without a style (ADR-0053 decision 3: unchanged by styles).
    pipeline: wgpu::RenderPipeline,
    /// Draws styled layers: shape, border, a quad grown by one pixel.
    styled: wgpu::RenderPipeline,
    /// Draws 4:2:0 layers without a style, sampling Y' and CbCr (R-11 step b).
    yuv: wgpu::RenderPipeline,
    /// Draws styled 4:2:0 layers.
    yuv_styled: wgpu::RenderPipeline,
    layers_buffer: wgpu::Buffer,
    styles_buffer: wgpu::Buffer,
    /// One colour record per drawn layer, read only by the 4:2:0 pipelines.
    yuv_buffer: wgpu::Buffer,
    layers_bind_group: wgpu::BindGroup,
    staging: Vec<LayerGpu>,
    style_staging: Vec<StyleGpu>,
    yuv_staging: Vec<YuvGpu>,
    /// Per drawn layer: its pipeline (`STYLED`, `YUV`, `WRAP` bits). Room for `reserved`.
    kinds: Vec<u8>,
    /// Per drawn layer: its index in the call's layer slice. Room for `reserved`.
    order: Vec<u32>,
    reserved: u32,
}

fn storage_entry(binding: u32, min: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: wgpu::BufferSize::new(min),
        },
        count: None,
    }
}

/// The compositor's group 0 layout: layers (0), styles (1), 4:2:0 colour records (2).
/// One per device, on [`GpuContext`].
pub(crate) fn layers_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("rbs-gpu layers"),
        entries: &[
            storage_entry(0, LAYER_GPU_SIZE),
            storage_entry(1, STYLE_GPU_SIZE),
            storage_entry(2, YUV_GPU_SIZE),
        ],
    })
}

/// The scrolling pipelines of one device (ADR-0150 decision 7): a wrap variant of the
/// plain, styled, 4:2:0 and styled 4:2:0 pipelines, with group 2 holding one scroll
/// record bound at a dynamic offset. Built once, by
/// [`GpuContext::prepare_scrolling`].
pub(crate) struct WrapSet {
    scroll_layout: wgpu::BindGroupLayout,
    /// Indexed by `STYLED | YUV` bits.
    pipelines: [wgpu::RenderPipeline; 4],
}

impl WrapSet {
    pub(crate) fn new(ctx: &GpuContext) -> Self {
        let device = ctx.device();
        let source = concat!(
            include_str!("shaders/composite.wgsl"),
            "\n",
            include_str!("shaders/wrap.wgsl")
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu composite wrap"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let scroll_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu scroll record"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(SCROLL_GPU_SIZE),
                },
                count: None,
            }],
        });
        let rgb = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite wrap"),
            bind_group_layouts: &[
                Some(&ctx.layers_layout),
                Some(&ctx.sampled_layout),
                Some(&scroll_layout),
            ],
            immediate_size: 0,
        });
        let yuv = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite wrap 4:2:0"),
            bind_group_layouts: &[
                Some(&ctx.layers_layout),
                Some(&ctx.yuv_layout),
                Some(&scroll_layout),
            ],
            immediate_size: 0,
        });
        let pipelines = [
            pipeline(device, &rgb, &module, "rbs-gpu wrap", "vs_wrap", "fs_wrap"),
            pipeline(
                device,
                &rgb,
                &module,
                "rbs-gpu wrap styled",
                "vs_styled",
                "fs_wrap_styled",
            ),
            pipeline(
                device,
                &yuv,
                &module,
                "rbs-gpu wrap 4:2:0",
                "vs_wrap",
                "fs_yuv_wrap",
            ),
            pipeline(
                device,
                &yuv,
                &module,
                "rbs-gpu wrap 4:2:0 styled",
                "vs_styled",
                "fs_yuv_wrap_styled",
            ),
        ];
        Self {
            scroll_layout,
            pipelines,
        }
    }
}

/// One compositor's scroll records (ADR-0150 decision 7): a uniform buffer of
/// [`MAX_SCROLL_LAYERS`] records 256 bytes apart and its group 2 bind group. Built on
/// the control thread with [`ScrollBinding::new`] and handed to the compositor's node.
#[derive(Debug)]
pub struct ScrollBinding {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl ScrollBinding {
    /// Builds the scrolling pipelines of `ctx`'s device if needed
    /// ([`GpuContext::prepare_scrolling`]) and a record buffer for one compositor.
    /// Allocates and may compile shaders: control thread only.
    pub fn new(ctx: &GpuContext) -> Self {
        ctx.prepare_scrolling();
        let device = ctx.device();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu scroll records"),
            size: SCROLL_STRIDE * u64::from(MAX_SCROLL_LAYERS),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = match ctx.wrap.get() {
            Some(set) => &set.scroll_layout,
            // `prepare_scrolling` just filled it; kept total for the type system.
            None => &ctx.layers_layout,
        };
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu scroll records"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(SCROLL_GPU_SIZE),
                }),
            }],
        });
        Self { buffer, bind_group }
    }
}

fn pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    label: &str,
    vs: &str,
    fs: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(vs),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: crate::CANVAS_FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

impl Compositor {
    /// Creates the compositor for a canvas and reserves room for `max_layers` layers.
    pub fn new(ctx: &GpuContext, canvas: CanvasDesc, max_layers: u32) -> Result<Self, GpuError> {
        let max = ctx.max_texture_dimension();
        if canvas.width == 0 || canvas.height == 0 || canvas.width > max || canvas.height > max {
            return Err(GpuError::InvalidSize {
                width: canvas.width,
                height: canvas.height,
            });
        }
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/composite.wgsl").into()),
        });
        let layers_layout = &ctx.layers_layout;
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite"),
            bind_group_layouts: &[Some(layers_layout), Some(&ctx.sampled_layout)],
            immediate_size: 0,
        });
        let plain = pipeline(
            device,
            &pipeline_layout,
            &module,
            "rbs-gpu composite",
            "vs_main",
            "fs_main",
        );
        let styled = pipeline(
            device,
            &pipeline_layout,
            &module,
            "rbs-gpu composite styled",
            "vs_styled",
            "fs_styled",
        );
        let yuv_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite 4:2:0"),
            bind_group_layouts: &[Some(layers_layout), Some(&ctx.yuv_layout)],
            immediate_size: 0,
        });
        let yuv = pipeline(
            device,
            &yuv_layout,
            &module,
            "rbs-gpu composite 4:2:0",
            "vs_main",
            "fs_yuv",
        );
        let yuv_styled = pipeline(
            device,
            &yuv_layout,
            &module,
            "rbs-gpu composite 4:2:0 styled",
            "vs_styled",
            "fs_yuv_styled",
        );
        let reserved = max_layers.max(1);
        let (buffers, layers_bind_group) = create_layers_buffer(ctx, layers_layout, reserved);
        Ok(Self {
            canvas,
            pipeline: plain,
            styled,
            yuv,
            yuv_styled,
            layers_buffer: buffers.layers,
            styles_buffer: buffers.styles,
            yuv_buffer: buffers.yuv,
            layers_bind_group,
            staging: Vec::with_capacity(reserved as usize),
            style_staging: Vec::with_capacity(reserved as usize),
            yuv_staging: Vec::with_capacity(reserved as usize),
            kinds: Vec::with_capacity(reserved as usize),
            order: Vec::with_capacity(reserved as usize),
            reserved,
        })
    }

    /// The canvas this compositor renders.
    pub fn canvas(&self) -> CanvasDesc {
        self.canvas
    }

    /// Layers that fit without calling [`Compositor::reserve_layers`].
    pub fn reserved_layers(&self) -> u32 {
        self.reserved
    }

    /// Grows the layer capacity. Allocates; call from the control thread on scene change.
    pub fn reserve_layers(&mut self, ctx: &GpuContext, max_layers: u32) -> Result<(), GpuError> {
        if max_layers <= self.reserved {
            return Ok(());
        }
        let (buffers, bind_group) = create_layers_buffer(ctx, &ctx.layers_layout, max_layers);
        self.layers_buffer = buffers.layers;
        self.styles_buffer = buffers.styles;
        self.yuv_buffer = buffers.yuv;
        self.layers_bind_group = bind_group;
        self.staging.clear();
        self.staging.reserve(max_layers as usize);
        self.style_staging.clear();
        self.style_staging.reserve(max_layers as usize);
        self.yuv_staging.clear();
        self.yuv_staging.reserve(max_layers as usize);
        self.kinds.clear();
        self.kinds.reserve(max_layers as usize);
        self.order.clear();
        self.order.reserve(max_layers as usize);
        self.reserved = max_layers;
        Ok(())
    }

    /// Clears `target` to transparent black and draws `layers` bottom to top. Layers with
    /// a scroll draw nothing here (counted); see [`Compositor::render_scrolled`].
    ///
    /// Hot path. Layers whose texture is stale or not sampleable are skipped and counted.
    pub fn render(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        layers: &[Layer],
        target: TextureId,
    ) -> Result<RenderStats, GpuError> {
        self.render_scrolled(ctx, pool, layers, target, None)
    }

    /// [`Compositor::render`] with this compositor's scroll records (ADR-0150): layers
    /// with [`Layer::scroll`] are drawn by the device's wrap pipelines, each with its
    /// record. Without `scroll`, without the device's wrap pipelines
    /// ([`GpuContext::prepare_scrolling`]) or past [`MAX_SCROLL_LAYERS`], a scrolling
    /// layer draws nothing and counts in `unscrolled_layers`.
    ///
    /// Hot path: one queue write per scrolling layer, no allocation in RBS code.
    pub fn render_scrolled(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        layers: &[Layer],
        target: TextureId,
        scroll: Option<&ScrollBinding>,
    ) -> Result<RenderStats, GpuError> {
        let count = u32::try_from(layers.len()).unwrap_or(u32::MAX);
        if count > self.reserved {
            return Err(GpuError::TooManyLayers {
                requested: count,
                reserved: self.reserved,
            });
        }
        let target_tex = pool.get(target).ok_or(GpuError::StaleTexture(target))?;
        let desc = target_tex.desc();
        if desc.format != crate::CANVAS_FORMAT
            || desc.width != self.canvas.width
            || desc.height != self.canvas.height
            || !desc.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(GpuError::TargetMismatch(target));
        }
        let wrap = ctx.wrap.get();
        let records = scroll.filter(|_| wrap.is_some());

        let (w, h) = (self.canvas.width as f32, self.canvas.height as f32);
        let mut stats = RenderStats::default();
        // Capacity is `reserved`, so these pushes never allocate.
        self.staging.clear();
        self.style_staging.clear();
        self.yuv_staging.clear();
        self.kinds.clear();
        self.order.clear();
        let mut any_yuv = false;
        let mut scrolled = 0u32;
        for (index, layer) in layers.iter().enumerate() {
            let Some((_, colour)) = sampled(pool, layer) else {
                stats.skipped_layers += 1;
                continue;
            };
            let t = &layer.transform;
            let c = &layer.crop;
            // A crop larger than the picture (a window that shrank under it) draws
            // nothing rather than a mirrored strip (ADR-0058).
            if !has_picture(c) {
                stats.empty_crop_layers += 1;
                continue;
            }
            let mut kind = 0;
            if let Some(record) = layer.scroll {
                // Draws nothing rather than the whole picture squeezed into the window.
                let Some(binding) = records.filter(|_| scrolled < MAX_SCROLL_LAYERS) else {
                    stats.unscrolled_layers += 1;
                    continue;
                };
                ctx.queue().write_buffer(
                    &binding.buffer,
                    u64::from(scrolled) * SCROLL_STRIDE,
                    bytemuck::bytes_of(&record),
                );
                scrolled += 1;
                kind |= WRAP;
            }
            // A singular transform (a nested scene of no size) has no pixel to grow
            // the quad by: the plain pipeline draws it, which draws nothing.
            let singular = (t.a * t.d - t.b * t.c).abs() < 1e-6;
            let style = if singular { None } else { layer.style.gpu() };
            // Styled layers name their style record; the plain shader never reads it.
            let style_index = match style {
                Some(record) => {
                    self.style_staging.push(record);
                    (self.style_staging.len() - 1) as f32
                }
                None => 0.0,
            };
            if style.is_some() {
                kind |= STYLED;
            }
            if let Some(record) = colour {
                kind |= YUV;
                any_yuv = true;
                self.yuv_staging.push(record);
            } else {
                // Keeps the records in step with the instances; never read.
                self.yuv_staging.push(YuvGpu::zeroed());
            }
            self.kinds.push(kind);
            self.order.push(index as u32);
            self.staging.push(LayerGpu {
                m: [t.a, t.b, t.c, t.d],
                t: [t.tx, t.ty, w, h],
                crop: [c.u0, c.v0, c.u1, c.v1],
                params: [
                    layer.opacity.clamp(0.0, 1.0),
                    if layer.source_transfer == Transfer::Srgb {
                        1.0
                    } else {
                        0.0
                    },
                    if layer.opaque { 1.0 } else { 0.0 },
                    style_index,
                ],
            });
        }
        stats.drawn_layers = self.staging.len() as u32;
        if !self.staging.is_empty() {
            ctx.queue()
                .write_buffer(&self.layers_buffer, 0, bytemuck::cast_slice(&self.staging));
        }
        if any_yuv {
            ctx.queue()
                .write_buffer(&self.yuv_buffer, 0, bytemuck::cast_slice(&self.yuv_staging));
        }
        if !self.style_staging.is_empty() {
            ctx.queue().write_buffer(
                &self.styles_buffer,
                0,
                bytemuck::cast_slice(&self.style_staging),
            );
        }

        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu composite"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_tex.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.layers_bind_group, &[]);
            let mut bound = 0u8;
            let mut slot = 0u32;
            for (instance, (&kind, &index)) in self.kinds.iter().zip(&self.order).enumerate() {
                let Some((bind_group, _)) = layers
                    .get(index as usize)
                    .and_then(|layer| sampled(pool, layer))
                else {
                    continue;
                };
                if kind != bound {
                    let wrap_pipeline =
                        |bits: u8| wrap.map(|set| &set.pipelines[usize::from(bits & 3)]);
                    let next = match kind {
                        0 => Some(&self.pipeline),
                        STYLED => Some(&self.styled),
                        YUV => Some(&self.yuv),
                        3 => Some(&self.yuv_styled),
                        bits => wrap_pipeline(bits),
                    };
                    let Some(next) = next else {
                        continue;
                    };
                    pass.set_pipeline(next);
                    bound = kind;
                }
                pass.set_bind_group(1, bind_group, &[]);
                if kind & WRAP != 0 {
                    let Some(binding) = records else {
                        continue;
                    };
                    let offset = slot * SCROLL_STRIDE as u32;
                    pass.set_bind_group(2, &binding.bind_group, &[offset]);
                    slot += 1;
                }
                let instance = instance as u32;
                pass.draw(0..6, instance..instance + 1);
            }
        }
        ctx.queue().submit(Some(encoder.finish()));
        Ok(stats)
    }
}

/// True when `crop` leaves an area of the texture; `!(a > b)` is also true for NaN.
fn has_picture(crop: &UvRect) -> bool {
    crop.u1 > crop.u0 && crop.v1 > crop.v0
}

/// The bind group a layer draws with and, for a 4:2:0 texture, its colour record; `None`
/// when the layer cannot be drawn. Pure arithmetic for the record: no allocation.
fn sampled<'p>(
    pool: &'p TexturePool,
    layer: &Layer,
) -> Option<(&'p wgpu::BindGroup, Option<YuvGpu>)> {
    let tex = pool.get(layer.texture)?;
    if let Some(group) = tex.sampled_bind_group() {
        return Some((group, None));
    }
    let group = tex.yuv_bind_group()?;
    let bits = match tex.desc().format {
        wgpu::TextureFormat::NV12 => 8,
        wgpu::TextureFormat::P010 => 10,
        _ => return None,
    };
    let record = yuv_params(layer.ycbcr, bits).ok()?;
    Some((group, Some(record)))
}

/// The compositor's per-layer storage buffers.
struct LayerBuffers {
    layers: wgpu::Buffer,
    styles: wgpu::Buffer,
    yuv: wgpu::Buffer,
}

fn create_layers_buffer(
    ctx: &GpuContext,
    layout: &wgpu::BindGroupLayout,
    max_layers: u32,
) -> (LayerBuffers, wgpu::BindGroup) {
    let buffer = |label: &str, size: u64| {
        ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size * u64::from(max_layers),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    let layers = buffer("rbs-gpu layers", LAYER_GPU_SIZE);
    let styles = buffer("rbs-gpu layer styles", STYLE_GPU_SIZE);
    let yuv = buffer("rbs-gpu layer 4:2:0 colour", YUV_GPU_SIZE);
    let bind_group = ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("rbs-gpu layers"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: layers.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: styles.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: yuv.as_entire_binding(),
            },
        ],
    });
    (
        LayerBuffers {
            layers,
            styles,
            yuv,
        },
        bind_group,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: [f32; 2] = [320.0, -180.0];

    fn shape(s: LayerStyle) -> [f32; 4] {
        s.gpu().map(|g| g.shape).unwrap_or([0.0; 4])
    }

    #[test]
    fn a_plain_box_takes_the_plain_pipeline() {
        assert!(LayerStyle::default().is_plain());
        let s = LayerStyle::new(LayerShape::Rounded { radius: 0.0 }, 0.0, [0; 4], BOX);
        assert!(s.is_plain());
        let s = LayerStyle::new(
            LayerShape::Rounded { radius: f32::NAN },
            f32::INFINITY,
            [0; 4],
            BOX,
        );
        assert!(s.is_plain());
        // A circle in a box with no area draws nothing extra either.
        assert!(LayerStyle::new(LayerShape::Circle, 0.0, [0; 4], [0.0, 10.0]).is_plain());
    }

    #[test]
    fn shapes_and_borders_reach_the_shader() {
        let s = LayerStyle::new(LayerShape::Rounded { radius: 12.0 }, 0.0, [0; 4], BOX);
        assert_eq!(shape(s), [1.0, 12.0, 0.0, 0.0]);
        assert_eq!(s.gpu().map(|g| g.size), Some([320.0, 180.0, 0.0, 0.0]));
        let s = LayerStyle::new(LayerShape::Box, 4.0, [255; 4], BOX);
        assert_eq!(shape(s), [1.0, 0.0, 4.0, 0.0]);
        let s = LayerStyle::new(LayerShape::Circle, -3.0, [255; 4], BOX);
        assert_eq!(shape(s), [2.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            shape(LayerStyle::new(LayerShape::Ellipse, 0.0, [0; 4], BOX))[0],
            3.0
        );
        assert_eq!(
            shape(LayerStyle::new(LayerShape::Triangle, 0.0, [0; 4], BOX))[0],
            4.0
        );
        assert_eq!(
            shape(LayerStyle::new(LayerShape::Arrow, 0.0, [0; 4], BOX))[0],
            5.0
        );
        assert!(!LayerStyle::new(LayerShape::Triangle, 0.0, [0; 4], BOX).is_plain());
        assert!(!LayerStyle::new(LayerShape::Arrow, 0.0, [0; 4], BOX).is_plain());
    }

    #[test]
    fn border_colours_are_linear_and_premultiplied() {
        assert_eq!(linear_premultiplied([255, 255, 255, 255]), [1.0; 4]);
        let [r, g, b, a] = linear_premultiplied([255, 0, 188, 128]);
        assert!((a - 128.0 / 255.0).abs() < 1e-6);
        assert!((r - a).abs() < 1e-6 && g == 0.0);
        // sRGB 188 is about 0.5 linear.
        assert!((b / a - 0.5).abs() < 0.01);
    }
}
