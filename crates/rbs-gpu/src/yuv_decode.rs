//! NV12 or P010 sources to linear RGB (spec `gpu-compositor.md`, "YUV source decode").
//!
//! Cameras, hardware decoders and some capture paths deliver 4:2:0 frames. The compositor
//! samples RGB, so [`YuvDecoder::decode`] renders the two planes into an RGB texture with
//! the frame's matrix, range and transfer applied. The importer uses it for native NV12 /
//! P010 surfaces; it works on any pool planes.
//!
//! 8-bit NV12 decodes into [`DECODED_8BIT_FORMAT`] (`Rgba8UnormSrgb`): the GPU encodes the
//! linear result to sRGB on store and decodes it back to linear on every sample, so the
//! compositor still blends in linear light, at half the bytes of `Rgba16Float` (perf audit
//! R-11 step a). 10-bit P010 decodes into the canvas format.
//!
//! Hot path: one render pass with one full-screen triangle, submitted once per frame. Bind
//! groups and the per-source uniform are cached by plane ids, so steady-state decoding does
//! not allocate in RBS code; the uniform is rewritten only when the colour space changes.
//! Callers keep a source's plane slots stable across frames (as the importer does); new
//! plane ids every frame would create a bind group every frame.

use rbs_core::media::{
    ColorRange, ColorSpace, FramePayload, MatrixCoefficients, PixelFormat, TextureId,
    TransferFunction, VideoFormat, VideoFrame,
};

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

/// The texture format 8-bit (NV12) sources decode into: sRGB-encoded 8-bit, linear on
/// sampling.
pub const DECODED_8BIT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// The output description for a decoded `width x height` source of `pixel_format`
/// (`Nv12` or `P010`): [`DECODED_8BIT_FORMAT`] for NV12, the canvas format otherwise. The
/// compositor samples either directly.
pub fn decoded_desc(width: u32, height: u32, pixel_format: PixelFormat) -> TextureDesc {
    let canvas = TextureDesc::canvas(width, height);
    match pixel_format {
        PixelFormat::Nv12 => TextureDesc {
            format: DECODED_8BIT_FORMAT,
            ..canvas
        },
        _ => canvas,
    }
}

/// The pixel format of a decoded frame of a `pixel_format` source: `Rgba8Srgb` for NV12,
/// `Rgba16Float` otherwise. Its colour space is [`decoded_color`] (linear), which is what
/// a sample of the texture returns.
pub fn decoded_pixel_format(pixel_format: PixelFormat) -> PixelFormat {
    match pixel_format {
        PixelFormat::Nv12 => PixelFormat::Rgba8Srgb,
        _ => PixelFormat::Rgba16Float,
    }
}

/// The target texture format for a source of `bits` per sample.
fn target_format(bits: u32) -> wgpu::TextureFormat {
    if bits == 8 {
        DECODED_8BIT_FORMAT
    } else {
        crate::CANVAS_FORMAT
    }
}

/// Uniform block of the decode shader (`Params` in `yuv_decode.wgsl`), and the
/// compositor's per-layer 4:2:0 record (`YuvGpu` in `composite.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Params {
    rows: [[f32; 4]; 3],
    transfer: [u32; 4],
}

