//! NV12 or P010 sources to linear `Rgba16Float` (spec `gpu-compositor.md`,
//! "YUV source decode").
//!
//! Cameras, hardware decoders and some capture paths deliver 4:2:0 frames. The compositor
//! samples RGB, so [`YuvDecoder::decode`] renders the two planes into a canvas-format
//! texture with the frame's matrix, range and transfer applied. The importer uses it for
//! native NV12 / P010 surfaces; it works on any pool planes.
//!
//! Hot path: one render pass with one full-screen triangle, submitted once per frame. Bind
//! groups and the per-source uniform are cached by plane ids, so steady-state decoding does
//! not allocate in RBS code; the uniform is rewritten only when the colour space changes.

use rbs_core::media::{ColorRange, ColorSpace, MatrixCoefficients, TextureId, TransferFunction};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};

/// Most sources (plane pairs) whose bind groups are cached. Older entries are replaced.
pub const MAX_DECODE_SOURCES: usize = 64;

/// Where the two planes of a 4:2:0 source are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvSource {
    /// Two pool textures: `R8Unorm` + `Rg8Unorm` (NV12) or `R16Unorm` + `Rg16Unorm` (P010),
    /// the chroma plane half the luma size.
    Planes {
        /// The Y' plane.
        luma: TextureId,
        /// The interleaved CbCr plane.
        chroma: TextureId,
    },
    /// One multi-planar `NV12` or `P010` pool texture (plane views 0 and 1).
    Surface(TextureId),
}

/// The output description for a decoded `width x height` source: the canvas format, so
/// the compositor samples it directly.
pub fn decoded_desc(width: u32, height: u32) -> TextureDesc {
    TextureDesc::canvas(width, height)
}

/// Uniform block of the decode shader (`Params` in `yuv_decode.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    rows: [[f32; 4]; 3],
    transfer: [u32; 4],
}

struct Cached {
    source: YuvSource,
    /// Bits of the source format (8 or 10), part of the parameters.
    bits: u32,
    color: ColorSpace,
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

/// Decodes 4:2:0 sources of any size. Holds GPU objects only while it exists; the importer
/// creates one on the first YUV surface (P13).
pub struct YuvDecoder {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    cache: Vec<Cached>,
    tick: u64,
}

impl YuvDecoder {
    /// Creates the pipeline. Not a per-frame call.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = ctx.device();
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu yuv decode"),
            entries: &[
                texture(0),
                texture(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<Params>() as u64
                        ),
                    },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu yuv decode"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/yuv_decode.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu yuv decode"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rbs-gpu yuv decode"),
            layout: Some(&pipeline_layout),
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
                entry_point: Some("fs_decode"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: crate::CANVAS_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            layout,
            pipeline,
            cache: Vec::with_capacity(MAX_DECODE_SOURCES),
            tick: 0,
        }
    }

