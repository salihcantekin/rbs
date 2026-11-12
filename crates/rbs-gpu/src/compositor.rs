//! Draws a flat list of layers into a linear `Rgba16Float` canvas.
//!
//! Hot path: [`Compositor::render`] records one render pass with one draw call per
//! layer. RBS code in it does not allocate; all pipelines, layouts and buffers are
//! created in [`Compositor::new`] and [`Compositor::reserve_layers`].

use bytemuck::{Pod, Zeroable};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::math::{Affine2, UvRect};
use crate::pool::TexturePool;
use rbs_core::media::TextureId;

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
        }
    }
}

/// What one [`Compositor::render`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// Layers drawn.
    pub drawn_layers: u32,
    /// Layers skipped because their texture was stale, unknown or not sampleable (P7).
    pub skipped_layers: u32,
    /// Layers skipped because their crop leaves no picture (`u1 <= u0` or `v1 <= v0`,
    /// ADR-0058).
    pub empty_crop_layers: u32,
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

/// The compositor of one canvas.
pub struct Compositor {
    canvas: CanvasDesc,
    /// Draws layers without a style (ADR-0053 decision 3: unchanged by styles).
    pipeline: wgpu::RenderPipeline,
    /// Draws styled layers: shape, border, a quad grown by one pixel.
    styled: wgpu::RenderPipeline,
    layers_layout: wgpu::BindGroupLayout,
    layers_buffer: wgpu::Buffer,
    styles_buffer: wgpu::Buffer,
    layers_bind_group: wgpu::BindGroup,
    staging: Vec<LayerGpu>,
    style_staging: Vec<StyleGpu>,
    /// Per drawn layer: drawn by the styled pipeline. Room for `reserved`.
    styled_flags: Vec<bool>,
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
        let layers_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu layers"),
            entries: &[
                storage_entry(0, LAYER_GPU_SIZE),
                storage_entry(1, STYLE_GPU_SIZE),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite"),
            bind_group_layouts: &[Some(&layers_layout), Some(&ctx.sampled_layout)],
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
        let reserved = max_layers.max(1);
        let (layers_buffer, styles_buffer, layers_bind_group) =
            create_layers_buffer(ctx, &layers_layout, reserved);
        Ok(Self {
            canvas,
            pipeline: plain,
            styled,
            layers_layout,
            layers_buffer,
            styles_buffer,
            layers_bind_group,
            staging: Vec::with_capacity(reserved as usize),
            style_staging: Vec::with_capacity(reserved as usize),
            styled_flags: Vec::with_capacity(reserved as usize),
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
        let (layers, styles, bind_group) =
            create_layers_buffer(ctx, &self.layers_layout, max_layers);
        self.layers_buffer = layers;
        self.styles_buffer = styles;
        self.layers_bind_group = bind_group;
        self.staging.clear();
        self.staging.reserve(max_layers as usize);
        self.style_staging.clear();
        self.style_staging.reserve(max_layers as usize);
        self.styled_flags.clear();
        self.styled_flags.reserve(max_layers as usize);
        self.reserved = max_layers;
        Ok(())
    }

    /// Clears `target` to transparent black and draws `layers` bottom to top.
    ///
    /// Hot path. Layers whose texture is stale or not sampleable are skipped and counted.
    pub fn render(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        layers: &[Layer],
        target: TextureId,
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

        let (w, h) = (self.canvas.width as f32, self.canvas.height as f32);
        let mut stats = RenderStats::default();
        // Capacity is `reserved`, so these pushes never allocate.
        self.staging.clear();
        self.style_staging.clear();
        self.styled_flags.clear();
        for layer in layers {
            if sampled(pool, layer.texture).is_none() {
                stats.skipped_layers += 1;
                continue;
            }
            let t = &layer.transform;
            let c = &layer.crop;
            // A crop larger than the picture (a window that shrank under it) draws
            // nothing rather than a mirrored strip (ADR-0058).
            if !has_picture(c) {
                stats.empty_crop_layers += 1;
                continue;
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
            self.styled_flags.push(style.is_some());
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
            let mut instance = 0u32;
            let mut bound_styled = false;
            for layer in layers {
                let Some(bind_group) = sampled(pool, layer.texture) else {
                    continue;
                };
                // Skipped in the loop above, so the record indices stay in step.
                if !has_picture(&layer.crop) {
                    continue;
                }
                let styled = self.styled_flags[instance as usize];
                if styled != bound_styled {
                    pass.set_pipeline(if styled { &self.styled } else { &self.pipeline });
                    bound_styled = styled;
                }
                pass.set_bind_group(1, bind_group, &[]);
                pass.draw(0..6, instance..instance + 1);
                instance += 1;
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

fn sampled(pool: &TexturePool, id: TextureId) -> Option<&wgpu::BindGroup> {
    pool.get(id)?.sampled_bind_group()
}

fn create_layers_buffer(
    ctx: &GpuContext,
    layout: &wgpu::BindGroupLayout,
    max_layers: u32,
) -> (wgpu::Buffer, wgpu::Buffer, wgpu::BindGroup) {
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
        ],
    });
    (layers, styles, bind_group)
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