/// A plane pair given as two pool slots, with its bind group on the 4:2:0 layout.
struct CachedPlanes {
    luma: TextureId,
    chroma: TextureId,
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

/// The parameter uniform of one colour space at one bit depth.
struct CachedParams {
    bits: u32,
    color: ColorSpace,
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

/// Most colour descriptions whose parameter uniforms are cached. Older entries are
/// replaced.
const MAX_DECODE_COLORS: usize = 8;

/// Decodes 4:2:0 sources of any size. Holds GPU objects only while it exists; consumers
/// that need RGB (filters, [`YuvToRgb`]) create one on the first 4:2:0 frame (P13).
///
/// The planes are bound with the context's 4:2:0 layout (group 0), the colour parameters
/// with a uniform cached per colour space (group 1). A 4:2:0 pool slot
/// ([`YuvSource::Surface`]) brings its own plane bind group, so decoding it creates
/// nothing per frame, whatever its id.
pub struct YuvDecoder {
    params_layout: wgpu::BindGroupLayout,
    /// Writes [`DECODED_8BIT_FORMAT`] (NV12 sources).
    pipeline_8bit: wgpu::RenderPipeline,
    /// Writes the canvas format (P010 sources).
    pipeline_canvas: wgpu::RenderPipeline,
    planes: Vec<CachedPlanes>,
    colors: Vec<CachedParams>,
    tick: u64,
}

impl YuvDecoder {
    /// Creates the pipeline. Not a per-frame call.
    pub fn new(ctx: &GpuContext) -> Self {
        let device = ctx.device();
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu yuv decode params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<Params>() as u64),
                },
                count: None,
            }],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu yuv decode"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/yuv_decode.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu yuv decode"),
            bind_group_layouts: &[Some(&ctx.yuv_layout), Some(&params_layout)],
            immediate_size: 0,
        });
        let pipeline = |format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            pipeline_8bit: pipeline(DECODED_8BIT_FORMAT),
            pipeline_canvas: pipeline(crate::CANVAS_FORMAT),
            params_layout,
            planes: Vec::with_capacity(MAX_DECODE_SOURCES),
            colors: Vec::with_capacity(MAX_DECODE_COLORS),
            tick: 0,
        }
    }

    /// Decodes `source` with colour space `color` into `target`, a pool texture of
    /// [`decoded_desc`] for the source's format at the luma size, and returns the
    /// submission.
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
        if d.format != target_format(bits)
            || d.width != w
            || d.height != h
            || !d.usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(GpuError::TargetMismatch(target));
        }
        let params = params(color, bits)?;
        self.tick += 1;
        let colour = self.cached_params(ctx, bits, color, params);
        let pair = match source {
            YuvSource::Surface(_) => None,
            YuvSource::Planes { .. } => {
                Some(self.cached_planes(ctx, pool, source, luma.view(), chroma))
            }
        };
        let planes_group = match pair {
            Some(i) => &self.planes[i].bind_group,
            None => luma
                .yuv_bind_group()
                .ok_or(GpuError::TargetMismatch(target))?,
        };
        let params_group = &self.colors[colour].bind_group;

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
            pass.set_pipeline(if bits == 8 {
                &self.pipeline_8bit
            } else {
                &self.pipeline_canvas
            });
            pass.set_bind_group(0, planes_group, &[]);
            pass.set_bind_group(1, params_group, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(ctx.queue().submit(Some(encoder.finish())))
    }

    /// Drops the cached bind group of a plane pair given as two slots, one of which is
    /// `id`. Call when a source's planes are released, so the bind group does not keep
    /// their textures alive. Not a per-frame call. 4:2:0 pool slots
    /// ([`YuvSource::Surface`]) need no call: their bind group lives in the slot.
    pub fn forget(&mut self, id: TextureId) {
        self.planes.retain(|c| c.luma != id && c.chroma != id);
    }

    /// Plane pairs given as two slots whose bind groups are cached.
    pub fn cached_sources(&self) -> usize {
        self.planes.len()
    }

    /// The cache entry of a two-slot plane pair, created when needed. Ids carry a
    /// generation, so a reused pool slot is a different key.
    fn cached_planes(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        source: YuvSource,
        luma_view: &wgpu::TextureView,
        chroma_view: &wgpu::TextureView,
    ) -> usize {
        let YuvSource::Planes { luma, chroma } = source else {
            return 0;
        };
        if let Some(i) = self
            .planes
            .iter()
            .position(|c| c.luma == luma && c.chroma == chroma)
        {
            self.planes[i].last_used = self.tick;
            return i;
        }
        // First sight: allocates. Entries whose planes left the pool go first.
        self.planes
            .retain(|c| pool.get(c.luma).is_some() && pool.get(c.chroma).is_some());
        let bind_group = ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu yuv decode planes"),
            layout: &ctx.yuv_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(chroma_view),
                },
            ],
        });
        let entry = CachedPlanes {
            luma,
            chroma,
            bind_group,
            last_used: self.tick,
        };
        replace_oldest(&mut self.planes, entry, MAX_DECODE_SOURCES, |c| c.last_used)
    }

    /// The parameter uniform of `color` at `bits`, created when first seen.
    fn cached_params(
        &mut self,
        ctx: &GpuContext,
        bits: u32,
        color: ColorSpace,
        params: Params,
    ) -> usize {
        if let Some(i) = self
            .colors
            .iter()
            .position(|c| c.bits == bits && c.color == color)
        {
            self.colors[i].last_used = self.tick;
            return i;
        }
        // A new colour description: rare, allocates.
        let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu yuv decode params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue()
            .write_buffer(&buffer, 0, bytemuck::bytes_of(&params));
        let bind_group = ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu yuv decode params"),
            layout: &self.params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });
        let entry = CachedParams {
            bits,
            color,
            bind_group,
            last_used: self.tick,
        };
        replace_oldest(&mut self.colors, entry, MAX_DECODE_COLORS, |c| c.last_used)
    }
}