    /// Decodes `source` with colour space `color` into `target`, a pool texture of
    /// [`decoded_desc`] at the luma size, and returns the submission.
    ///
    /// `color` is the source's description: matrix BT.601, BT.709 or BT.2020 (NCL), full or
    /// limited range, transfer BT.709, sRGB or linear. PQ and HLG are
    /// `GpuError::Unsupported` (HDR sources are a follow-up). The output is linear light
    /// with the source's primaries; draw it with `Transfer::Linear`. Width and height must
    /// be even. Hot path.
    pub fn decode(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        source: YuvSource,
        color: ColorSpace,
        target: TextureId,
    ) -> Result<wgpu::SubmissionIndex, GpuError> {
        let (luma, chroma, bits) = planes(pool, source)?;
        let (w, h) = (luma.texture().width(), luma.texture().height());
        let out = pool.get(target).ok_or(GpuError::StaleTexture(target))?;
        let d = out.desc();
        if d.format != crate::CANVAS_FORMAT
            || d.width != w
            || d.height != h
            || !d.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(GpuError::TargetMismatch(target));
        }
        let params = params(color, bits)?;
        self.tick += 1;
        let index = self.cached(ctx, source, luma, chroma, bits, color, params);
        let bind_group = &self.cache[index].bind_group;

        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu yuv decode"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu yuv decode"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: out.view(),
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(ctx.queue().submit(Some(encoder.finish())))
    }

    /// Sources whose bind groups are cached.
    pub fn cached_sources(&self) -> usize {
        self.cache.len()
    }

    /// The cache entry for `source`, created or refreshed when needed. Ids carry a
    /// generation, so a reused pool slot is a different key.
    #[allow(clippy::too_many_arguments)]
    fn cached(
        &mut self,
        ctx: &GpuContext,
        source: YuvSource,
        luma: &crate::pool::PooledTexture,
        chroma: &wgpu::TextureView,
        bits: u32,
        color: ColorSpace,
        params: Params,
    ) -> usize {
        if let Some(i) = self.cache.iter().position(|c| c.source == source) {
            let c = &mut self.cache[i];
            c.last_used = self.tick;
            if c.color != color || c.bits != bits {
                // Rare: the source changed colour description.
                ctx.queue()
                    .write_buffer(&c.buffer, 0, bytemuck::bytes_of(&params));
                c.color = color;
                c.bits = bits;
            }
            return i;
        }
        // First sight of this source: allocates (buffer, bind group).
        let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu yuv decode params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue()
            .write_buffer(&buffer, 0, bytemuck::bytes_of(&params));
        let bind_group = ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu yuv decode"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(luma.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(chroma),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: buffer.as_entire_binding(),
                },
            ],
        });
        let entry = Cached {
            source,
            bits,
            color,
            buffer,
            bind_group,
            last_used: self.tick,
        };
        if self.cache.len() < MAX_DECODE_SOURCES {
            self.cache.push(entry);
            self.cache.len() - 1
        } else {
            let oldest = self
                .cache
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_used)
                .map_or(0, |(i, _)| i);
            self.cache[oldest] = entry;
            oldest
        }
    }
}

/// The luma texture, the chroma view and the bit depth of `source`, checked.
fn planes(
    pool: &TexturePool,
    source: YuvSource,
) -> Result<(&crate::pool::PooledTexture, &wgpu::TextureView, u32), GpuError> {
    let sampled = |d: &TextureDesc| d.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING);
    match source {
        YuvSource::Planes { luma, chroma } => {
            let l = pool.get(luma).ok_or(GpuError::StaleTexture(luma))?;
            let c = pool.get(chroma).ok_or(GpuError::StaleTexture(chroma))?;
            let (ld, cd) = (l.desc(), c.desc());
            let bits = match (ld.format, cd.format) {
                (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm) => 8,
                (wgpu::TextureFormat::R16Unorm, wgpu::TextureFormat::Rg16Unorm) => 10,
                _ => return Err(GpuError::TargetMismatch(chroma)),
            };
            if !sampled(ld) || !sampled(cd) {
                return Err(GpuError::TargetMismatch(luma));
            }
            check_size(ld.width, ld.height)?;
            if cd.width != ld.width / 2 || cd.height != ld.height / 2 {
                return Err(GpuError::TargetMismatch(chroma));
            }
            Ok((l, c.view(), bits))
        }
        YuvSource::Surface(id) => {
            let t = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
            let d = t.desc();
            let bits = match d.format {
                wgpu::TextureFormat::NV12 => 8,
                wgpu::TextureFormat::P010 => 10,
                _ => return Err(GpuError::TargetMismatch(id)),
            };
            if !sampled(d) {
                return Err(GpuError::TargetMismatch(id));
            }
            check_size(d.width, d.height)?;
            let chroma = t.plane_view(1).ok_or(GpuError::TargetMismatch(id))?;
            Ok((t, chroma, bits))
        }
    }
}

fn check_size(width: u32, height: u32) -> Result<(), GpuError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(GpuError::InvalidSize { width, height });
    }
    Ok(())
}

