//! Three studio-owned `Bgra8` textures a browser helper copies into (ADR-0109
//! decision 8). Windows only. The helper never allocates the texture the render
//! thread samples, so a dead helper cannot fault the studio.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.

use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HMODULE, LUID,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11Device5, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};
use windows::core::{Interface, PCWSTR};

use crate::error::GpuError;

/// Textures and the fence, alive while the page's frames can still be imported.
pub struct PageTextures {
    _device: ID3D11Device,
    _textures: [ID3D11Texture2D; 3],
    _fence: ID3D11Fence,
    slots: [Held; 3],
    fence: Held,
    luid: u64,
    vendor: u32,
    device_id: u32,
}

struct Held {
    handle: HANDLE,
}

// SAFETY: `handle` is an owned NT handle, closed in `Drop`, and is only passed
// to DuplicateHandle and CloseHandle. Those calls do not alias the pointer.
unsafe impl Send for Held {}
// SAFETY: same as `Send`. The handle is not dereferenced.
unsafe impl Sync for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateSharedHandle and has not been closed.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl PageTextures {
    /// Three shared textures and one shared fence on the adapter `luid`.
    pub fn create(luid: u64, width: u32, height: u32) -> Result<Self, GpuError> {
        let (adapter, vendor, device_id) = adapter(luid)?;
        let mut device = None;
        let mut context = None;
        // SAFETY: valid out pointers; the adapter is live for the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|_| GpuError::Unsupported("D3D11 device"))?;
        let device = device.ok_or(GpuError::Unsupported("D3D11 device"))?;
        let multithread: ID3D11Multithread = device
            .cast()
            .map_err(|_| GpuError::Unsupported("D3D11 multithread"))?;
        // SAFETY: plain call on a live device.
        unsafe {
            multithread.SetMultithreadProtected(true);
        }
        let device5: ID3D11Device5 = device
            .cast()
            .map_err(|_| GpuError::Unsupported("D3D11 fence"))?;
        let mut fence = None;
        // SAFETY: a valid out pointer on a live device.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }
            .map_err(|_| GpuError::Unsupported("D3D11 fence"))?;
        let fence = fence.ok_or(GpuError::Unsupported("D3D11 fence"))?;
        let (fence_object, fence) = share_fence(&fence)?;
        let (t0, s0) = share_texture(&device, width, height)?;
        let (t1, s1) = share_texture(&device, width, height)?;
        let (t2, s2) = share_texture(&device, width, height)?;
        Ok(Self {
            _device: device,
            _textures: [t0, t1, t2],
            _fence: fence_object,
            slots: [s0, s1, s2],
            fence,
            luid,
            vendor,
            device_id,
        })
    }

    /// The adapter the textures were created on.
    pub fn luid(&self) -> u64 {
        self.luid
    }

    /// DXGI vendor and device, for the helper's GPU-process command line.
    pub fn vendor_device(&self) -> (u32, u32) {
        (self.vendor, self.device_id)
    }

    /// Parent-process NT handle of slot `slot`.
    pub fn texture(&self, slot: usize) -> u64 {
        self.slots[slot.min(2)].handle.0 as usize as u64
    }

    /// Parent-process NT handle of the fence.
    pub fn fence(&self) -> u64 {
        self.fence.handle.0 as usize as u64
    }

    /// The same objects as handle values valid in process `pid`.
    pub fn duplicate_to(&self, pid: u32) -> Option<([u64; 3], u64)> {
        // SAFETY: opens the child only to duplicate our handles into it.
        let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, pid) }.ok()?;
        let textures = [
            dup(&process, self.slots[0].handle)?,
            dup(&process, self.slots[1].handle)?,
            dup(&process, self.slots[2].handle)?,
        ];
        let fence = dup(&process, self.fence.handle)?;
        // SAFETY: we opened the process and are done with it.
        unsafe {
            let _ = CloseHandle(process);
        }
        Some((textures, fence))
    }
}

fn dup(process: &windows::Win32::Foundation::HANDLE, source: HANDLE) -> Option<u64> {
    let mut target = HANDLE::default();
    // SAFETY: duplicates our live handle into `process` with the same access.
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            *process,
            &mut target,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .ok()?;
    Some(target.0 as usize as u64)
}

fn adapter(luid: u64) -> Result<(IDXGIAdapter, u32, u32), GpuError> {
    let id = LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as i32,
    };
    // SAFETY: the factory enumerates one adapter and reads its description.
    let (adapter, desc) = unsafe {
        let factory: IDXGIFactory4 =
            CreateDXGIFactory1().map_err(|_| GpuError::Unsupported("DXGI factory"))?;
        let adapter: IDXGIAdapter = factory
            .EnumAdapterByLuid(id)
            .map_err(|_| GpuError::WrongAdapter)?;
        let desc = adapter.GetDesc().map_err(|_| GpuError::WrongAdapter)?;
        (adapter, desc)
    };
    Ok((adapter, desc.VendorId, desc.DeviceId))
}

fn share_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, Held), GpuError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let mut texture = None;
    // SAFETY: `desc` is a valid description and `texture` a valid out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|_| GpuError::Unsupported("shared texture"))?;
    let texture = texture.ok_or(GpuError::Unsupported("shared texture"))?;
    let resource: IDXGIResource1 = texture
        .cast()
        .map_err(|_| GpuError::Unsupported("shared texture"))?;
    // SAFETY: creates one NT handle we close on drop.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }
    .map_err(|_| GpuError::Unsupported("shared texture"))?;
    Ok((texture, Held { handle }))
}

fn share_fence(fence: &ID3D11Fence) -> Result<(ID3D11Fence, Held), GpuError> {
    // SAFETY: creates one NT handle we close on drop.
    let handle = unsafe {
        fence.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }
    .map_err(|_| GpuError::Unsupported("shared fence"))?;
    Ok((fence.clone(), Held { handle }))
}
