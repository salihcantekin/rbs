//! Native surface export for encoders (spec `docs/specs/gpu-compositor.md#native-export`,
//! ADR-0025).
//!
//! macOS: [`SurfaceExporter`] owns a fixed ring of bi-planar IOSurfaces (`'420v'` for NV12,
//! `'x420'` for P010, the layouts VideoToolbox takes). Each plane is aliased as a pool
//! texture, so [`YuvConverter::convert`] renders straight into the surface: no copy, no
//! CPU readback. Once the GPU has finished, [`SurfaceExporter::take_ready`] returns a
//! native `IoSurface` frame with `SurfaceSync::Ready` and a pool lease. The encoder adapter
//! (the frame's only consumer) wraps the surface in a `CVPixelBuffer` and drops the frame
//! when VideoToolbox no longer reads it; the ring reuses a surface only after that.
//!
//! This is a marked low-level module (principle P11): every `unsafe` block states its
//! invariant.
//!
//! Hot path: [`SurfaceExporter::export`] and [`SurfaceExporter::take_ready`] do not
//! allocate in RBS code. [`SurfaceExporter::new`] creates every surface up front.

use std::collections::VecDeque;
use std::time::Duration;

use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_surface::{
    IOSurfaceRef, kIOSurfaceAllocSize, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfacePlaneBytesPerElement, kIOSurfacePlaneBytesPerRow, kIOSurfacePlaneHeight,
    kIOSurfacePlaneInfo, kIOSurfacePlaneOffset, kIOSurfacePlaneSize, kIOSurfacePlaneWidth,
    kIOSurfaceWidth,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTextureUsage};
use rbs_core::media::{
    ColorSpace, NativeSurface, Size, SurfaceSync, TextureId, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::metal::{PlaneTexture, alias_plane};
use crate::pool::TexturePool;
use crate::yuv::{self, YuvConverter, YuvFormat};

type Metal = wgpu::hal::api::Metal;

/// Surfaces a [`SurfaceExporter`] may own. Encoders hold a few frames (look-ahead,
/// B-frame reordering); more than this points at a consumer that never releases.
pub const MAX_EXPORT_SURFACES: usize = 32;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v'): NV12, limited range.
pub const FOURCC_NV12: u32 = u32::from_be_bytes(*b"420v");
/// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange` ('x420'): 10-bit codes in the high
/// bits of 16-bit words (P010), limited range.
pub const FOURCC_P010: u32 = u32::from_be_bytes(*b"x420");

/// Plane offsets are page aligned, like CoreVideo's own pixel buffers.
const PLANE_ALIGN: usize = 4096;

/// Counters of one exporter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportStats {
    /// Frames converted into a surface.
    pub converted: u64,
    /// Frames handed out by `take_ready`.
    pub delivered: u64,
    /// `export` calls refused because every surface was pending or leased.
    pub busy: u64,
}

struct Surface {
    /// Keeps the surface alive for the exporter; the plane textures retain it too.
    io: CFRetained<IOSurfaceRef>,
    luma: TextureId,
    chroma: TextureId,
    /// Converted but not yet handed out.
    pending: bool,
}

struct Pending {
    surface: usize,
    submission: wgpu::SubmissionIndex,
    timestamp: Timestamp,
    sequence: u64,
}

/// Converts canvases into IOSurfaces for VideoToolbox. Owned by the render thread.
///
/// Installed but unused costs nothing: an exporter exists only while an encoder that
/// takes Metal memory is Active (P13). Call [`Self::close`] with the pool when the encoder
/// stops; dropping the exporter without it keeps the surfaces until the pool is dropped.
pub struct SurfaceExporter {
    converter: YuvConverter,
    format: VideoFormat,
    surfaces: Vec<Surface>,
    /// Converted surfaces in submission order; capacity is the ring size.
    pending: VecDeque<Pending>,
    /// Where the search for a free surface starts (round robin).
    next: usize,
    stats: ExportStats,
}

