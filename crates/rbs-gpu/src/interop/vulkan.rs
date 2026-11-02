//! Vulkan backend of the importer: single-plane Linux DMA-BUFs.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.
//!
//! The import goes through wgpu-hal's `texture_from_dmabuf_fd`
//! (`VK_EXT_external_memory_dma_buf` with `VK_EXT_image_drm_format_modifier`). The cache key
//! is the DMA-BUF's inode: unique per buffer while the buffer lives, and the imported memory
//! keeps the buffer alive while the entry is cached, so a key never names another buffer.
//! File descriptor numbers are not used as keys because the OS reuses them.

use std::mem::ManuallyDrop;
use std::os::fd::{BorrowedFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

use rbs_core::media::{DmaBufSurface, SurfaceSync};

use super::{Opened, USAGE};
use crate::context::{CAP_IMPORT_DMABUF, GpuContext};
use crate::error::GpuError;
use crate::pool::TextureDesc;

type Vulkan = wgpu::hal::api::Vulkan;

/// DRM fourcc `AR24` (`DRM_FORMAT_ARGB8888`): bytes B, G, R, A in memory.
const DRM_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// DRM fourcc `AB24` (`DRM_FORMAT_ABGR8888`): bytes R, G, B, A in memory.
const DRM_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
/// DRM fourcc `XR24` (`DRM_FORMAT_XRGB8888`): as `AR24` with an undefined fourth byte.
const DRM_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// DRM fourcc `XB24` (`DRM_FORMAT_XBGR8888`): as `AB24` with an undefined fourth byte.
const DRM_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

/// Checks the surface and sync kind, and returns the cache key (the buffer's inode).
pub(super) fn validate(
    ctx: &GpuContext,
    surface: &DmaBufSurface,
    sync: SurfaceSync,
) -> Result<u64, GpuError> {
    if !ctx.has(&CAP_IMPORT_DMABUF) {
        return Err(GpuError::Unsupported(
            "DMA-BUF import needs a Vulkan device with external DMA-BUF memory",
        ));
    }
    if sync != SurfaceSync::Ready {
        return Err(GpuError::Unsupported("sync kind on Vulkan; deliver Ready"));
    }
    let [plane] = surface.planes() else {
        return Err(GpuError::Unsupported("multi-plane DMA-BUF"));
    };
    map_format(surface.fourcc())?;
    if plane.fd < 0 {
        return Err(GpuError::Import("negative DMA-BUF file descriptor".into()));
    }
    // SAFETY: the producer keeps `fd` open while its lease is held (VideoFrame::native
    // contract) and the importer holds the lease. The `File` is never dropped, so the
    // descriptor stays owned by the producer; `metadata` only calls fstat.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(plane.fd) });
    let meta = file
        .metadata()
        .map_err(|e| GpuError::Import(e.to_string()))?;
    Ok(meta.ino())
}

/// Imports the DMA-BUF on the render device without a copy.
pub(super) fn open(
    ctx: &GpuContext,
    surface: &DmaBufSurface,
    (width, height): (u32, u32),
) -> Result<Opened, GpuError> {
    let [plane] = surface.planes() else {
        return Err(GpuError::Unsupported("multi-plane DMA-BUF"));
    };
    let format = map_format(surface.fourcc())?;
    if width == 0 || height == 0 || width.max(height) > ctx.max_texture_dimension() {
        return Err(GpuError::InvalidSize { width, height });
    }
    // SAFETY: as in `validate`, the descriptor is open for this call. The duplicate is
    // ours: wgpu-hal takes ownership of it and closes it on failure.
    let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) }
        .try_clone_to_owned()
        .map_err(|e| GpuError::Import(e.to_string()))?;

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("rbs-gpu imported"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let hal_texture = {
        // SAFETY: the HAL device is only borrowed to create one texture; wgpu state is not
        // changed and the guard is dropped before the texture is registered.
        let hal = unsafe { ctx.device().as_hal::<Vulkan>() }
            .ok_or(GpuError::Unsupported("render device is not Vulkan"))?;
        // SAFETY: the device has VULKAN_EXTERNAL_MEMORY_DMA_BUF (checked in `validate`);
        // `fd` is a DMA-BUF holding one plane of `format` at `width x height` with the
        // producer's modifier, stride and offset (VideoFrame::native contract).
        unsafe {
            hal.texture_from_dmabuf_fd(
                fd,
                &hal_desc,
                surface.modifier(),
                u64::from(plane.stride),
                u64::from(plane.offset),
            )
        }
        .map_err(|e| GpuError::Import(e.to_string()))?
    };
    // SAFETY: the HAL texture was created on this device from `hal_desc`, which matches
    // the descriptor below. Its contents come from the producer; the image starts in
    // UNDEFINED layout, which UNINITIALIZED states (wgpu does not clear textures created
    // from HAL), as is usual for DMA-BUF imports.
    let texture = unsafe {
        ctx.device().create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("rbs-gpu imported"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: USAGE,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    };
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

fn map_format(fourcc: u32) -> Result<wgpu::TextureFormat, GpuError> {
    match fourcc {
        // X formats alias the same layout; the source draws them with `Layer::opaque`.
        DRM_ARGB8888 | DRM_XRGB8888 => Ok(wgpu::TextureFormat::Bgra8Unorm),
        DRM_ABGR8888 | DRM_XBGR8888 => Ok(wgpu::TextureFormat::Rgba8Unorm),
        _ => Err(GpuError::Unsupported(
            "DMA-BUF format (ARGB8888, XRGB8888, ABGR8888, XBGR8888)",
        )),
    }
}
