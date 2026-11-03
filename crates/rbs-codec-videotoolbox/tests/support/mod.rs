//! Test support: a `metal` frame source that hands out IOSurfaces the way the `rbs-gpu`
//! exporter does (ADR-0025): a fixed ring of bi-planar surfaces, `SurfaceSync::Ready`,
//! and a lease per frame whose owner is the ring. The ring counts references, so tests
//! can check that the encoder releases every frame.
//!
//! The adapter never depends on `rbs-gpu`; this source builds the surfaces with
//! CoreVideo instead, in the layout the exporter uses.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Test support.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey,
};
use objc2_io_surface::IOSurfaceRef;
use rbs_codec::conformance::FrameSource;
use rbs_core::media::{
    ColorRange, Lease, LeaseOwner, MemoryKind, NativeSurface, PixelFormat, SurfaceSync,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

/// Surfaces of one ring: more than the encoder's frames in flight, so the source
/// never runs dry while the encoder still holds frames.
pub const RING: usize = 20;

struct Entry {
    buffer: CFRetained<CVPixelBuffer>,
    surface: CFRetained<IOSurfaceRef>,
    count: AtomicI64,
}

/// The lease owner: one reference count per surface.
pub struct Ring {
    format: VideoFormat,
    entries: Vec<Entry>,
}

// SAFETY: test-only; the ring retains every surface for its whole life, and the counts
// are exact atomics. CoreVideo objects may be shared between threads.
unsafe impl Send for Ring {}
// SAFETY: see `Send`.
unsafe impl Sync for Ring {}

// SAFETY: surfaces live as long as the ring (which every lease keeps alive) and are only
// written by `IoSurfaceSource::frame` while their count is zero.
unsafe impl LeaseOwner for Ring {
    fn retain(&self, token: u64) {
        self.entries[token as usize]
            .count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn release(&self, token: u64) {
        self.entries[token as usize]
            .count
            .fetch_sub(1, Ordering::Release);
    }
}

impl Ring {
    /// References still held, over all surfaces.
    pub fn held(&self) -> i64 {
        self.entries
            .iter()
            .map(|e| e.count.load(Ordering::Acquire))
            .sum()
    }
}

fn fourcc(format: &VideoFormat) -> u32 {
    let full = format.color.range == ColorRange::Full;
    let code = match (format.pixel_format, full) {
        (PixelFormat::Nv12, false) => b"420v",
        (PixelFormat::Nv12, true) => b"420f",
        (PixelFormat::P010, false) => b"x420",
        (PixelFormat::P010, true) => b"xf20",
        _ => unreachable!("unsupported test format"),
    };
    u32::from_be_bytes(*code)
}

/// An IOSurface-backed pixel buffer of `format`.
pub fn surface_buffer(format: &VideoFormat) -> CFRetained<CVPixelBuffer> {
    let empty = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    // SAFETY: the key is a CoreFoundation constant.
    let key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attributes = CFDictionary::<CFString, CFType>::from_slices(&[key], &[&empty]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid attributes and out pointer.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            format.size.width as usize,
            format.size.height as usize,
            fourcc(format),
            Some(attributes.as_opaque()),
            NonNull::from(&mut raw),
        )
    };
    assert_eq!(status, 0, "CVPixelBufferCreate");
    // SAFETY: a successful create hands over a +1 reference.
    unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) }
}

/// Fills the luma plane with a moving gradient and chroma with grey.
pub fn fill(buffer: &CVPixelBuffer, format: &VideoFormat, index: u64) {
    let ten_bit = format.pixel_format == PixelFormat::P010;
    // SAFETY: valid buffer, unlocked below.
    let locked = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
    assert_eq!(locked, 0);
    for plane in 0..2 {
        let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
        let rows = CVPixelBufferGetHeightOfPlane(buffer, plane);
        for y in 0..rows {
            // SAFETY: each row has `stride` bytes while locked.
            let row = unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), stride) };
            if ten_bit {
                for (x, px) in row.chunks_exact_mut(2).enumerate() {
                    let v: u16 = if plane == 0 {
                        (((x + y + index as usize * 4) % 876 + 64) as u16) << 6
                    } else {
                        512 << 6
                    };
                    px.copy_from_slice(&v.to_le_bytes());
                }
            } else {
                for (x, px) in row.iter_mut().enumerate() {
                    *px = if plane == 0 {
                        (x + y + index as usize * 4) as u8
                    } else {
                        128
                    };
                }
            }
        }
    }
    // SAFETY: locked above.
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
}

/// A `metal` frame source over a ring of IOSurfaces.
pub struct IoSurfaceSource {
    rings: Vec<Arc<Ring>>,
    next: usize,
}

impl Default for IoSurfaceSource {
    fn default() -> Self {
        Self::new()
    }
}

impl IoSurfaceSource {
    /// A source with no rings yet; one is created per format on first use.
    pub fn new() -> Self {
        Self {
            rings: Vec::new(),
            next: 0,
        }
    }

    fn ring(&mut self, format: &VideoFormat) -> Arc<Ring> {
        if let Some(r) = self.rings.iter().find(|r| r.format == *format) {
            return Arc::clone(r);
        }
        let entries = (0..RING)
            .map(|_| {
                let buffer = surface_buffer(format);
                let surface = CVPixelBufferGetIOSurface(Some(&buffer)).expect("IOSurface");
                Entry {
                    buffer,
                    surface,
                    count: AtomicI64::new(0),
                }
            })
            .collect();
        let ring = Arc::new(Ring {
            format: *format,
            entries,
        });
        self.rings.push(Arc::clone(&ring));
        ring
    }

    /// References the encoder still holds, over every ring.
    pub fn held(&self) -> i64 {
        self.rings.iter().map(|r| r.held()).sum()
    }

    /// Distinct surfaces handed out so far (at most `RING` per format).
    pub fn surfaces(&self) -> usize {
        self.rings.len() * RING
    }
}

impl FrameSource for IoSurfaceSource {
    fn memory(&self) -> MemoryKind {
        MemoryKind::Metal
    }

    fn supports(&self, pixel_format: PixelFormat) -> bool {
        matches!(pixel_format, PixelFormat::Nv12 | PixelFormat::P010)
    }

    fn frame(
        &mut self,
        format: &VideoFormat,
        index: u64,
        timestamp: Timestamp,
    ) -> Option<VideoFrame> {
        if !self.supports(format.pixel_format) {
            return None;
        }
        let ring = self.ring(format);
        let slot = (0..RING)
            .map(|i| (self.next + i) % RING)
            .find(|&i| ring.entries[i].count.load(Ordering::Acquire) == 0)?;
        self.next = (slot + 1) % RING;
        let entry = &ring.entries[slot];
        fill(&entry.buffer, format, index);
        ring.retain(slot as u64);
        let surface = CFRetained::as_ptr(&entry.surface).as_ptr() as usize;
        // SAFETY: the reference counted just above is handed to the lease.
        let lease = unsafe { Lease::adopt(Arc::clone(&ring) as Arc<dyn LeaseOwner>, slot as u64) };
        // SAFETY: the surface is a live IOSurface of `format`, retained by the ring (kept
        // alive by the lease), and not written while its count is above zero.
        let frame = unsafe {
            VideoFrame::native(
                *format,
                timestamp,
                NativeSurface::io_surface(surface),
                SurfaceSync::Ready,
                lease,
            )
        };
        Some(frame.with_sequence(index))
    }
}