/// Pushes `entry`, or replaces the least recently used one when `cache` holds `max`.
/// Returns its index.
fn replace_oldest<T>(cache: &mut Vec<T>, entry: T, max: usize, used: impl Fn(&T) -> u64) -> usize {
    if cache.len() < max {
        cache.push(entry);
        return cache.len() - 1;
    }
    let oldest = cache
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| used(c))
        .map_or(0, |(i, _)| i);
    cache[oldest] = entry;
    oldest
}

/// RGB for consumers that cannot sample 4:2:0 (filters, the separate-file normalisation):
/// decodes a 4:2:0 texture frame once per picture into a pool texture of
/// [`decoded_desc`] and keeps that frame until the next picture (perf audit R-11 step
/// b). Holds nothing until the first 4:2:0 frame (P13).
#[derive(Default)]
pub struct YuvToRgb {
    decoder: Option<YuvDecoder>,
    /// The last picture decoded: its slot, time and sequence, and the RGB frame.
    last: Option<((TextureId, rbs_core::time::Timestamp, u64), VideoFrame)>,
}

impl YuvToRgb {
    /// Nothing decoded yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when `frame` is a texture frame of a 4:2:0 slot, which [`Self::rgb`] decodes.
    pub fn needs_decode(pool: &TexturePool, frame: &VideoFrame) -> bool {
        let FramePayload::Texture(id) = *frame.payload() else {
            return false;
        };
        pool.get(id).is_some_and(|t| t.is_planar())
    }

    /// The RGB frame of `frame`'s picture (a 4:2:0 texture frame): decoded on the first
    /// call for a picture, the kept frame on later calls. The frame has the decode's
    /// pixel format and [`decoded_color`], the time and sequence of `frame`. Hot path:
    /// a picture already seen costs a comparison; a new one one decode pass, with no
    /// allocation in RBS code once the pool holds textures of its size.
    pub fn rgb(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        frame: &VideoFrame,
    ) -> Result<&VideoFrame, GpuError> {
        let FramePayload::Texture(id) = *frame.payload() else {
            return Err(GpuError::Unsupported(
                "RGB of a frame that is not a texture",
            ));
        };
        let key = (id, frame.timestamp(), frame.sequence());
        // The kept frame goes unless it is this picture's, so its slot can be reused.
        if self.last.as_ref().is_some_and(|(k, _)| *k != key) {
            self.last = None;
        }
        if self.last.is_none() {
            self.last = Some((key, self.decode(ctx, pool, id, frame)?));
        }
        self.last
            .as_ref()
            .map(|(_, rgb)| rgb)
            .ok_or(GpuError::StaleTexture(id))
    }

    /// Decodes the 4:2:0 picture of `frame` (slot `id`) into a new RGB frame.
    fn decode(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        id: TextureId,
        frame: &VideoFrame,
    ) -> Result<VideoFrame, GpuError> {
        let format = *frame.format();
        let (w, h) = (format.size.width, format.size.height);
        let decoder = self.decoder.get_or_insert_with(|| YuvDecoder::new(ctx));
        let target = pool.acquire(ctx, &decoded_desc(w, h, format.pixel_format))?;
        let decoded = decoder
            .decode(ctx, pool, YuvSource::Surface(id), format.color, target)
            .and_then(|_| {
                let rgb = VideoFormat::new(
                    format.size,
                    decoded_pixel_format(format.pixel_format),
                    decoded_color(format.color),
                );
                pool.frame(target, rgb, frame.timestamp())
            });
        // The kept frame's lease holds the slot; this handle is not needed.
        let _ = pool.release(target);
        let mut rgb = decoded?.with_sequence(frame.sequence());
        if let Some(t) = frame.source_time() {
            rgb = rgb.with_source_time(t);
        }
        Ok(rgb)
    }

    /// Drops the kept frame (its slot returns to the pool once no reader holds it).
    /// Not a per-frame call.
    pub fn release(&mut self) {
        self.last = None;
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
pub(crate) fn params(color: ColorSpace, bits: u32) -> Result<Params, GpuError> {
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
        ColorRange::Full => (0.0, max, 128.0 * k, max),
        _ => return Err(GpuError::Unsupported("YUV range (limited or full)")),
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

/// Checks that `color` can be decoded at `bits` per sample (8 or 10), without GPU work.
pub fn check_color(color: ColorSpace, bits: u32) -> Result<(), GpuError> {
    params(color, bits).map(|_| ())
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
