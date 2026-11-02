//! Canvas to NV12 conversion for encoders (BT.709, limited range).
//!
//! Hot path: [`Nv12Converter::convert`] records two render passes (luma at full size,
//! chroma at half size) and submits them. It does not allocate in RBS code. Render
//! passes are used instead of a compute pass because `R8Unorm` and `Rg8Unorm` are
//! render targets on every backend, while storage writes to them need optional features.

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TextureId, TexturePool};

/// Description of the NV12 luma plane: `R8Unorm`, `width x height`.
pub fn luma_desc(width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format: wgpu::TextureFormat::R8Unorm,
        usage: plane_usage(),
    }
}

/// Description of the NV12 chroma plane: `Rg8Unorm` (Cb, Cr), `width/2 x height/2`.
pub fn chroma_desc(width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width: width / 2,
        height: height / 2,
        format: wgpu::TextureFormat::Rg8Unorm,
        usage: plane_usage(),
    }
}

fn plane_usage() -> wgpu::TextureUsages {
    wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC
}

/// Converts canvases of one size to NV12 planes.
pub struct Nv12Converter {
    width: u32,
    height: u32,
    luma: wgpu::RenderPipeline,
    chroma: wgpu::RenderPipeline,
}

impl Nv12Converter {
    /// Creates a converter for `width x height` canvases. Both must be even and non-zero.
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Result<Self, GpuError> {
        let max = ctx.max_texture_dimension();
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > max
            || height > max
        {
            return Err(GpuError::InvalidSize { width, height });
        }
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu nv12"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/nv12.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu nv12"),
            bind_group_layouts: &[Some(&ctx.sampled_layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str, format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("rbs-gpu nv12"),
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
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        Ok(Self {
            width,
            height,
            luma: pipeline("fs_luma", wgpu::TextureFormat::R8Unorm),
            chroma: pipeline("fs_chroma", wgpu::TextureFormat::Rg8Unorm),
        })
    }

    /// Writes the NV12 planes of `canvas` into `luma` and `chroma`.
    ///
    /// Hot path. The planes must match [`luma_desc`] and [`chroma_desc`] for this size.
    pub fn convert(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        luma: TextureId,
        chroma: TextureId,
    ) -> Result<(), GpuError> {
        let canvas_tex = pool.get(canvas).ok_or(GpuError::StaleTexture(canvas))?;
        let d = canvas_tex.desc();
        let source = canvas_tex
            .sampled_bind_group()
            .filter(|_| {
                d.format == crate::CANVAS_FORMAT && d.width == self.width && d.height == self.height
            })
            .ok_or(GpuError::TargetMismatch(canvas))?;
        let luma_tex = plane(pool, luma, &luma_desc(self.width, self.height))?;
        let chroma_tex = plane(pool, chroma, &chroma_desc(self.width, self.height))?;

        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu nv12"),
            });
        for (pipeline, view) in [(&self.luma, luma_tex), (&self.chroma, chroma_tex)] {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu nv12"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, source, &[]);
            pass.draw(0..3, 0..1);
        }
        ctx.queue().submit(Some(encoder.finish()));
        Ok(())
    }
}

fn plane<'p>(
    pool: &'p TexturePool,
    id: TextureId,
    expected: &TextureDesc,
) -> Result<&'p wgpu::TextureView, GpuError> {
    let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
    let d = tex.desc();
    if d.format != expected.format
        || d.width != expected.width
        || d.height != expected.height
        || !d.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err(GpuError::TargetMismatch(id));
    }
    Ok(tex.view())
}
