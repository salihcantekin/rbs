//! System-memory source frames to pool textures (spec `docs/specs/gpu-compositor.md`,
//! "System-memory upload"; ADR-0038).
//!
//! Cameras and other sources that deliver CPU buffers (Media Foundation cameras, the
//! synthetic test adapter) need their pixels on the GPU before the compositor can draw
//! them. [`SourceUpload`] writes a `System` frame into pool textures with
//! `Queue::write_texture` and returns a `Texture` frame of the same time:
//!
//! - `Bgra8` and `Rgba8`: one pool texture per frame, sampled directly by the compositor.
//! - `Nv12` and `P010`: one 4:2:0 pool slot per frame ([`TextureDesc::planar`], a plane
//!   pair), which the compositor samples as it is, like imported 4:2:0 surfaces (perf
//!   audit R-11 step b). The frame keeps the source's pixel format and colour.
//!
//! Hot path: [`SourceUpload::upload`] allocates nothing in RBS code for a format and size
//! it has seen (see [`SourceUpload::prepare_for`]); wgpu stages the bytes internally.
//! The pool textures of a new size are created on first use.

use rbs_core::media::{FramePayload, PixelFormat, SystemFrame, VideoFormat, VideoFrame};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use crate::yuv_decode::check_color;
use rbs_core::media::TextureId;

/// Uploads system-memory frames into pool textures. Holds nothing until the first frame
/// or [`SourceUpload::prepare_for`].
#[derive(Default)]
pub struct SourceUpload {
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

/// The 4:2:0 slot description of a `width x height` `pf` (`Nv12` or `P010`) frame:
/// sampled by the compositor, filled by copies, copyable for diagnostics and tests.
fn planar_desc(width: u32, height: u32, pf: PixelFormat) -> TextureDesc {
    let format = if pf == PixelFormat::Nv12 {
        wgpu::TextureFormat::NV12
    } else {
        wgpu::TextureFormat::P010
    };
    let mut desc = TextureDesc::planar(width, height, format);
    desc.usage |= wgpu::TextureUsages::COPY_SRC;
    desc
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
    // Plane 1 of a 4:2:0 slot is its CbCr texture.
    let target = u32::try_from(index)
        .ok()
        .and_then(|i| tex.plane_texture(i))
        .ok_or(GpuError::TargetMismatch(texture))?;
    // `SystemFrame::new` checked that every row fits; trim to what the copy reads.
    let needed = u64::from(stride) * u64::from(rows.saturating_sub(1)) + row;
    let data = usize::try_from(needed)
        .ok()
        .and_then(|n| data.get(..n))
        .ok_or(GpuError::Unsupported("system frame plane too short"))?;
    ctx.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: target,
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
        target.size(),
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

    /// Creates what frames of `format` need, so the first [`Self::upload`] does not: a
    /// ring of three textures (4:2:0 plane pairs, or single-plane textures) on the pool's
    /// free list. Not real-time (call from `warm`).
    pub fn prepare_for(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        format: &VideoFormat,
    ) -> Result<(), GpuError> {
        let (w, h) = (format.size.width, format.size.height);
        match format.pixel_format {
            PixelFormat::Nv12 | PixelFormat::P010 => {
                ring(ctx, pool, &planar_desc(w, h, format.pixel_format))
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
                // One 4:2:0 slot per frame from the pool's free list; the frame keeps
                // the source's format and colour and the compositor samples it as it is.
                let id = pool.acquire(ctx, &planar_desc(w, h, format.pixel_format))?;
                let written = write_plane(ctx, pool, system, 0, id)
                    .and_then(|()| write_plane(ctx, pool, system, 1, id))
                    .and_then(|()| pool.frame(id, format, frame.timestamp()));
                let _ = pool.release(id);
                written?
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

    /// Kept for callers that stop a source: the upload holds no texture between frames
    /// (each frame's slot is held by the frame alone), so there is nothing to give back.
    pub fn release(&mut self, pool: &mut TexturePool) {
        let _ = pool;
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

/// Two textures of one size a source keeps acquired and writes in turn (ADR-0129
/// decision 5, spec `gpu-compositor.md`, "Reserved upload textures").
///
/// Unlike [`SourceUpload::prepare_for`], which leaves a ring on the pool's shared free
/// list, these stay `InUse`: `trim_idle` cannot drop them and no other source can take
/// them. [`ReservedUpload::upload_into`] writes into the one no reader leases. Text
/// textures are read only within the tick that publishes them and are never queued, so
/// the one the node's own output slot shows is the only lease in the steady state.
#[derive(Debug)]
pub struct ReservedUpload {
    ids: [TextureId; 2],
    width: u32,
    height: u32,
    format: PixelFormat,
}

impl ReservedUpload {
    /// Acquires two `width` x `height` textures of a single-plane `format` (`Rgba8` or
    /// `Bgra8`). Not real-time except after a size change (ADR-0062 decision 5).
    pub fn reserve(
        ctx: &GpuContext,
        pool: &mut TexturePool,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Self, GpuError> {
        let tex_format =
            rgb_format(format).ok_or(GpuError::Unsupported("reserved upload pixel format"))?;
        let desc = rgb_desc(width, height, tex_format);
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
            format,
        })
    }

    /// Width and height of the textures.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Writes `system` (of this size and format) into a texture no reader leases and
    /// returns a frame of it at `time`. `Ok(None)` when both are leased: the caller keeps
    /// the picture for a later tick. Hot path: no allocation in RBS code (wgpu stages
    /// the bytes).
    pub fn upload_into(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        system: &SystemFrame,
        time: rbs_core::time::Timestamp,
    ) -> Result<Option<VideoFrame>, GpuError> {
        let format = *system.format();
        if format.size.width != self.width
            || format.size.height != self.height
            || format.pixel_format != self.format
        {
            return Err(GpuError::InvalidSize {
                width: format.size.width,
                height: format.size.height,
            });
        }
        let Some(&id) = self.ids.iter().find(|id| pool.lease_count(**id) == 0) else {
            return Ok(None);
        };
        write_plane(ctx, pool, system, 0, id)?;
        pool.frame(id, format, time).map(Some)
    }

    /// Gives both textures back to the pool and forgets them, so their memory goes as
    /// soon as no reader holds them (as the filter node does on a size change).
    pub fn give_back(self, pool: &mut TexturePool) {
        for id in self.ids {
            let _ = pool.release(id);
            pool.forget_texture(id);
        }
    }
}
