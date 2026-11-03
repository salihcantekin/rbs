//! Metal backend of the importer: `IOSurfaceRef` aliased as an `MTLTexture`.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.
//!
//! IOSurfaces are not tied to one `MTLDevice`, so there is no device identity to check.
//! The Metal texture retains the surface, so its address cannot be reused for another
//! surface while the importer caches the entry; the address is therefore a safe cache key.

use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use rbs_core::media::SurfaceSync;

use super::{Opened, USAGE};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TextureDesc;

type Metal = wgpu::hal::api::Metal;

/// `kCVPixelFormatType_32BGRA` ('BGRA').
const FOURCC_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCVPixelFormatType_32RGBA` ('RGBA').
const FOURCC_RGBA: u32 = u32::from_be_bytes(*b"RGBA");
/// `kCVPixelFormatType_64RGBAHalf` ('RGhA').
const FOURCC_RGBA_HALF: u32 = u32::from_be_bytes(*b"RGhA");

/// Checks the sync kind. Metal producers deliver surfaces that are ready to read.
pub(super) fn validate(sync: SurfaceSync) -> Result<(), GpuError> {
    match sync {
        SurfaceSync::Ready => Ok(()),
        _ => Err(GpuError::Unsupported("sync kind on Metal; deliver Ready")),
    }
}

/// Opens an `IOSurfaceRef` on the render device and wraps it without a copy.
pub(super) fn open(ctx: &GpuContext, surface: usize) -> Result<Opened, GpuError> {
    if surface == 0 {
        return Err(GpuError::Import("null IOSurfaceRef".into()));
    }
    // SAFETY: `surface` is a non-null `IOSurfaceRef` that the producer keeps alive while
    // its lease is held (VideoFrame::native contract); the importer holds the lease for
    // this whole call. The reference does not outlive the call.
    let io: &IOSurfaceRef = unsafe { &*(surface as *const IOSurfaceRef) };
    if io.plane_count() > 1 {
        return Err(GpuError::Unsupported("planar IOSurface"));
    }
    let (format, mtl_format) = map_format(io.pixel_format())?;
    let width = u32::try_from(io.width()).map_err(|_| GpuError::InvalidSize {
        width: u32::MAX,
        height: 0,
    })?;
    let height = u32::try_from(io.height()).map_err(|_| GpuError::InvalidSize {
        width,
        height: u32::MAX,
    })?;
    if width == 0 || height == 0 || width.max(height) > ctx.max_texture_dimension() {
        return Err(GpuError::InvalidSize { width, height });
    }

    let texture = alias_plane(
        ctx,
        io,
        0,
        PlaneTexture {
            format,
            mtl_format,
            width,
            height,
            mtl_usage: MTLTextureUsage::ShaderRead,
            usage: USAGE,
        },
    )?;
    Ok(Opened {
        texture,
        desc: TextureDesc {
            width,
            height,
            format,
            usage: USAGE,
        },
    })
}

/// How one IOSurface plane is aliased: its formats, size and usage.
pub(crate) struct PlaneTexture {
    /// The wgpu format of the wrapper.
    pub format: wgpu::TextureFormat,
    /// The matching Metal format; its texel size equals the plane's bytes per element.
    pub mtl_format: MTLPixelFormat,
    /// Plane width in texels.
    pub width: u32,
    /// Plane height in texels.
    pub height: u32,
    /// Metal usage of the aliasing texture.
    pub mtl_usage: MTLTextureUsage,
    /// wgpu usage of the wrapper; must be covered by `mtl_usage`.
    pub usage: wgpu::TextureUsages,
}

/// Wraps plane `plane` of `io` as a wgpu texture on the render device, without a copy.
///
/// Opening is not a per-frame call: it allocates (Metal and wgpu objects).
pub(crate) fn alias_plane(
    ctx: &GpuContext,
    io: &IOSurfaceRef,
    plane: usize,
    p: PlaneTexture,
) -> Result<wgpu::Texture, GpuError> {
    // SAFETY: the HAL device is only borrowed to create one texture; wgpu state is not
    // changed and the guard is dropped at the end of this function.
    let hal = unsafe { ctx.device().as_hal::<Metal>() }
        .ok_or(GpuError::Unsupported("render device is not Metal"))?;
    // SAFETY: plain descriptor constructor; the caller checked the format and sizes
    // against the device limits.
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            p.mtl_format,
            p.width as usize,
            p.height as usize,
            false,
        )
    };
    desc.setUsage(p.mtl_usage);
    let raw = hal
        .raw_device()
        .newTextureWithDescriptor_iosurface_plane(&desc, io, plane)
        .ok_or_else(|| GpuError::Import("newTextureWithDescriptor:iosurface: failed".into()))?;
    drop(hal);

    let size = wgpu::Extent3d {
        width: p.width,
        height: p.height,
        depth_or_array_layers: 1,
    };
    // SAFETY: `raw` is a single-mip 2D texture of `p.format` and `size`, created on this
    // device just above.
    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw,
            p.format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width: p.width,
                height: p.height,
                depth: 1,
            },
            None,
        )
    };
    // SAFETY: the HAL texture belongs to this device and matches the descriptor; its Metal
    // usage covers `p.usage`. Metal has no layouts, so any initial state is correct and
    // PRESENT keeps the hand-back path shared with D3D12.
    let texture = unsafe {
        ctx.device().create_texture_from_hal::<Metal>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("rbs-gpu iosurface"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: p.format,
                usage: p.usage,
                view_formats: &[],
            },
            wgpu::TextureUses::PRESENT,
        )
    };
    Ok(texture)
}

fn map_format(fourcc: u32) -> Result<(wgpu::TextureFormat, MTLPixelFormat), GpuError> {
    Ok(match fourcc {
        FOURCC_BGRA => (wgpu::TextureFormat::Bgra8Unorm, MTLPixelFormat::BGRA8Unorm),
        FOURCC_RGBA => (wgpu::TextureFormat::Rgba8Unorm, MTLPixelFormat::RGBA8Unorm),
        FOURCC_RGBA_HALF => (
            wgpu::TextureFormat::Rgba16Float,
            MTLPixelFormat::RGBA16Float,
        ),
        _ => return Err(GpuError::Unsupported("IOSurface pixel format")),
    })
}
