//! Canvas to NV12 conversion for encoders (BT.709, limited range).
//!
//! Two outputs: one multi-planar NV12 texture ([`surface_desc`],
//! [`Nv12Converter::convert_to_surface`]) where the device supports the format
//! (capability `gpu.nv12_surface`), which hardware encoders consume directly, or two plane
//! textures ([`luma_desc`], [`chroma_desc`], [`Nv12Converter::convert`]) everywhere else.
//!
//! Hot path: both conversions record two render passes (luma at full size,
//! chroma at half size) and submits them. It does not allocate in RBS code. Render
//! passes are used instead of a compute pass because `R8Unorm` and `Rg8Unorm` are
//! render targets on every backend, while storage writes to them need optional features.

use crate::context::{CAP_NV12_SURFACE, CAP_SOFTWARE, GpuContext};
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use rbs_core::media::TextureId;

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

/// Description of a single NV12 texture with both planes, for encoders.
///
/// Needs `wgpu::Features::TEXTURE_FORMAT_NV12` (capability `gpu.nv12_surface`).
pub fn surface_desc(width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format: wgpu::TextureFormat::NV12,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
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
    /// Software adapters only: planes and a buffer for the staged NV12 surface path.
    staged: Option<Staged>,
}

/// Intermediate planes for adapters where rendering into NV12 plane views is not
/// reliable (software adapters: lavapipe writes plane 1 over plane 0). The planes are
/// rendered as usual and copied into the NV12 texture through one GPU buffer; nothing
/// reaches the CPU.
struct Staged {
    luma: wgpu::Texture,
    luma_view: wgpu::TextureView,
    chroma: wgpu::Texture,
    chroma_view: wgpu::TextureView,
    buffer: wgpu::Buffer,
    /// Bytes per row in `buffer`, aligned for copies (luma and chroma rows are equally wide).
    pitch: u32,
}

impl Staged {
    fn new(ctx: &GpuContext, width: u32, height: u32) -> Self {
        let plane = |d: TextureDesc| {
            let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("rbs-gpu nv12 staged plane"),
                size: wgpu::Extent3d {
                    width: d.width,
                    height: d.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: d.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view)
        };
        let (luma, luma_view) = plane(luma_desc(width, height));
        let (chroma, chroma_view) = plane(chroma_desc(width, height));
        // A luma row is `width` bytes; a chroma row is `width / 2` texels of 2 bytes.
        let pitch =
            width.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu nv12 staged"),
            size: u64::from(pitch) * u64::from(height + height / 2),
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            luma,
            luma_view,
            chroma,
            chroma_view,
            buffer,
            pitch,
        }
    }

    /// Copies both planes into `surface` (an NV12 texture of the same size).
    fn copy_into(&self, encoder: &mut wgpu::CommandEncoder, surface: &wgpu::Texture) {
        let planes = [
            (&self.luma, wgpu::TextureAspect::Plane0, 0u64),
            (
                &self.chroma,
                wgpu::TextureAspect::Plane1,
                u64::from(self.pitch) * u64::from(self.luma.height()),
            ),
        ];
        for (texture, aspect, offset) in planes {
            let size = texture.size();
            let layout = wgpu::TexelCopyBufferLayout {
                offset,
                bytes_per_row: Some(self.pitch),
                rows_per_image: Some(size.height),
            };
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.buffer,
                    layout,
                },
                size,
            );
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.buffer,
                    layout,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: surface,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect,
                },
                size,
            );
        }
    }
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
            staged: (ctx.has(&CAP_NV12_SURFACE) && ctx.has(&CAP_SOFTWARE))
                .then(|| Staged::new(ctx, width, height)),
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
        let source = self.source(pool, canvas)?;
        let luma_tex = plane(pool, luma, &luma_desc(self.width, self.height))?;
        let chroma_tex = plane(pool, chroma, &chroma_desc(self.width, self.height))?;
        let encoder = self.record(ctx, source, luma_tex, chroma_tex);
        ctx.queue().submit(Some(encoder.finish()));
        Ok(())
    }

    /// Writes the NV12 planes of `canvas` into the single NV12 texture `surface`.
    ///
    /// Hot path. `surface` must match [`surface_desc`] for this size; without
    /// `gpu.nv12_surface` such a texture cannot be created and [`Self::convert`] is used.
    pub fn convert_to_surface(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        surface: TextureId,
    ) -> Result<(), GpuError> {
        let source = self.source(pool, canvas)?;
        let tex = pool.get(surface).ok_or(GpuError::StaleTexture(surface))?;
        let expected = surface_desc(self.width, self.height);
        let d = tex.desc();
        if d.format != expected.format
            || d.width != expected.width
            || d.height != expected.height
            || !d.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(GpuError::TargetMismatch(surface));
        }
        let (Some(luma), Some(chroma)) = (tex.plane_view(0), tex.plane_view(1)) else {
            return Err(GpuError::TargetMismatch(surface));
        };
        let encoder = match &self.staged {
            None => self.record(ctx, source, luma, chroma),
            Some(staged) => {
                let mut encoder = self.record(ctx, source, &staged.luma_view, &staged.chroma_view);
                staged.copy_into(&mut encoder, tex.texture());
                encoder
            }
        };
        ctx.queue().submit(Some(encoder.finish()));
        Ok(())
    }

    fn source<'p>(
        &self,
        pool: &'p TexturePool,
        canvas: TextureId,
    ) -> Result<&'p wgpu::BindGroup, GpuError> {
        let canvas_tex = pool.get(canvas).ok_or(GpuError::StaleTexture(canvas))?;
        let d = canvas_tex.desc();
        canvas_tex
            .sampled_bind_group()
            .filter(|_| {
                d.format == crate::CANVAS_FORMAT && d.width == self.width && d.height == self.height
            })
            .ok_or(GpuError::TargetMismatch(canvas))
    }

    /// Records the luma and chroma passes into the two views.
    fn record(
        &self,
        ctx: &GpuContext,
        source: &wgpu::BindGroup,
        luma: &wgpu::TextureView,
        chroma: &wgpu::TextureView,
    ) -> wgpu::CommandEncoder {
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu nv12"),
            });
        for (pipeline, view) in [(&self.luma, luma), (&self.chroma, chroma)] {
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
        encoder
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
