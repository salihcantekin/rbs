//! Canvas to NV12 or P010 conversion for encoders (BT.709, limited range).
//!
//! Two outputs: one multi-planar texture ([`surface_desc`],
//! [`YuvConverter::convert_to_surface`]) where the device supports the format
//! (capability `gpu.nv12_surface` or `gpu.p010_surface`), which hardware encoders consume
//! directly, or two plane textures ([`luma_desc`], [`chroma_desc`], [`YuvConverter::convert`])
//! everywhere else.
//!
//! Hot path: both conversions record two render passes (luma at full size,
//! chroma at half size) and submit them. It does not allocate in RBS code. Render
//! passes are used instead of a compute pass because the plane formats are render targets,
//! while storage writes to them need optional features. NV12 and P010 share one shader;
//! the code levels are pipeline-overridable constants.

use crate::context::{CAP_NV12_SURFACE, CAP_P010, CAP_P010_SURFACE, CAP_SOFTWARE, GpuContext};
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use rbs_core::media::{PixelFormat, TextureId};

/// The encoder format a [`YuvConverter`] writes: 4:2:0 with interleaved chroma.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvFormat {
    /// 8-bit: `R8Unorm` luma, `Rg8Unorm` chroma.
    Nv12,
    /// 10-bit in the high bits of 16-bit words: `R16Unorm` luma, `Rg16Unorm` chroma.
    /// Needs `gpu.p010`.
    P010,
}

impl YuvFormat {
    /// The `rbs-core` pixel format of the converted frame.
    pub fn pixel_format(self) -> PixelFormat {
        match self {
            Self::Nv12 => PixelFormat::Nv12,
            Self::P010 => PixelFormat::P010,
        }
    }

    fn luma_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Nv12 => wgpu::TextureFormat::R8Unorm,
            Self::P010 => wgpu::TextureFormat::R16Unorm,
        }
    }

    fn chroma_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Nv12 => wgpu::TextureFormat::Rg8Unorm,
            Self::P010 => wgpu::TextureFormat::Rg16Unorm,
        }
    }

    fn surface_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Nv12 => wgpu::TextureFormat::NV12,
            Self::P010 => wgpu::TextureFormat::P010,
        }
    }

    /// Bytes per luma sample (and per chroma component).
    fn sample_bytes(self) -> u32 {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 2,
        }
    }

    /// Shader constants: limited-range code levels and the code-to-unorm scale.
    fn constants(self) -> [(&'static str, f64); 5] {
        let (bits, scale) = match self {
            Self::Nv12 => (8, 1.0 / 255.0),
            // A 10-bit code shifted into the high bits of a 16-bit unorm word.
            Self::P010 => (10, 64.0 / 65535.0),
        };
        let k = f64::from(1u32 << (bits - 8));
        [
            ("y_black", 16.0 * k),
            ("y_range", 219.0 * k),
            ("c_mid", 128.0 * k),
            ("c_range", 224.0 * k),
            ("code_scale", scale),
        ]
    }
}

/// Description of the luma plane: `width x height`.
pub fn luma_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format: format.luma_format(),
        usage: plane_usage(),
    }
}

/// Description of the interleaved chroma plane (Cb, Cr): `width/2 x height/2`.
pub fn chroma_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width: width / 2,
        height: height / 2,
        format: format.chroma_format(),
        usage: plane_usage(),
    }
}

/// Description of a single multi-planar texture with both planes, for encoders.
///
/// Needs capability `gpu.nv12_surface` (NV12) or `gpu.p010_surface` (P010).
pub fn surface_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format: format.surface_format(),
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

/// Converts canvases of one size to NV12 or P010.
pub struct YuvConverter {
    format: YuvFormat,
    width: u32,
    height: u32,
    luma: wgpu::RenderPipeline,
    chroma: wgpu::RenderPipeline,
    /// Software adapters only: planes and a buffer for the staged surface path.
    staged: Option<Staged>,
}