impl SurfaceExporter {
    /// Creates `surfaces` IOSurfaces of `format` and `width x height` and aliases their
    /// planes in `pool` (two slots per surface). Not for real-time threads: it allocates.
    ///
    /// `width` and `height` must be even. `surfaces` is 2 to [`MAX_EXPORT_SURFACES`];
    /// pick the frames the encoder holds at once plus two. P010 needs `gpu.p010`.
    pub fn new(
        ctx: &GpuContext,
        pool: &mut TexturePool,
        format: YuvFormat,
        width: u32,
        height: u32,
        surfaces: usize,
    ) -> Result<Self, GpuError> {
        if !(2..=MAX_EXPORT_SURFACES).contains(&surfaces) {
            return Err(GpuError::Unsupported("export ring size outside 2..=32"));
        }
        // Checks the size, the P010 capability and builds the pipelines.
        let converter = YuvConverter::new(ctx, format, width, height)?;
        let layout = Layout::new(ctx, format, width, height)?;
        let mut exporter = Self {
            converter,
            format: VideoFormat::new(
                Size::new(width, height),
                format.pixel_format(),
                ColorSpace::BT709_LIMITED,
            ),
            surfaces: Vec::with_capacity(surfaces),
            pending: VecDeque::with_capacity(surfaces),
            next: 0,
            stats: ExportStats::default(),
        };
        for _ in 0..surfaces {
            match layout.create(ctx, pool) {
                Ok(s) => exporter.surfaces.push(s),
                Err(e) => {
                    exporter.close(pool);
                    return Err(e);
                }
            }
        }
        Ok(exporter)
    }

    /// The format of exported frames: NV12 or P010, BT.709 limited range.
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// Counters since creation.
    pub fn stats(&self) -> ExportStats {
        self.stats
    }

    /// Surfaces in the ring.
    pub fn surfaces(&self) -> usize {
        self.surfaces.len()
    }

    /// Converted frames not yet handed out by [`Self::take_ready`].
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Converts `canvas` into the next free surface. The frame becomes available from
    /// [`Self::take_ready`] once the GPU has finished.
    ///
    /// Hot path; does not allocate in RBS code. Returns [`GpuError::ExportBusy`] when every
    /// surface is pending or still held by the encoder; the caller drops the frame.
    pub fn export(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<(), GpuError> {
        let n = self.surfaces.len();
        let Some(i) = (0..n).map(|k| (self.next + k) % n).find(|&i| {
            let s = &self.surfaces[i];
            !s.pending && pool.lease_count(s.luma) == 0
        }) else {
            self.stats.busy += 1;
            return Err(GpuError::ExportBusy { surfaces: n as u32 });
        };
        let s = &mut self.surfaces[i];
        let submission = self
            .converter
            .convert(ctx, pool, canvas, s.luma, s.chroma)?;
        s.pending = true;
        self.next = (i + 1) % n;
        self.stats.converted += 1;
        // At most one entry per surface, so this never grows past the reserved capacity.
        self.pending.push_back(Pending {
            surface: i,
            submission,
            timestamp,
            sequence,
        });
        Ok(())
    }

    /// The oldest converted frame whose GPU work has completed, as a native `IoSurface`
    /// frame (`SurfaceSync::Ready`) holding a lease on the surface; `None` when no frame is
    /// complete yet. Call in a loop until `None`, once per render tick. Does not block.
    ///
    /// Hot path; does not allocate in RBS code.
    pub fn take_ready(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
    ) -> Result<Option<VideoFrame>, GpuError> {
        let Some(front) = self.pending.front() else {
            return Ok(None);
        };
        let done = ctx
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(front.submission.clone()),
                timeout: Some(Duration::ZERO),
            })
            .is_ok();
        if !done {
            return Ok(None);
        }
        let Some(p) = self.pending.pop_front() else {
            return Ok(None);
        };
        let s = &mut self.surfaces[p.surface];
        s.pending = false;
        let lease = pool.lease(s.luma)?;
        let ptr = std::ptr::from_ref::<IOSurfaceRef>(&s.io) as usize;
        self.stats.delivered += 1;
        // SAFETY: `ptr` is a live IOSurfaceRef of `self.format` (created by `Layout::create`
        // with these sizes and the matching '420v'/'x420' layout). It stays valid while
        // `lease` lives: the lease keeps the luma pool slot, whose Metal texture retains the
        // surface, from being dropped, even after `close`. The exporter writes a surface
        // only when its luma slot has no lease, so it does not write while the frame
        // exists. The GPU writes of this frame have completed (polled above), so the surface
        // is ready on delivery.
        let frame = unsafe {
            VideoFrame::native(
                self.format,
                p.timestamp,
                NativeSurface::io_surface(ptr),
                SurfaceSync::Ready,
                lease,
            )
        };
        Ok(Some(frame.with_sequence(p.sequence)))
    }

    /// Releases the pool slots of every surface. Frames already handed out stay valid
    /// until the encoder drops them; pending frames are discarded. Not for real-time
    /// threads.
    pub fn close(mut self, pool: &mut TexturePool) {
        self.pending.clear();
        for s in self.surfaces.drain(..) {
            // Both ids are still owned by the exporter; release cannot fail for them.
            let _ = pool.release(s.luma);
            let _ = pool.release(s.chroma);
        }
    }
}

