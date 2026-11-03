//! System-memory source frames to pool textures (spec `docs/specs/gpu-compositor.md`,
//! "System-memory upload"; ADR-0038).
//!
//! Cameras and other sources that deliver CPU buffers (Media Foundation cameras, the
//! synthetic test adapter) need their pixels on the GPU before the compositor can draw
//! them. [`SourceUpload`] writes a `System` frame into pool textures with
//! `Queue::write_texture` and returns a `Texture` frame of the same time:
//!
//! - `Bgra8` and `Rgba8`: one pool texture per frame, sampled directly by the compositor.
//! - `Nv12` and `P010`: two plane textures the upload keeps (one pair per size), decoded
//!   by a [`YuvDecoder`] into a canvas-format pool texture per frame, like imported
//!   4:2:0 surfaces (rule 6 of "Native import").
//!
//! Hot path: [`SourceUpload::upload`] allocates nothing in RBS code for a format and size
//! it has seen (see [`SourceUpload::prepare_for`]); wgpu stages the bytes internally.
//! The pool textures of a new size, and the decoder on the first 4:2:0 frame, are created
//! on first use.

use rbs_core::media::{FramePayload, PixelFormat, SystemFrame, VideoFormat, VideoFrame};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use crate::yuv_decode::{YuvDecoder, YuvSource, check_color, decoded_color, decoded_desc};
use rbs_core::media::TextureId;

/// Plane textures kept for 4:2:0 frames of one size.
#[derive(Debug, Clone, Copy)]
struct Planes {
    luma: TextureId,
    chroma: TextureId,
    width: u32,
    height: u32,
    format: PixelFormat,
}

/// Uploads system-memory frames into pool textures. Holds nothing until the first frame
/// or [`SourceUpload::prepare_for`].
#[derive(Default)]
pub struct SourceUpload {
    planes: Option<Planes>,
    decoder: Option<YuvDecoder>,
    uploaded: u64,
}

/// The pool texture format a single-plane system format is uploaded as, or `None`.
fn rgb_format(pf: PixelFormat) -> Option<wgpu::TextureFormat> {
    match pf {
        PixelFormat::Bgra8 => Some(wgpu::TextureFormat::Bgra8Unorm),
        PixelFormat::Rgba8 => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

/// Whether [`SourceUpload`] takes system frames of `pf`.
pub fn supports(pf: PixelFormat) -> bool {
    rgb_format(pf).is_some() || matches!(pf, PixelFormat::Nv12 | PixelFormat::P010)
}

/// The pixel formats [`SourceUpload`] takes, cheapest to upload first.
pub const UPLOAD_FORMATS: [PixelFormat; 4] = [
    PixelFormat::Nv12,
    PixelFormat::P010,
    PixelFormat::Bgra8,
    PixelFormat::Rgba8,
];

/// A single-plane source texture: sampled by the compositor, filled by a copy, and
/// copyable for diagnostics and tests.
fn rgb_desc(width: u32, height: u32, format: wgpu::TextureFormat) -> TextureDesc {
    let mut desc = TextureDesc::source(width, height, format);
    desc.usage |= wgpu::TextureUsages::COPY_SRC;
    desc
}

fn plane_desc(width: u32, height: u32, format: wgpu::TextureFormat) -> TextureDesc {
    TextureDesc {
        width,
        height,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    }
}

/// Writes rows `rows` of `bytes_per_row` from plane `index` of `frame` into `texture`.
fn write_plane(
    ctx: &GpuContext,
    pool: &TexturePool,
    frame: &SystemFrame,
    index: usize,
    texture: TextureId,
) -> Result<(), GpuError> {
    let tex = pool.get(texture).ok_or(GpuError::StaleTexture(texture))?;
    let (data, stride) = frame
        .plane(index)
        .ok_or(GpuError::Unsupported("system frame plane missing"))?;
    let format = frame.format();
    let pf = format.pixel_format;
    let row = pf
        .min_row_bytes(index, format.size.width)
        .ok_or(GpuError::Unsupported("system frame plane missing"))?;
    let rows = pf
        .plane_rows(index, format.size.height)
        .ok_or(GpuError::Unsupported("system frame plane missing"))?;
    let stride = u32::try_from(stride).map_err(|_| GpuError::Unsupported("stride too large"))?;
    // `SystemFrame::new` checked that every row fits; trim to what the copy reads.
    let needed = u64::from(stride) * u64::from(rows.saturating_sub(1)) + row;
    let data = usize::try_from(needed)
        .ok()
        .and_then(|n| data.get(..n))
        .ok_or(GpuError::Unsupported("system frame plane too short"))?;
    ctx.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(rows),
        },
        tex.texture().size(),
    );
    Ok(())
}

impl SourceUpload {
    /// An upload with nothing allocated.
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames uploaded so far.
    pub fn uploaded(&self) -> u64 {
        self.uploaded
    }

