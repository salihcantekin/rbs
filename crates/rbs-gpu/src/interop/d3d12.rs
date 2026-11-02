//! D3D12 backend of the importer: NT shared handles with shared timeline fences.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.

use std::ffi::c_void;

use rbs_core::media::SurfaceSync;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};

use super::{Opened, USAGE};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TextureDesc;

type Dx12 = wgpu::hal::api::Dx12;

struct Fence {
    handle: u64,
    fence: ID3D12Fence,
}

/// Shared fences opened so far. Empty until the first fenced import (P13).
#[derive(Default)]
pub(super) struct Fences {
    fences: Vec<Fence>,
}

pub(crate) fn adapter_luid(device: &wgpu::Device) -> Option<u64> {
    // SAFETY: the HAL device is only borrowed to read its adapter LUID; no wgpu state is
    // changed and the guard is dropped before returning.
    let hal = unsafe { device.as_hal::<Dx12>() }?;
    // SAFETY: `raw_device` is a live ID3D12Device for the guard's lifetime.
    let luid = unsafe { hal.raw_device().GetAdapterLuid() };
    Some((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart))
}

/// Checks that the surface can be opened on the render device.
pub(super) fn validate(ctx: &GpuContext, nt: bool, adapter_luid: u64) -> Result<(), GpuError> {
    if !nt {
        return Err(GpuError::Unsupported("legacy (non-NT) shared handles"));
    }
    if ctx.adapter_luid() != Some(adapter_luid) {
        return Err(GpuError::WrongAdapter);
    }
    Ok(())
}

impl Fences {
    /// Makes the next submission on the render queue wait for the producer's fence.
    pub(super) fn wait(&mut self, ctx: &GpuContext, sync: SurfaceSync) -> Result<(), GpuError> {
        let (handle, value) = match sync {
            SurfaceSync::Ready => return Ok(()),
            SurfaceSync::TimelineFence { handle, value, .. } => (handle, value),
            SurfaceSync::KeyedMutex { .. } => {
                return Err(GpuError::Unsupported(
                    "keyed mutex on D3D12; use a shared fence",
                ));
            }
            _ => return Err(GpuError::Unsupported("sync kind on D3D12")),
        };
        let fence = match self.fences.iter().find(|f| f.handle == handle) {
            Some(f) => f.fence.clone(),
            None => {
                let fence: ID3D12Fence = open_shared(ctx, handle)?;
                self.fences.push(Fence {
                    handle,
                    fence: fence.clone(),
                });
                fence
            }
        };
        // SAFETY: the HAL queue is only used to stage a GPU-side wait that the next wgpu
        // submission executes first; the fence is a live COM object we hold a reference to.
        let queue = unsafe { ctx.queue().as_hal::<Dx12>() }
            .ok_or(GpuError::Unsupported("render queue is not D3D12"))?;
        queue.add_wait_fence(fence, value);
        Ok(())
    }
}

/// Opens an NT shared texture handle on the render device and wraps it without a copy.
pub(super) fn open(ctx: &GpuContext, handle: u64) -> Result<Opened, GpuError> {
    let resource: ID3D12Resource = open_shared(ctx, handle)?;
    // SAFETY: `resource` is a live ID3D12Resource; GetDesc only reads it.
    let desc = unsafe { resource.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.MipLevels != 1
        || desc.DepthOrArraySize != 1
        || desc.SampleDesc.Count != 1
    {
        return Err(GpuError::Unsupported("surface is not a single 2D texture"));
    }
    let format = map_format(desc.Format)?;
    let width = u32::try_from(desc.Width).map_err(|_| GpuError::InvalidSize {
        width: u32::MAX,
        height: desc.Height,
    })?;
    let size = wgpu::Extent3d {
        width,
        height: desc.Height,
        depth_or_array_layers: 1,
    };
    // SAFETY: `resource` was opened on this device (same adapter, checked by LUID), is a
    // single-mip, single-sample 2D texture of `format` and `size` (checked above), and
    // the wrapper keeps its own COM reference.
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource,
            format,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        )
    };
    // SAFETY: the HAL texture belongs to this device and matches the descriptor;
    // a texture shared from another API starts in COMMON, which wgpu calls PRESENT.
    let texture = unsafe {
        ctx.device().create_texture_from_hal::<Dx12>(
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
            wgpu::TextureUses::PRESENT,
        )
    };
    Ok(Opened {
        texture,
        desc: TextureDesc {
            width,
            height: desc.Height,
            format,
            usage: USAGE,
        },
    })
}

fn open_shared<T: windows::core::Interface>(ctx: &GpuContext, handle: u64) -> Result<T, GpuError> {
    // SAFETY: the HAL device is borrowed only to open a shared object; wgpu state is not
    // touched.
    let hal = unsafe { ctx.device().as_hal::<Dx12>() }
        .ok_or(GpuError::Unsupported("render device is not D3D12"))?;
    let mut out: Option<T> = None;
    // SAFETY: `handle` is an NT handle the producer keeps open while its lease is held
    // (VideoFrame::native contract); OpenSharedHandle adds its own reference and does not
    // take ownership of the handle.
    unsafe {
        hal.raw_device()
            .OpenSharedHandle(HANDLE(handle as usize as *mut c_void), &mut out)
    }
    .map_err(|e| GpuError::Import(e.to_string()))?;
    out.ok_or_else(|| GpuError::Import("OpenSharedHandle returned no object".into()))
}

fn map_format(format: DXGI_FORMAT) -> Result<wgpu::TextureFormat, GpuError> {
    Ok(match format {
        DXGI_FORMAT_B8G8R8A8_UNORM => wgpu::TextureFormat::Bgra8Unorm,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => wgpu::TextureFormat::Bgra8UnormSrgb,
        DXGI_FORMAT_R8G8B8A8_UNORM => wgpu::TextureFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => wgpu::TextureFormat::Rgba8UnormSrgb,
        DXGI_FORMAT_R10G10B10A2_UNORM => wgpu::TextureFormat::Rgb10a2Unorm,
        DXGI_FORMAT_R16G16B16A16_FLOAT => wgpu::TextureFormat::Rgba16Float,
        _ => return Err(GpuError::Unsupported("surface format")),
    })
}