/// Byte layout of one surface of the ring, computed once.
struct Layout {
    format: YuvFormat,
    width: u32,
    height: u32,
    luma_row: usize,
    chroma_row: usize,
    chroma_offset: usize,
    alloc: usize,
}

impl Layout {
    fn new(ctx: &GpuContext, format: YuvFormat, width: u32, height: u32) -> Result<Self, GpuError> {
        let (luma_mtl, chroma_mtl) = mtl_formats(format);
        let sample = sample_bytes(format);
        // SAFETY: the HAL device is only borrowed to query alignments; the guard is
        // dropped at the end of this block.
        let (luma_align, chroma_align) = unsafe {
            let hal = ctx
                .device()
                .as_hal::<Metal>()
                .ok_or(GpuError::Unsupported("render device is not Metal"))?;
            let raw = hal.raw_device();
            (
                raw.minimumLinearTextureAlignmentForPixelFormat(luma_mtl),
                raw.minimumLinearTextureAlignmentForPixelFormat(chroma_mtl),
            )
        };
        // SAFETY: the keys are CoreFoundation constants that live for the whole process.
        let row_key = unsafe { kIOSurfacePlaneBytesPerRow };
        let row = |bytes: usize, align: usize| {
            align_up(IOSurfaceRef::align_property(row_key, bytes), align.max(1))
        };
        let w = width as usize;
        let h = height as usize;
        let luma_row = row(w * sample, luma_align);
        // Interleaved CbCr: w/2 texels of two samples.
        let chroma_row = row(w * sample, chroma_align);
        let chroma_offset = align_up(luma_row * h, PLANE_ALIGN);
        let alloc = align_up(chroma_offset + chroma_row * (h / 2), PLANE_ALIGN);
        Ok(Self {
            format,
            width,
            height,
            luma_row,
            chroma_row,
            chroma_offset,
            alloc,
        })
    }

