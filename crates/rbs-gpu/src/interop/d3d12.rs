//! D3D12 import of NT shared handles with shared timeline fences.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.

use std::ffi::c_void;
use std::time::Duration;

use rbs_core::media::{FramePayload, Lease, NativeSurface, SurfaceSync, TextureId, VideoFrame};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};

use super::{MAX_SURFACES, SURFACE_IDLE_TICKS};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};

type Dx12 = wgpu::hal::api::Dx12;

/// Usage of aliased surfaces: sampled by the compositor, copyable for readback/tests.
const USAGE: wgpu::TextureUsages =
    wgpu::TextureUsages::TEXTURE_BINDING.union(wgpu::TextureUsages::COPY_SRC);

struct Surface {
    handle: u64,
    id: TextureId,
    last_seen: u64,
    /// The producer's lease while the surface may still be read.
    native: Option<Lease>,
    /// Submission that hands the surface back (state COMMON); lease released after it.
    handback: Option<wgpu::SubmissionIndex>,
}

struct Fence {
    handle: u64,
    fence: ID3D12Fence,
}

/// Empty until the first import, so an unused importer holds no memory (P13).
#[derive(Default)]
pub(crate) struct D3d12Importer {
    surfaces: Vec<Surface>,
    fences: Vec<Fence>,
    tick: u64,
}

pub(crate) fn adapter_luid(device: &wgpu::Device) -> Option<u64> {
    // SAFETY: the HAL device is only borrowed to read its adapter LUID; no wgpu state is
    // changed and the guard is dropped before returning.
    let hal = unsafe { device.as_hal::<Dx12>() }?;
    // SAFETY: `raw_device` is a live ID3D12Device for the guard's lifetime.
    let luid = unsafe { hal.raw_device().GetAdapterLuid() };
    Some((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart))
}

impl D3d12Importer {
    pub(crate) fn open_surfaces(&self) -> usize {
        self.surfaces.len()
    }

    pub(crate) fn held_leases(&self) -> usize {
        self.surfaces.iter().filter(|s| s.native.is_some()).count()
    }

    pub(crate) fn import(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        frame: VideoFrame,
    ) -> Result<VideoFrame, GpuError> {
        let format = *frame.format();
        let timestamp = frame.timestamp();
        let sequence = frame.sequence();
        let source_time = frame.source_time();
        let FramePayload::Native {
            surface:
                NativeSurface::D3dSharedHandle {
                    handle,
                    nt,
                    adapter_luid,
                    ..
                },
            ..
        } = *frame.payload()
        else {
            return Err(GpuError::Unsupported("not a D3D shared handle"));
        };
        let Some((_, sync, lease)) = frame.into_native() else {
            return Err(GpuError::Unsupported("native frame without lease"));
        };
        if !nt {
            return Err(GpuError::Unsupported("legacy (non-NT) shared handles"));
        }
        if ctx.adapter_luid() != Some(adapter_luid) {
            return Err(GpuError::WrongAdapter);
        }

        // Sync first: an unsupported kind is rejected before anything is opened.
        self.wait(ctx, sync)?;
        let index = match self.surfaces.iter().position(|s| s.handle == handle) {
            Some(i) => i,
            None => self.open_surface(ctx, pool, handle)?,
        };

        let surface = &mut self.surfaces[index];
        surface.last_seen = self.tick;
        surface.handback = None;
        // A producer resends a surface only after its lease was released, so this is
        // normally `None`; replacing drops the stale lease.
        surface.native = Some(lease);
        let mut out = pool
            .frame(surface.id, format, timestamp)?
            .with_sequence(sequence);
        if let Some(t) = source_time {
            out = out.with_source_time(t);
        }
        Ok(out)
    }

    fn wait(&mut self, ctx: &GpuContext, sync: SurfaceSync) -> Result<(), GpuError> {
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

    fn open_surface(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        handle: u64,
    ) -> Result<usize, GpuError> {
        if self.surfaces.len() >= MAX_SURFACES {
            self.evict_one(pool)?;
        }
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
        let id = pool.insert_external(
            ctx,
            texture,
            TextureDesc {
                width,
                height: desc.Height,
                format,
                usage: USAGE,
            },
        )?;
        ctx.metrics().count_surface_opened();
        self.surfaces.push(Surface {
            handle,
            id,
            last_seen: self.tick,
            native: None,
            handback: None,
        });
        Ok(self.surfaces.len() - 1)
    }

    fn evict_one(&mut self, pool: &mut TexturePool) -> Result<(), GpuError> {
        let i = self
            .surfaces
            .iter()
            .enumerate()
            .filter(|(_, s)| s.native.is_none() && pool.lease_count(s.id) == 0)
            .min_by_key(|(_, s)| s.last_seen)
            .map(|(i, _)| i)
            .ok_or(GpuError::Unsupported(
                "more producer surfaces in flight than MAX_SURFACES",
            ))?;
        let s = self.surfaces.swap_remove(i);
        let _ = pool.release(s.id);
        Ok(())
    }

    pub(crate) fn retire(&mut self, ctx: &GpuContext, pool: &mut TexturePool) {
        self.tick += 1;
        if self.surfaces.is_empty() {
            return;
        }

        // 1. Leases whose hand-back submission has completed go back to the producer.
        for s in &mut self.surfaces {
            let done = match &s.handback {
                Some(index) => ctx
                    .device()
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index.clone()),
                        timeout: Some(Duration::ZERO),
                    })
                    .is_ok(),
                None => false,
            };
            if done {
                s.handback = None;
                s.native = None;
            }
        }

        // 2. Surfaces no frame references any more: return them to COMMON after all
        //    earlier work on the queue, then release the lease once that has run.
        let mut encoder: Option<wgpu::CommandEncoder> = None;
        for s in &self.surfaces {
            if s.native.is_none() || s.handback.is_some() || pool.lease_count(s.id) != 0 {
                continue;
            }
            let Some(texture) = pool.get(s.id) else {
                continue;
            };
            let enc = encoder.get_or_insert_with(|| {
                ctx.device()
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("rbs-gpu import hand-back"),
                    })
            });
            enc.transition_resources(
                std::iter::empty(),
                std::iter::once(wgpu::TextureTransition {
                    texture: texture.texture(),
                    selector: None,
                    state: wgpu::TextureUses::PRESENT,
                }),
            );
        }
        if let Some(enc) = encoder {
            let index = ctx.queue().submit(Some(enc.finish()));
            for s in &mut self.surfaces {
                if s.native.is_some() && s.handback.is_none() && pool.lease_count(s.id) == 0 {
                    s.handback = Some(index.clone());
                }
            }
        }

        // 3. Close surfaces the producer stopped sending.
        let tick = self.tick;
        let mut i = 0;
        while i < self.surfaces.len() {
            let s = &self.surfaces[i];
            if s.native.is_none()
                && tick.saturating_sub(s.last_seen) > SURFACE_IDLE_TICKS
                && pool.lease_count(s.id) == 0
            {
                let s = self.surfaces.swap_remove(i);
                let _ = pool.release(s.id);
            } else {
                i += 1;
            }
        }
    }
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
