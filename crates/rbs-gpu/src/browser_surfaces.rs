//! Studio-owned `'BGRA'` IOSurfaces a macOS browser helper copies into (ADR-0167
//! decisions 3 and 8). The helper never allocates the surface the render thread
//! samples, so a dead helper cannot fault the studio. macOS only.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_surface::{
    IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight,
    kIOSurfacePixelFormat, kIOSurfaceWidth,
};
use objc2_metal::MTLDevice;

use crate::context::GpuContext;
use crate::error::GpuError;

/// `kCVPixelFormatType_32BGRA`, the layout CEF paints (ADR-0167 context).
const FOURCC_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// A page's slot surfaces, alive while the page's frames can still be imported.
pub struct PageSurfaces {
    surfaces: Vec<CFRetained<IOSurfaceRef>>,
}

// SAFETY: an IOSurface is a kernel object with thread-safe reference counting; the
// value is only read (dimensions, pointer value, Mach port creation), which
// IOSurface allows from any thread.
unsafe impl Send for PageSurfaces {}
// SAFETY: as `Send`.
unsafe impl Sync for PageSurfaces {}

impl PageSurfaces {
    /// `count` surfaces of `width` x `height`, 4 bytes per pixel.
    pub fn create(width: u32, height: u32, count: usize) -> Result<Self, GpuError> {
        if width == 0 || height == 0 || count == 0 {
            return Err(GpuError::InvalidSize { width, height });
        }
        let surfaces = (0..count)
            .map(|_| bgra_surface(width, height))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { surfaces })
    }

    /// How many surfaces there are.
    pub fn count(&self) -> usize {
        self.surfaces.len()
    }

    /// The `IOSurfaceRef` value of `slot`, for `NativeSurface::IoSurface`. Valid while
    /// `self` lives.
    pub fn surface(&self, slot: usize) -> Option<usize> {
        self.surfaces
            .get(slot)
            .map(|s| std::ptr::from_ref::<IOSurfaceRef>(s) as usize)
    }

    /// Device checks only (ADR-0167 decision 10): reads `points` pixels of `slot`
    /// spread over the surface, as BGRA, through a read-only CPU lock. Never on the
    /// frame path.
    #[doc(hidden)]
    pub fn sample(&self, slot: usize, points: usize) -> Option<Vec<[u8; 4]>> {
        use objc2_io_surface::IOSurfaceLockOptions;
        let surface = self.surfaces.get(slot)?;
        // SAFETY: a read-only lock of a live surface; `seed` may be null.
        if unsafe { surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) } != 0 {
            return None;
        }
        let (w, h, row) = (surface.width(), surface.height(), surface.bytes_per_row());
        let base = surface.base_address().as_ptr().cast::<u8>();
        let side = points.isqrt().max(2);
        let mut out = Vec::with_capacity(side * side);
        for j in 0..side {
            for i in 0..side {
                let (x, y) = (i * (w - 1) / (side - 1), j * (h - 1) / (side - 1));
                // SAFETY: `x < w` and `y < h`; the locked surface is `h` rows of
                // `row` bytes with 4 bytes per pixel.
                let px = unsafe { std::ptr::read(base.add(y * row + x * 4).cast::<[u8; 4]>()) };
                out.push(px);
            }
        }
        // SAFETY: matches the lock above.
        unsafe { surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) };
        Some(out)
    }

    /// One new send right per surface (`IOSurfaceCreateMachPort`), in slot order, for
    /// the handover. The caller owns the rights.
    pub fn mach_ports(&self) -> Result<Vec<u32>, GpuError> {
        let mut ports = Vec::with_capacity(self.surfaces.len());
        for surface in &self.surfaces {
            let port = surface.create_mach_port();
            if port == 0 {
                ports.into_iter().for_each(rbs_mach::deallocate);
                return Err(GpuError::Export("IOSurfaceCreateMachPort failed"));
            }
            ports.push(port);
        }
        Ok(ports)
    }
}

fn bgra_surface(w: u32, h: u32) -> Result<CFRetained<IOSurfaceRef>, GpuError> {
    // SAFETY: the key is a CoreFoundation constant that lives for the whole process.
    let row_key = unsafe { kIOSurfaceBytesPerRow };
    let row = IOSurfaceRef::align_property(row_key, w as usize * 4);
    let num = |v: usize| CFNumber::new_isize(v as isize);
    let (width, height, bpe, bpr) = (num(w as usize), num(h as usize), num(4), num(row));
    let pixel_format = CFNumber::new_i32(FOURCC_BGRA as i32);
    // SAFETY: the keys are CoreFoundation constants that live for the whole process.
    let keys: [&CFString; 5] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfaceBytesPerRow,
            kIOSurfacePixelFormat,
        ]
    };
    let values: [&CFType; 5] = [&width, &height, &bpe, &bpr, &pixel_format];
    let props = CFDictionary::from_slices(&keys, &values);
    // SAFETY: `props` maps IOSurface property keys to numbers, the documented types.
    let io = unsafe { IOSurfaceRef::new(props.as_ref()) }
        .ok_or(GpuError::Export("IOSurfaceCreate failed"))?;
    if io.width() != w as usize
        || io.height() != h as usize
        || io.plane_count() > 1
        || io.bytes_per_element() != 4
        || io.pixel_format() != FOURCC_BGRA
    {
        return Err(GpuError::Export("IOSurface has an unexpected layout"));
    }
    Ok(io)
}

/// The `registryID` of the render device's `MTLDevice`, which a browser helper must
/// copy on (ADR-0167 decision 2). `None` off Metal.
pub fn metal_registry_id(ctx: &GpuContext) -> Option<u64> {
    // SAFETY: the HAL device is only borrowed to read one property; wgpu state is not
    // changed and the guard is dropped at the end of this function.
    let hal = unsafe { ctx.device().as_hal::<wgpu::hal::api::Metal>() }?;
    Some(hal.raw_device().registryID())
}

/// How many Metal devices the system has. The browser GPU path needs exactly one
/// (ADR-0167 decision 2: CEF's GPU process and the copy then run on the same GPU).
pub fn metal_device_count() -> usize {
    objc2_metal::MTLCopyAllDevices().count()
}
