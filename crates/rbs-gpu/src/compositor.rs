//! Draws a flat list of layers into a linear `Rgba16Float` canvas.
//!
//! Hot path: [`Compositor::render`] records one render pass with one draw call per
//! layer. RBS code in it does not allocate; all pipelines, layouts and buffers are
//! created in [`Compositor::new`] and [`Compositor::reserve_layers`].

use bytemuck::{Pod, Zeroable};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::math::{Affine2, UvRect};
use crate::pool::{TextureId, TexturePool};

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
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LayerGpu {
    m: [f32; 4],
    t: [f32; 4],
    crop: [f32; 4],
    params: [f32; 4],
}

const LAYER_GPU_SIZE: u64 = std::mem::size_of::<LayerGpu>() as u64;

/// The compositor of one canvas.
pub struct Compositor {
    canvas: CanvasDesc,
    pipeline: wgpu::RenderPipeline,
    layers_layout: wgpu::BindGroupLayout,
    layers_buffer: wgpu::Buffer,
    layers_bind_group: wgpu::BindGroup,
    staging: Vec<LayerGpu>,
    reserved: u32,
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
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(LAYER_GPU_SIZE),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu composite"),
            bind_group_layouts: &[Some(&layers_layout), Some(&ctx.sampled_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rbs-gpu composite"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: crate::CANVAS_FORMAT,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let (layers_buffer, layers_bind_group) =
            create_layers_buffer(ctx, &layers_layout, max_layers.max(1));
        Ok(Self {
            canvas,
            pipeline,
            layers_layout,
            layers_buffer,
            layers_bind_group,
            staging: Vec::with_capacity(max_layers.max(1) as usize),
            reserved: max_layers.max(1),
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
        let (buffer, bind_group) = create_layers_buffer(ctx, &self.layers_layout, max_layers);
        self.layers_buffer = buffer;
        self.layers_bind_group = bind_group;
        self.staging.clear();
        self.staging.reserve(max_layers as usize);
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
        for layer in layers {
            if sampled(pool, layer.texture).is_none() {
                stats.skipped_layers += 1;
                continue;
            }
            let t = &layer.transform;
            let c = &layer.crop;
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
                    0.0,
                    0.0,
                ],
            });
        }
        stats.drawn_layers = self.staging.len() as u32;
        if !self.staging.is_empty() {
            ctx.queue()
                .write_buffer(&self.layers_buffer, 0, bytemuck::cast_slice(&self.staging));
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
            for layer in layers {
                let Some(bind_group) = sampled(pool, layer.texture) else {
                    continue;
                };
                pass.set_bind_group(1, bind_group, &[]);
                pass.draw(0..6, instance..instance + 1);
                instance += 1;
            }
        }
        ctx.queue().submit(Some(encoder.finish()));
        Ok(stats)
    }
}

fn sampled(pool: &TexturePool, id: TextureId) -> Option<&wgpu::BindGroup> {
    pool.get(id)?.sampled_bind_group()
}

fn create_layers_buffer(
    ctx: &GpuContext,
    layout: &wgpu::BindGroupLayout,
    max_layers: u32,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("rbs-gpu layers"),
        size: LAYER_GPU_SIZE * u64::from(max_layers),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("rbs-gpu layers"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    (buffer, bind_group)
}