/// Intermediate planes for adapters where rendering into plane views is not
/// reliable (software adapters: lavapipe writes plane 1 over plane 0). The planes are
/// rendered as usual and copied into the multi-planar texture through one GPU buffer; nothing
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
    fn new(ctx: &GpuContext, format: YuvFormat, width: u32, height: u32) -> Self {
        let plane = |d: TextureDesc| {
            let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("rbs-gpu yuv staged plane"),
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
        let (luma, luma_view) = plane(luma_desc(format, width, height));
        let (chroma, chroma_view) = plane(chroma_desc(format, width, height));
        // A luma row is `width` samples; a chroma row is `width / 2` texels of two samples.
        let row = width * format.sample_bytes();
        let pitch =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu yuv staged"),
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

    /// Copies both planes into `surface` (a multi-planar texture of the same size).
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

impl YuvConverter {
    /// Creates a converter to `format` for `width x height` canvases. Both must be even and
    /// non-zero. P010 needs capability `gpu.p010`.
    pub fn new(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if format == YuvFormat::P010 && !ctx.has(&CAP_P010) {
            return Err(GpuError::Unsupported(
                "P010 needs 16-bit normalized render targets (gpu.p010)",
            ));
        }
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
            label: Some("rbs-gpu yuv"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/yuv.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu yuv"),
            bind_group_layouts: &[Some(&ctx.sampled_layout)],
            immediate_size: 0,
        });
        let constants = format.constants();
        let pipeline = |entry: &str, target: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("rbs-gpu yuv"),
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
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &constants,
                        ..Default::default()
                    },
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let surface_cap = match format {
            YuvFormat::Nv12 => &CAP_NV12_SURFACE,
            YuvFormat::P010 => &CAP_P010_SURFACE,
        };
        Ok(Self {
            format,
            width,
            height,
            luma: pipeline("fs_luma", format.luma_format()),
            chroma: pipeline("fs_chroma", format.chroma_format()),
            staged: (ctx.has(surface_cap) && ctx.has(&CAP_SOFTWARE))
                .then(|| Staged::new(ctx, format, width, height)),
        })
    }

    /// The format this converter writes.
    pub fn format(&self) -> YuvFormat {
        self.format
    }

    /// Writes the planes of `canvas` into `luma` and `chroma` and returns the submission,
    /// so a caller can tell when the planes are complete (for example before a native
    /// encoder reads them).
    ///
    /// Hot path. The planes must match [`luma_desc`] and [`chroma_desc`] for this size.
    pub fn convert(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        luma: TextureId,
        chroma: TextureId,
    ) -> Result<wgpu::SubmissionIndex, GpuError> {
        let luma_tex = plane(pool, luma, &luma_desc(self.format, self.width, self.height))?;
        let chroma_tex = plane(
            pool,
            chroma,
            &chroma_desc(self.format, self.width, self.height),
        )?;
        self.convert_views(ctx, pool, canvas, luma_tex, chroma_tex)
    }

    /// Writes both planes of `canvas` into the single multi-planar texture `surface` and
    /// returns the submission.
    ///
    /// Hot path. `surface` must match [`surface_desc`] for this format and size; without
    /// the surface capability such a texture cannot be created and [`Self::convert`] is used.
    pub fn convert_to_surface(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        surface: TextureId,
    ) -> Result<wgpu::SubmissionIndex, GpuError> {
        let source = self.source(pool, canvas)?;
        let tex = pool.get(surface).ok_or(GpuError::StaleTexture(surface))?;
        let expected = surface_desc(self.format, self.width, self.height);
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
        Ok(ctx.queue().submit(Some(encoder.finish())))
    }

    /// Writes the planes of `canvas` into two views the caller created for this format
    /// and size (luma and chroma render targets, for example aliased native surfaces).
    /// The views are not checked. Hot path.
    pub(crate) fn convert_views(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        luma: &wgpu::TextureView,
        chroma: &wgpu::TextureView,
    ) -> Result<wgpu::SubmissionIndex, GpuError> {
        let source = self.source(pool, canvas)?;
        let encoder = self.record(ctx, source, luma, chroma);
        Ok(ctx.queue().submit(Some(encoder.finish())))
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
                label: Some("rbs-gpu yuv"),
            });
        for (pipeline, view) in [(&self.luma, luma), (&self.chroma, chroma)] {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu yuv"),
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
