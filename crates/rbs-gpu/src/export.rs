//! Native surface export for encoders (spec `docs/specs/gpu-compositor.md#native-export`,
//! ADR-0025).
//!
//! macOS: [`SurfaceExporter`] owns a fixed ring of bi-planar IOSurfaces (`'420v'` for NV12,
//! `'x420'` for P010, the layouts VideoToolbox takes). Each plane is aliased as a Metal
//! texture, so the YUV conversion renders straight into the surface: no copy, no CPU
//! readback. Once the GPU has finished, [`SurfaceExporter::take_ready`] returns a native
//! `IoSurface` frame with `SurfaceSync::Ready` and a lease on the ring. The encoder adapter
//! (the frame's only consumer) wraps the surface in a `CVPixelBuffer` and drops the frame
//! when VideoToolbox no longer reads it; the ring reuses a surface only after that.
//!
//! The lease owner is the ring itself and it retains every IOSurface, so an exported
//! frame keeps its surface alive whatever is dropped first (exporter, pool, device).
//!
//! This is a marked low-level module (principle P11): every `unsafe` block states its
//! invariant.
//!
//! Hot path: [`SurfaceExporter::export`] and [`SurfaceExporter::take_ready`] do not
//! allocate in RBS code. [`SurfaceExporter::new`] creates every surface up front.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
    ColorRange, ColorSpace, Lease, LeaseOwner, NativeSurface, Size, SurfaceSync, TextureId,
    VideoFormat, VideoFrame,
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
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` ('420f'): NV12, full range.
pub const FOURCC_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
/// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange` ('x420'): 10-bit codes in the high
/// bits of 16-bit words (P010), limited range.
pub const FOURCC_P010: u32 = u32::from_be_bytes(*b"x420");
/// `kCVPixelFormatType_420YpCbCr10BiPlanarFullRange` ('xf20'): P010, full range.
pub const FOURCC_P010_FULL: u32 = u32::from_be_bytes(*b"xf20");

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

/// A retained IOSurface that may be shared between threads.
struct SharedSurface(CFRetained<IOSurfaceRef>);

// SAFETY: IOSurface objects are thread-safe: retain/release are atomic and the surface
// may be used from any thread (and process). The ring only keeps the surface alive and
// reads its address; it never locks or writes the surface from another thread.
unsafe impl Send for SharedSurface {}
// SAFETY: see `Send`; shared references only read the (immutable) address.
unsafe impl Sync for SharedSurface {}

/// The lease owner of exported frames: one retained surface and one lease count per
/// ring entry. Shared with encoder threads through leases.
struct Ring {
    surfaces: Box<[(SharedSurface, AtomicU32)]>,
}

// SAFETY: a surface is written by the exporter only while its count is zero (checked
// with `Acquire` in `SurfaceExporter::export`, paired with the `Release` decrement here).
// `retain` is reachable only through a `Lease` clone, which already holds a reference,
// or from `take_ready` on the exporter's own thread, so a count never rises from zero
// behind the exporter's back. The surfaces are retained by the ring, which every lease
// keeps alive, so they stay valid for as long as any lease exists.
unsafe impl LeaseOwner for Ring {
    fn retain(&self, token: u64) {
        if let Some((_, count)) = self.surfaces.get(token as usize) {
            // Relaxed: the caller already holds a reference, so nothing is published.
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, token: u64) {
        if let Some((_, count)) = self.surfaces.get(token as usize) {
            // Release: the encoder's reads happen before the exporter writes again.
            count.fetch_sub(1, Ordering::Release);
        }
    }
}

/// The exporter's GPU side of one ring entry: views of the aliased planes.
struct Planes {
    luma: wgpu::TextureView,
    chroma: wgpu::TextureView,
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
/// takes Metal memory is Active (P13). It holds no pool slots; dropping it frees its GPU
/// objects, and each surface lives on only while an exported frame holds it.
pub struct SurfaceExporter {
    converter: YuvConverter,
    format: VideoFormat,
    ring: Arc<Ring>,
    planes: Vec<Planes>,
    /// Converted surfaces in submission order; capacity is the ring size.
    pending: VecDeque<Pending>,
    /// Where the search for a free surface starts (round robin).
    next: usize,
    stats: ExportStats,
}

impl SurfaceExporter {
    /// Creates `surfaces` IOSurfaces of `format` and `width x height` and aliases their
    /// planes on the render device. Not for real-time threads: it allocates.
    ///
    /// `width` and `height` must be even. `surfaces` is 2 to [`MAX_EXPORT_SURFACES`]; see
    /// the spec for sizing. P010 needs `gpu.p010`. The device must have unified memory
    /// (Apple silicon): coherency of GPU-written IOSurfaces on discrete GPUs is not
    /// verified yet, so they get `GpuError::Unsupported`.
    pub fn new(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
        surfaces: usize,
    ) -> Result<Self, GpuError> {
        Self::with_color(
            ctx,
            format,
            width,
            height,
            surfaces,
            ColorSpace::BT709_LIMITED,
        )
    }