    /// Creates one IOSurface and aliases its planes as pool textures.
    fn create(&self, ctx: &GpuContext, pool: &mut TexturePool) -> Result<Surface, GpuError> {
        let io = self.io_surface()?;
        if io.plane_count() != 2
            || io.width_of_plane(0) != self.width as usize
            || io.height_of_plane(0) != self.height as usize
        {
            return Err(GpuError::Export("IOSurface has an unexpected plane layout"));
        }
        let (luma_mtl, chroma_mtl) = mtl_formats(self.format);
        let usage = MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget;
        let luma_desc = yuv::luma_desc(self.format, self.width, self.height);
        let chroma_desc = yuv::chroma_desc(self.format, self.width, self.height);
        let alias = |plane: usize, desc: crate::pool::TextureDesc, mtl_format| {
            alias_plane(
                ctx,
                &io,
                plane,
                PlaneTexture {
                    format: desc.format,
                    mtl_format,
                    width: desc.width,
                    height: desc.height,
                    mtl_usage: usage,
                    usage: desc.usage,
                },
            )
        };
        let luma_tex = alias(0, luma_desc, luma_mtl)?;
        let chroma_tex = alias(1, chroma_desc, chroma_mtl)?;
        let luma = pool.insert_external(ctx, luma_tex, luma_desc)?;
        let chroma = match pool.insert_external(ctx, chroma_tex, chroma_desc) {
            Ok(id) => id,
            Err(e) => {
                let _ = pool.release(luma);
                return Err(e);
            }
        };
        Ok(Surface {
            io,
            luma,
            chroma,
            pending: false,
        })
    }

    fn io_surface(&self) -> Result<CFRetained<IOSurfaceRef>, GpuError> {
        let sample = sample_bytes(self.format);
        let w = self.width as usize;
        let h = self.height as usize;
        let num = |v: usize| CFNumber::new_isize(v as isize);
        // SAFETY: the keys are CoreFoundation constants that live for the whole process.
        let plane_keys: [&CFString; 6] = unsafe {
            [
                kIOSurfacePlaneWidth,
                kIOSurfacePlaneHeight,
                kIOSurfacePlaneBytesPerRow,
                kIOSurfacePlaneBytesPerElement,
                kIOSurfacePlaneOffset,
                kIOSurfacePlaneSize,
            ]
        };
        let plane = |pw: usize, ph: usize, row: usize, bpe: usize, offset: usize| {
            let values = [
                num(pw),
                num(ph),
                num(row),
                num(bpe),
                num(offset),
                num(row * ph),
            ];
            let values: [&CFNumber; 6] = [
                &values[0], &values[1], &values[2], &values[3], &values[4], &values[5],
            ];
            CFDictionary::from_slices(&plane_keys, &values)
        };
        let planes = [
            plane(w, h, self.luma_row, sample, 0),
            plane(
                w / 2,
                h / 2,
                self.chroma_row,
                2 * sample,
                self.chroma_offset,
            ),
        ];
        let planes = CFArray::from_retained_objects(&planes);
        let fourcc = match self.format {
            YuvFormat::Nv12 => FOURCC_NV12,
            YuvFormat::P010 => FOURCC_P010,
        };
        let width = num(w);
        let height = num(h);
        let alloc = num(self.alloc);
        let pixel_format = CFNumber::new_i32(fourcc as i32);
        // SAFETY: the keys are CoreFoundation constants that live for the whole process.
        let keys: [&CFString; 5] = unsafe {
            [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfacePixelFormat,
                kIOSurfaceAllocSize,
                kIOSurfacePlaneInfo,
            ]
        };
        let values: [&CFType; 5] = [&width, &height, &pixel_format, &alloc, &planes];
        let props = CFDictionary::from_slices(&keys, &values);
        // SAFETY: `props` is a dictionary of IOSurface property keys with values of the
        // documented types (numbers, and an array of plane dictionaries).
        unsafe { IOSurfaceRef::new(props.as_ref()) }
            .ok_or(GpuError::Export("IOSurfaceCreate failed"))
    }
}

fn mtl_formats(format: YuvFormat) -> (MTLPixelFormat, MTLPixelFormat) {
    match format {
        YuvFormat::Nv12 => (MTLPixelFormat::R8Unorm, MTLPixelFormat::RG8Unorm),
        YuvFormat::P010 => (MTLPixelFormat::R16Unorm, MTLPixelFormat::RG16Unorm),
    }
}

fn sample_bytes(format: YuvFormat) -> usize {
    match format {
        YuvFormat::Nv12 => 1,
        YuvFormat::P010 => 2,
    }
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}