    /// Creates what frames of `format` need, so the first [`Self::upload`] does not: the
    /// plane textures and the decoder for 4:2:0, a ring of three textures on the pool's
    /// free list for single-plane formats. Not real-time (call from `warm`).
    pub fn prepare_for(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        format: &VideoFormat,
    ) -> Result<(), GpuError> {
        let (w, h) = (format.size.width, format.size.height);
        match format.pixel_format {
            PixelFormat::Nv12 | PixelFormat::P010 => {
                self.planes_for(ctx, pool, format)?;
                if self.decoder.is_none() {
                    self.decoder = Some(YuvDecoder::new(ctx));
                }
                ring(ctx, pool, &decoded_desc(w, h))
            }
            pf => match rgb_format(pf) {
                Some(f) => ring(ctx, pool, &rgb_desc(w, h, f)),
                None => Err(GpuError::Unsupported("system frame pixel format")),
            },
        }
    }

    /// Uploads a `System` frame and returns a `Texture` frame of the same time and
    /// sequence. Other frames are returned unchanged. Hot path.
    pub fn upload(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        frame: VideoFrame,
    ) -> Result<VideoFrame, GpuError> {
        let FramePayload::System(system) = frame.payload() else {
            return Ok(frame);
        };
        let format = *system.format();
        let (w, h) = (format.size.width, format.size.height);
        let uploaded = match format.pixel_format {
            PixelFormat::Nv12 | PixelFormat::P010 => {
                let bits = if format.pixel_format == PixelFormat::Nv12 {
                    8
                } else {
                    10
                };
                check_color(format.color, bits)?;
                let planes = self.planes_for(ctx, pool, &format)?;
                write_plane(ctx, pool, system, 0, planes.luma)?;
                write_plane(ctx, pool, system, 1, planes.chroma)?;
                let decoder = self.decoder.get_or_insert_with(|| YuvDecoder::new(ctx));
                let target = pool.acquire(ctx, &decoded_desc(w, h))?;
                let decoded = decoder
                    .decode(
                        ctx,
                        pool,
                        YuvSource::Planes {
                            luma: planes.luma,
                            chroma: planes.chroma,
                        },
                        format.color,
                        target,
                    )
                    .and_then(|_| {
                        let rgb = VideoFormat::new(
                            format.size,
                            PixelFormat::Rgba16Float,
                            decoded_color(format.color),
                        );
                        pool.frame(target, rgb, frame.timestamp())
                    });
                // The frame's lease keeps the slot; the upload keeps nothing.
                let _ = pool.release(target);
                decoded?
            }
            pf => {
                let tex_format =
                    rgb_format(pf).ok_or(GpuError::Unsupported("system frame pixel format"))?;
                let id = pool.acquire(ctx, &rgb_desc(w, h, tex_format))?;
                let written = write_plane(ctx, pool, system, 0, id)
                    .and_then(|()| pool.frame(id, format, frame.timestamp()));
                let _ = pool.release(id);
                written?
            }
        };
        self.uploaded += 1;
        let mut out = uploaded.with_sequence(frame.sequence());
        if let Some(t) = frame.source_time() {
            out = out.with_source_time(t);
        }
        Ok(out)
    }

    /// Returns the plane textures to the pool and forgets the decoder's bind groups for
    /// them. Call when the source stops.
    pub fn release(&mut self, pool: &mut TexturePool) {
        if let Some(p) = self.planes.take() {
            if let Some(decoder) = &mut self.decoder {
                decoder.forget(p.luma);
                decoder.forget(p.chroma);
            }
            let _ = pool.release(p.luma);
            let _ = pool.release(p.chroma);
        }
    }

    /// The plane pair for `format`, replacing a pair of another size or bit depth.
    fn planes_for(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        format: &VideoFormat,
    ) -> Result<Planes, GpuError> {
        let (w, h, pf) = (format.size.width, format.size.height, format.pixel_format);
        if let Some(p) = self.planes
            && p.width == w
            && p.height == h
            && p.format == pf
        {
            return Ok(p);
        }
        if w % 2 != 0 || h % 2 != 0 || w == 0 || h == 0 {
            return Err(GpuError::InvalidSize {
                width: w,
                height: h,
            });
        }
        self.release(pool);
        let (lf, cf) = if pf == PixelFormat::Nv12 {
            (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm)
        } else {
            (
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            )
        };
        let luma = pool.acquire(ctx, &plane_desc(w, h, lf))?;
        let chroma = match pool.acquire(ctx, &plane_desc(w / 2, h / 2, cf)) {
            Ok(id) => id,
            Err(e) => {
                let _ = pool.release(luma);
                return Err(e);
            }
        };
        let p = Planes {
            luma,
            chroma,
            width: w,
            height: h,
            format: pf,
        };
        self.planes = Some(p);
        Ok(p)
    }
}

/// Acquires and releases three textures of `desc`, so the pool's free list holds them.
fn ring(ctx: &GpuContext, pool: &mut TexturePool, desc: &TextureDesc) -> Result<(), GpuError> {
    let mut ids = [None; 3];
    let mut result = Ok(());
    for slot in &mut ids {
        match pool.acquire(ctx, desc) {
            Ok(id) => *slot = Some(id),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    for id in ids.into_iter().flatten() {
        let _ = pool.release(id);
    }
    result
}