    /// [`Self::new`] for `color`: [`ColorSpace::BT709_LIMITED`] or [`ColorSpace::SRGB_VIDEO`].
    /// Full range allocates `'420f'` or `'xf20'`.
    pub fn with_color(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
        surfaces: usize,
        color: ColorSpace,
    ) -> Result<Self, GpuError> {
        if !(2..=MAX_EXPORT_SURFACES).contains(&surfaces) {
            return Err(GpuError::Unsupported("export ring size outside 2..=32"));
        }
        // Checks the size, the colour, the P010 capability and builds the pipelines.
        let converter = YuvConverter::with_color(ctx, format, width, height, color)?;
        let layout = Layout::new(ctx, format, width, height, color)?;
        let mut ring = Vec::with_capacity(surfaces);
        let mut planes = Vec::with_capacity(surfaces);
        for _ in 0..surfaces {
            let (io, p) = layout.create(ctx)?;
            ring.push((SharedSurface(io), AtomicU32::new(0)));
            planes.push(p);
        }
        Ok(Self {
            converter,
            format: VideoFormat::new(Size::new(width, height), format.pixel_format(), color),
            ring: Arc::new(Ring {
                surfaces: ring.into_boxed_slice(),
            }),
            planes,
            pending: VecDeque::with_capacity(surfaces),
            next: 0,
            stats: ExportStats::default(),
        })
    }

    /// The format of exported frames: NV12 or P010, and the colour given at creation.
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// Counters since creation.
    pub fn stats(&self) -> ExportStats {
        self.stats
    }

    /// Surfaces in the ring.
    pub fn surfaces(&self) -> usize {
        self.planes.len()
    }

    /// Converted frames not yet handed out by [`Self::take_ready`].
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The lease owner of exported frames. The engine keeps this handle until the encoder
    /// has shut down and drops it on a background thread, so the last reference (and the
    /// release of the surfaces) never falls on a real-time thread (media-types spec,
    /// "Lifetime").
    pub fn lease_owner(&self) -> Arc<dyn LeaseOwner> {
        self.ring.clone()
    }

    /// Exported frames the encoder still holds.
    pub fn held(&self) -> usize {
        self.ring
            .surfaces
            .iter()
            .filter(|(_, c)| c.load(Ordering::Acquire) > 0)
            .count()
    }

