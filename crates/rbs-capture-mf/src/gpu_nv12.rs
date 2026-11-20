//! NV12 pictures copied from a locked Media Foundation buffer into NT shared textures
//! before the buffer is unlocked. The render thread imports the handle; it does not
//! upload the frame again.

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::Size;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FENCE_FLAG_SHARED, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice,
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use crate::NativeSlot;

/// One session's NV12 textures and the fence signalled after each write.
pub(crate) struct GpuRing {
    /// Kept so the device test can read a picture back. The producer itself uses the context.
    #[allow(dead_code)]
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    slots: Vec<SlotGpu>,
    fence: ID3D11Fence,
    fence_handle: u64,
    luid: u64,
    size: Size,
    value: AtomicU64,
}

struct SlotGpu {
    shared: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    handle: u64,
}

impl GpuRing {
    /// Textures on the adapter `luid`, one per ring slot. `None` when that device
    /// cannot share NV12.
    pub(crate) fn open(luid: u64, size: Size, slots: usize) -> Option<Self> {
        if !size.width.is_multiple_of(2) || !size.height.is_multiple_of(2) || slots == 0 {
            return None;
        }
        // SAFETY: the factory, adapter, device and fence are created here and owned by
        // the returned ring. Shared handles stay open until that ring is dropped.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            let adapter: IDXGIAdapter = factory
                .EnumAdapterByLuid(LUID {
                    LowPart: luid as u32,
                    HighPart: (luid >> 32) as i32,
                })
                .ok()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .ok()?;
            let device = device?;
            let context = context?;
            // The device is created on the session thread and written on the reader
            // callback. The immediate context is only safe across those threads once
            // multithreaded protection is on.
            let multithread: ID3D11Multithread = context.cast().ok()?;
            let _ = multithread.SetMultithreadProtected(true);
            let mut slots_gpu = Vec::with_capacity(slots);
            for _ in 0..slots {
                slots_gpu.push(shared_nv12(&device, size)?);
            }
            let mut fence: Option<ID3D11Fence> = None;
            device
                .cast::<ID3D11Device5>()
                .ok()?
                .CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
                .ok()?;
            let fence = fence?;
            // The fence was created with the shared flag on this device.
            let fence_handle = fence
                .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                .ok()?;
            Some(Self {
                device,
                context,
                slots: slots_gpu,
                fence,
                fence_handle: fence_handle.0 as usize as u64,
                luid,
                size,
                value: AtomicU64::new(0),
            })
        }
    }

    /// Copies a packed NV12 picture into `slot` and signals the fence. `src` is the
    /// first luma byte; `pitch` is the row stride in bytes; the chroma plane starts
    /// `pitch * height` bytes later. The call copies before it returns, so the caller
    /// may unlock the Media Foundation buffer immediately after.
    pub(crate) fn write(
        &self,
        slot: usize,
        src: *const u8,
        pitch: u32,
        height: u32,
    ) -> Option<NativeSlot> {
        let slot = self.slots.get(slot)?;
        if src.is_null() || pitch == 0 || height == 0 || height != self.size.height {
            return None;
        }
        let width = self.size.width;
        if pitch < width {
            return None;
        }
        // SAFETY: `src` covers `pitch` bytes per row for `height` luma rows and
        // `height / 2` chroma rows, and stays readable until this returns. The staging
        // texture and the shared texture are this slot's. The context is protected and
        // used only while the producer lock is held. The mapped pointer is valid until
        // `Unmap`, which runs before this returns.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&slot.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .ok()?;
            let copied = copy_nv12(&mapped, src, pitch, width, height);
            self.context.Unmap(&slot.staging, 0);
            if !copied {
                return None;
            }
            self.context
                .CopySubresourceRegion(&slot.shared, 0, 0, 0, 0, &slot.staging, 0, None);
            let value = self.value.fetch_add(1, Ordering::AcqRel) + 1;
            self.context
                .cast::<ID3D11DeviceContext4>()
                .ok()?
                .Signal(&self.fence, value)
                .ok()?;
            self.context.Flush();
            Some(NativeSlot {
                texture: slot.handle,
                fence: self.fence_handle,
                value,
                luid: self.luid,
            })
        }
    }
}

impl Drop for GpuRing {
    fn drop(&mut self) {
        for slot in &self.slots {
            close_nt(slot.handle);
        }
        close_nt(self.fence_handle);
        self.fence_handle = 0;
    }
}

fn close_nt(value: u64) {
    if value != 0 {
        // SAFETY: `value` is an NT handle this ring created and has not closed.
        let _ = unsafe { CloseHandle(HANDLE(value as usize as *mut _)) };
    }
}

