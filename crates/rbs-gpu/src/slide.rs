//! A change between two slide show pictures (ADR-0171 decision 7; spec
//! `docs/specs/gpu-compositor.md`, "Slide show change").
//!
//! [`ReservedTargets`] holds two render targets of the slide show's output size, in the
//! node's own format (`Rgba8Unorm` holding sRGB codes with straight alpha, as the still
//! and text uploads), reserved from `warm` to `cool` like [`crate::upload::ReservedUpload`].
//! [`SlidePass`] draws the outgoing and incoming pictures into the one no reader leases:
//! a fade, a slide (the incoming picture pushes the outgoing one out to the left) or a
//! swipe (the incoming picture moves in over the outgoing one).
//!
//! Hot path while a change runs: one uniform write, one render pass and one submit per
//! tick, no allocation in RBS code (wgpu records the command buffer). Built only when the
//! source's transition is not a cut; nothing runs between changes.

use rbs_core::media::{ColorSpace, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use rbs_core::media::TextureId;

/// How the incoming picture replaces the outgoing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideKind {
    /// The incoming picture fades in over the outgoing one.
    Fade,
    /// The incoming picture comes in from the right and pushes the outgoing one out.
    Slide,
    /// The incoming picture comes in from the right over the outgoing one.
    Swipe,
}

impl SlideKind {
    fn code(self) -> u32 {
        match self {
            Self::Fade => 0,
            Self::Slide => 1,
            Self::Swipe => 2,
        }
    }
}

/// The texture format of slide show pictures and targets.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn target_desc(width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
    }
}

/// Two render targets of one size, kept from `warm` to `cool`.
#[derive(Debug)]
pub struct ReservedTargets {
    ids: [TextureId; 2],
    width: u32,
    height: u32,
}

impl ReservedTargets {
    /// Acquires two `width` x `height` targets. Not real-time (`warm`).
    pub fn reserve(
        ctx: &GpuContext,
        pool: &mut TexturePool,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::InvalidSize { width, height });
        }
        let desc = target_desc(width, height);
        let first = pool.acquire(ctx, &desc)?;
        let second = match pool.acquire(ctx, &desc) {
            Ok(id) => id,
            Err(e) => {
                let _ = pool.release(first);
                pool.forget_texture(first);
                return Err(e);
            }
        };
        Ok(Self {
            ids: [first, second],
            width,
            height,
        })
    }

    /// Width and height of the targets.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The target no reader leases, or `None` when both are leased (the caller skips
    /// this tick's draw).
    pub fn free_target(&self, pool: &TexturePool) -> Option<TextureId> {
        self.ids
            .iter()
            .copied()
            .find(|id| pool.lease_count(*id) == 0)
    }

    /// Gives both targets back to the pool and forgets them.
    pub fn give_back(self, pool: &mut TexturePool) {
        for id in self.ids {
            let _ = pool.release(id);
            pool.forget_texture(id);
        }
    }
}

/// The pass that draws a change.
#[derive(Debug)]
pub struct SlidePass {
    pipeline: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    params_group: wgpu::BindGroup,
}

impl SlidePass {
    /// Builds the pipeline and its uniform buffer. Not real-time (`warm`).
    pub fn new(ctx: &GpuContext) -> Self {
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu slide"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/slide.wgsl").into()),
        });
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu slide params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu slide"),
            bind_group_layouts: &[
                Some(&ctx.sampled_layout),
                Some(&ctx.sampled_layout),
                Some(&params_layout),
            ],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rbs-gpu slide"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fullscreen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_slide"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu slide params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu slide params"),
            layout: &params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            params,
            params_group,
        }
    }

    /// Draws the change from `from` to `to` at progress `t` (0 to 1) into a free target
    /// of `targets` and returns a frame of it at `time`. `Ok(None)` when both targets are
    /// leased. Both pictures must be the targets' size, single-plane `Rgba8Unorm`.
    ///
    /// Hot path: no allocation in RBS code.
    #[allow(clippy::too_many_arguments)] // One call per tick; a struct would only rename them.
    pub fn draw(
        &self,
        ctx: &GpuContext,
        pool: &TexturePool,
        targets: &ReservedTargets,
        from: TextureId,
        to: TextureId,
        kind: SlideKind,
        t: f32,
        time: Timestamp,
    ) -> Result<Option<VideoFrame>, GpuError> {
        let Some(target) = targets.free_target(pool) else {
            return Ok(None);
        };
        let size = targets.size();
        let picture = |id: TextureId| {
            let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
            let d = tex.desc();
            if d.format != FORMAT || (d.width, d.height) != size {
                return Err(GpuError::TargetMismatch(id));
            }
            tex.sampled_bind_group().ok_or(GpuError::TargetMismatch(id))
        };
        let (from_group, to_group) = (picture(from)?, picture(to)?);
        let target_tex = pool.get(target).ok_or(GpuError::StaleTexture(target))?;
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&kind.code().to_le_bytes());
        bytes[4..8].copy_from_slice(&t.clamp(0.0, 1.0).to_le_bytes());
        bytes[8..12].copy_from_slice(&(size.0 as f32).to_le_bytes());
        ctx.queue().write_buffer(&self.params, 0, &bytes);
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu slide"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu slide"),
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
            pass.set_bind_group(0, from_group, &[]);
            pass.set_bind_group(1, to_group, &[]);
            pass.set_bind_group(2, &self.params_group, &[]);
            pass.draw(0..3, 0..1);
        }
        ctx.queue().submit(std::iter::once(encoder.finish()));
        let format = VideoFormat::new(
            Size::new(size.0, size.1),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        pool.frame(target, format, time).map(Some)
    }
}