    /// Converts `canvas` (a canvas texture in `pool`) into the next free surface. The
    /// frame becomes available from [`Self::take_ready`] once the GPU has finished.
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
        let n = self.planes.len();
        let ring = &self.ring.surfaces;
        let planes = &self.planes;
        let Some(i) = (0..n).map(|k| (self.next + k) % n).find(|&i| {
            // Acquire: pairs with the `Release` decrement of the encoder's last lease, so
            // its reads of the surface happen before this conversion writes it.
            !planes[i].pending && ring[i].1.load(Ordering::Acquire) == 0
        }) else {
            self.stats.busy += 1;
            return Err(GpuError::ExportBusy { surfaces: n as u32 });
        };
        let p = &mut self.planes[i];
        let submission = self
            .converter
            .convert_views(ctx, pool, canvas, &p.luma, &p.chroma)?;
        p.pending = true;
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
    /// Returns `GpuError::DeviceLost` once the device is lost: frames converted on a lost
    /// device are never delivered. Hot path; does not allocate in RBS code.
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        let Some(front) = self.pending.front() else {
            return Ok(None);
        };
        match ctx.device().poll(wgpu::PollType::Wait {
            submission_index: Some(front.submission.clone()),
            timeout: Some(Duration::ZERO),
        }) {
            Ok(_) => {}
            Err(wgpu::PollError::Timeout) => return Ok(None),
            Err(wgpu::PollError::WrongSubmissionIndex(..)) => {
                return Err(GpuError::Unsupported("submission of another device"));
            }
        }
        // Metal counts a failed command buffer as completed. A lost device is caught here;
        // a non-fatal command buffer error is not (residual risk, ADR-0025).
        if ctx.metrics().device_lost() {
            self.pending.clear();
            for p in &mut self.planes {
                p.pending = false;
            }
            return Err(GpuError::DeviceLost);
        }
        let Some(p) = self.pending.pop_front() else {
            return Ok(None);
        };
        self.planes[p.surface].pending = false;
        let token = p.surface as u64;
        self.ring.retain(token);
        let owner: Arc<dyn LeaseOwner> = self.ring.clone();
        // SAFETY: one reference to entry `token` was counted just above and is handed over.
        let lease = unsafe { Lease::adopt(owner, token) };
        let ptr = std::ptr::from_ref::<IOSurfaceRef>(&self.ring.surfaces[p.surface].0.0) as usize;
        self.stats.delivered += 1;
        // SAFETY: `ptr` is a live IOSurfaceRef of `self.format` (created by `Layout::create`
        // with these sizes and the matching '420v'/'x420' layout, checked after creation).
        // The ring retains it and `lease` keeps the ring alive, so it stays valid while the
        // lease lives, whatever else is dropped. The exporter writes a surface only when its
        // count is zero, so it does not write while this frame exists. The GPU writes of
        // this frame have completed (polled above, device not lost), so the surface is
        // ready on delivery.
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
}

/// Byte layout of one surface of the ring, computed once.
struct Layout {
    format: YuvFormat,
    fourcc: u32,
    width: u32,
    height: u32,
    luma_row: usize,
    chroma_row: usize,
    chroma_offset: usize,
    alloc: usize,
}

impl Layout {
    fn new(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
        color: ColorSpace,
    ) -> Result<Self, GpuError> {
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
            if !raw.hasUnifiedMemory() {
                return Err(GpuError::Unsupported(
                    "IOSurface export on a GPU without unified memory",
                ));
            }
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
        let fourcc = match (format, color.range) {
            (YuvFormat::Nv12, ColorRange::Full) => FOURCC_NV12_FULL,
            (YuvFormat::Nv12, _) => FOURCC_NV12,
            (YuvFormat::P010, ColorRange::Full) => FOURCC_P010_FULL,
            (YuvFormat::P010, _) => FOURCC_P010,
        };
        Ok(Self {
            format,
            fourcc,
            width,
            height,
            luma_row,
            chroma_row,
            chroma_offset,
            alloc,
        })
    }

    /// Creates one IOSurface and aliases its planes as render targets.
    fn create(&self, ctx: &GpuContext) -> Result<(CFRetained<IOSurfaceRef>, Planes), GpuError> {
        let io = self.io_surface()?;
        let w = self.width as usize;
        let h = self.height as usize;
        let sample = sample_bytes(self.format);
        let plane_ok = |p: usize, pw: usize, ph: usize, bpe: usize, row: usize| {
            io.width_of_plane(p) == pw
                && io.height_of_plane(p) == ph
                && io.bytes_per_element_of_plane(p) == bpe
                && io.bytes_per_row_of_plane(p) >= row
        };
        if io.plane_count() != 2
            || !plane_ok(0, w, h, sample, w * sample)
            || !plane_ok(1, w / 2, h / 2, 2 * sample, w * sample)
        {
            return Err(GpuError::Export("IOSurface has an unexpected plane layout"));
        }
        let (luma_mtl, chroma_mtl) = mtl_formats(self.format);
        let usage = MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget;
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
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
        };
        let luma = alias(
            0,
            yuv::luma_desc(self.format, self.width, self.height),
            luma_mtl,
        )?;
        let chroma = alias(
            1,
            yuv::chroma_desc(self.format, self.width, self.height),
            chroma_mtl,
        )?;
        Ok((
            io,
            Planes {
                luma,
                chroma,
                pending: false,
            },
        ))
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
        let fourcc = self.fourcc;
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