/// Copies luma then chroma into a mapped NV12 staging texture. DXGI places the UV
/// plane immediately after `RowPitch * height` luma bytes, with the same row pitch.
///
/// # Safety
///
/// `mapped.pData` is writable for the mapped NV12 layout. `src` is readable for
/// `pitch * height * 3 / 2` bytes.
unsafe fn copy_nv12(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    src: *const u8,
    pitch: u32,
    width: u32,
    height: u32,
) -> bool {
    let dst_pitch = mapped.RowPitch as usize;
    let src_pitch = pitch as usize;
    let row = width as usize;
    let rows = height as usize;
    if mapped.pData.is_null() || dst_pitch < row || src_pitch < row {
        return false;
    }
    let chroma = rows / 2;
    let dst_bytes = dst_pitch.checked_mul(rows.saturating_add(chroma));
    let src_bytes = src_pitch.checked_mul(rows.saturating_add(chroma));
    if dst_bytes.is_none() || src_bytes.is_none() {
        return false;
    }
    // SAFETY: the function contract covers both planes, and the checks above keep
    // every row inside that span.
    unsafe {
        let dst = mapped.pData.cast::<u8>();
        for y in 0..rows {
            std::ptr::copy_nonoverlapping(src.add(y * src_pitch), dst.add(y * dst_pitch), row);
        }
        let src_uv = src.add(rows * src_pitch);
        let dst_uv = dst.add(rows * dst_pitch);
        for y in 0..chroma {
            std::ptr::copy_nonoverlapping(
                src_uv.add(y * src_pitch),
                dst_uv.add(y * dst_pitch),
                row,
            );
        }
    }
    true
}

fn shared_nv12(device: &ID3D11Device, size: Size) -> Option<SlotGpu> {
    let sample = DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
    };
    let shared_desc = D3D11_TEXTURE2D_DESC {
        Width: size.width,
        Height: size.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: sample,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_SHARED.0)
            as u32,
    };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        ..shared_desc
    };
    // SAFETY: both descriptions are valid 2D textures and the out slots are writable.
    let (shared, staging) = unsafe {
        (
            create_texture(device, &shared_desc)?,
            create_texture(device, &staging_desc)?,
        )
    };
    // SAFETY: the shared texture was created with the NT-handle flags, so it
    // implements `IDXGIResource1`. The handle is owned by the ring and closed on drop.
    let handle = unsafe {
        shared
            .cast::<IDXGIResource1>()
            .ok()?
            .CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )
            .ok()?
    };
    Some(SlotGpu {
        shared,
        staging,
        handle: handle.0 as usize as u64,
    })
}

/// # Safety
///
/// `desc` is a valid texture description.
unsafe fn create_texture(
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Option<ID3D11Texture2D> {
    let mut texture = None;
    // SAFETY: `texture` is a writable out pointer for this call.
    unsafe {
        device
            .CreateTexture2D(desc, None, Some(&mut texture))
            .ok()?;
    }
    texture
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "slow: needs a D3D11 adapter that can share NV12"]
    fn slow_nv12_ring_accepts_a_packed_picture() {
        let Some(luid) = default_luid() else {
            panic!("adapter");
        };
        let size = Size::new(16, 16);
        let Some(ring) = GpuRing::open(luid, size, 1) else {
            panic!("shared NV12");
        };
        let mut picture = vec![16u8; 16 * 24];
        for byte in picture[256..].iter_mut() {
            *byte = 128;
        }
        let Some(slot) = ring.write(0, picture.as_ptr(), 16, 16) else {
            panic!("upload");
        };
        assert_ne!(slot.texture, 0);
        assert_ne!(slot.fence, 0);
        assert_eq!(slot.value, 1);
        assert_eq!(slot.luid, luid);
        let (y, uv) = read_back(&ring).unwrap_or_else(|| panic!("readback"));
        assert_eq!((y, uv), (16, 128));
    }

    /// First luma byte and first chroma byte of the texture the upload just wrote.
    fn read_back(ring: &GpuRing) -> Option<(u8, u8)> {
        let shared = &ring.slots.first()?.shared;
        // SAFETY: the shared texture is live. The staging texture is local. The map is
        // released before this returns, and the context is the one that wrote the picture.
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            shared.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags =
                windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let staging = create_texture(&ring.device, &desc)?;
            ring.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, shared, 0, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ring.context
                .Map(
                    &staging,
                    0,
                    windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )
                .ok()?;
            let pitch = mapped.RowPitch as usize;
            let ptr = mapped.pData.cast::<u8>();
            let y = *ptr;
            let uv = *ptr.add(pitch * desc.Height as usize);
            ring.context.Unmap(&staging, 0);
            Some((y, uv))
        }
    }

    fn default_luid() -> Option<u64> {
        // SAFETY: the factory is local and released at the end of this call.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            let adapter: IDXGIAdapter = factory.EnumAdapters(0).ok()?;
            let desc = adapter.GetDesc().ok()?;
            Some((desc.AdapterLuid.LowPart as u64) | ((desc.AdapterLuid.HighPart as u64) << 32))
        }
    }
}