/// The shader parameters for `color` at `bits` per sample (8, or 10 in the high bits of a
/// 16-bit word).
fn params(color: ColorSpace, bits: u32) -> Result<Params, GpuError> {
    let (kr, kb) = match color.matrix {
        MatrixCoefficients::Bt601 => (0.299, 0.114),
        MatrixCoefficients::Bt709 => (0.2126, 0.0722),
        MatrixCoefficients::Bt2020Ncl => (0.2627, 0.0593),
        _ => {
            return Err(GpuError::Unsupported(
                "YUV matrix (BT.601, BT.709, BT.2020)",
            ));
        }
    };
    let transfer = match color.transfer {
        TransferFunction::Linear => 0,
        TransferFunction::Bt709 => 1,
        TransferFunction::Srgb => 2,
        _ => {
            return Err(GpuError::Unsupported(
                "YUV source transfer (HDR is a follow-up)",
            ));
        }
    };
    // Normalized value stored per code: 1/255 for 8 bits, 64/65535 for P010.
    let code_scale: f64 = if bits == 8 {
        1.0 / 255.0
    } else {
        64.0 / 65535.0
    };
    let k = f64::from(1u32 << (bits - 8));
    let max = f64::from((1u32 << bits) - 1);
    let (y_black, y_range, c_mid, c_range) = match color.range {
        ColorRange::Limited => (16.0 * k, 219.0 * k, 128.0 * k, 224.0 * k),
        _ => (0.0, max, 128.0 * k, max),
    };
    // Y' = ay * v + by and C = ac * v + bc on the stored value v.
    let (ay, by) = (1.0 / (code_scale * y_range), -y_black / y_range);
    let (ac, bc) = (1.0 / (code_scale * c_range), -c_mid / c_range);
    let kg = 1.0 - kr - kb;
    // R'G'B' from Y'CbCr.
    let m = [
        [1.0, 0.0, 2.0 * (1.0 - kr)],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0],
    ];
    let row = |r: [f64; 3]| {
        [
            (r[0] * ay) as f32,
            (r[1] * ac) as f32,
            (r[2] * ac) as f32,
            (r[0] * by + (r[1] + r[2]) * bc) as f32,
        ]
    };
    Ok(Params {
        rows: [row(m[0]), row(m[1]), row(m[2])],
        transfer: [transfer, 0, 0, 0],
    })
}

/// The colour space of a decoded frame: linear light with the source's primaries.
pub fn decoded_color(source: ColorSpace) -> ColorSpace {
    ColorSpace::new(
        source.primaries,
        TransferFunction::Linear,
        MatrixCoefficients::Rgb,
        ColorRange::Full,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    fn apply(p: &Params, v: [f32; 3]) -> [f32; 3] {
        let x = [v[0], v[1], v[2], 1.0];
        p.rows.map(|r| r.iter().zip(x).map(|(a, b)| a * b).sum())
    }

    #[test]
    fn limited_709_codes_map_to_black_white_and_red() {
        let p = params(ColorSpace::BT709_LIMITED, 8).unwrap();
        let n = |c: f32| c / 255.0;
        let black = apply(&p, [n(16.0), n(128.0), n(128.0)]);
        let white = apply(&p, [n(235.0), n(128.0), n(128.0)]);
        // BT.709 red: Y' 63, Cb 102, Cr 240.
        let red = apply(&p, [n(63.0), n(102.0), n(240.0)]);
        for (got, want) in [(black, [0.0; 3]), (white, [1.0; 3]), (red, [1.0, 0.0, 0.0])] {
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 0.01, "{got:?} vs {want:?}");
            }
        }
    }

    #[test]
    fn p010_white_is_white() {
        let p = params(ColorSpace::BT709_LIMITED, 10).unwrap();
        let n = |c: f32| c * 64.0 / 65535.0;
        let white = apply(&p, [n(940.0), n(512.0), n(512.0)]);
        for g in white {
            assert!((g - 1.0).abs() < 0.01, "{white:?}");
        }
    }

    #[test]
    fn rgb_matrix_and_hdr_transfers_are_rejected() {
        assert!(params(ColorSpace::SRGB, 8).is_err());
        assert!(params(ColorSpace::BT2100_PQ, 10).is_err());
    }
}
