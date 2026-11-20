//! Copies CEF's accelerated-paint texture into the studio's textures (ADR-0109
//! decision 8, ADR-0132 decision 8). The copy finishes before the picture is
//! published, so the studio samples only its own texture and never waits on CEF.

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_browser_ipc::GpuShare;
use rbs_browser_shm::FrameRing;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device, ID3D11Device1, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory4, IDXGIKeyedMutex};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

/// How long the callback waits for one copy. The same bound as the keyed-mutex
/// timeout in ADR-0132 decision 8.
const COPY_WAIT_MS: u32 = 100;

pub struct Copier {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    slots: [ID3D11Texture2D; 3],
    event: HANDLE,
    value: AtomicU64,
}

// SAFETY: `event` is an owned kernel handle, closed in `Drop`. The paint callback
// is the only user, and it does not run concurrently with `Drop`. The handle is
// never dereferenced as a pointer.
unsafe impl Send for Copier {}
// SAFETY: same as `Send`. The callback and `Drop` do not overlap.
unsafe impl Sync for Copier {}

impl Drop for Copier {
    fn drop(&mut self) {
        // SAFETY: the event came from CreateEventW and has not been closed.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

impl Copier {
    /// Opens the studio's textures. `None` when this process cannot see them: the
    /// caller stays on the shared-memory path.
    pub fn open(share: &GpuShare) -> Option<Self> {
        let adapter = adapter(share.adapter_luid)?;
        let mut device = None;
        let mut immediate = None;
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
                Some(&mut immediate),
            )
            .ok()?;
        }
        let device = device?;
        let immediate = immediate?;
        let multithread: ID3D11Multithread = device.cast().ok()?;
        // SAFETY: plain call on a live device.
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let device1: ID3D11Device1 = device.cast().ok()?;
        let device5: ID3D11Device5 = device.cast().ok()?;
        let context: ID3D11DeviceContext4 = immediate.cast().ok()?;
        let mut fence = None;
        // SAFETY: the handle is the studio's fence, duplicated into this process.
        unsafe {
            device5
                .OpenSharedFence(HANDLE(share.fence as *mut std::ffi::c_void), &mut fence)
                .ok()?;
        }
        let fence = fence?;
        let open = |handle: u64| -> Option<ID3D11Texture2D> {
            // SAFETY: the handle is a studio texture duplicated into this process.
            unsafe {
                device1
                    .OpenSharedResource1(HANDLE(handle as *mut std::ffi::c_void))
                    .ok()
            }
        };
        let slots = [
            open(share.textures[0])?,
            open(share.textures[1])?,
            open(share.textures[2])?,
        ];
        // SAFETY: an unnamed auto-reset event this value closes.
        let event = unsafe { CreateEventW(None, false, false, None).ok()? };
        Some(Self {
            device,
            context,
            fence,
            slots,
            event,
            value: AtomicU64::new(0),
        })
    }

    /// Copies `source` (CEF's handle, valid for this callback) into a free studio
    /// slot and publishes it. Drops the frame when no slot is free or the GPU does
    /// not finish in time.
    pub fn copy(&self, source: *mut std::ffi::c_void, ring: &FrameRing) {
        let Some(slot) = ring.claim_gpu() else {
            return;
        };
        let device1: ID3D11Device1 = match self.device.cast() {
            Ok(device) => device,
            Err(_) => return,
        };
        // SAFETY: CEF passes a shared handle that stays valid for this callback.
        // We do not close it.
        let source: ID3D11Texture2D = match unsafe { device1.OpenSharedResource1(HANDLE(source)) } {
            Ok(texture) => texture,
            Err(_) => return,
        };
        let dest = &self.slots[slot as usize];
        // A shared texture Chromium has not released yet reads as black. Take its
        // key, copy only when the source covers the slot, then hand the key back.
        let mutex: Option<IDXGIKeyedMutex> = source.cast().ok();
        let release = if let Some(mutex) = &mutex {
            // SAFETY: the mutex belongs to `source`, which is live for this call.
            let key = unsafe {
                if mutex.AcquireSync(1, 16).is_ok() {
                    Some(0)
                } else if mutex.AcquireSync(0, 16).is_ok() {
                    Some(1)
                } else {
                    None
                }
            };
            let Some(key) = key else {
                return;
            };
            Some((mutex, key))
        } else {
            None
        };
        let value = self.value.fetch_add(1, Ordering::Relaxed) + 1;
        // SAFETY: both textures are live on this device. The source box is inside
        // the source (checked below) and the fence wait is bounded.
        let copied = unsafe {
            let mut src_desc = std::mem::zeroed();
            let mut dst_desc = std::mem::zeroed();
            source.GetDesc(&mut src_desc);
            dest.GetDesc(&mut dst_desc);
            let fits = src_desc.Width >= dst_desc.Width && src_desc.Height >= dst_desc.Height;
            if fits && src_desc.Width == dst_desc.Width && src_desc.Height == dst_desc.Height {
                self.context.CopyResource(dest, &source);
            } else if fits {
                let box_ = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: dst_desc.Width,
                    bottom: dst_desc.Height,
                    back: 1,
                };
                self.context
                    .CopySubresourceRegion(dest, 0, 0, 0, 0, &source, 0, Some(&box_));
            }
            let signalled = fits && self.context.Signal(&self.fence, value).is_ok();
            self.context.Flush();
            if let Some((mutex, key)) = &release {
                let _ = mutex.ReleaseSync(*key);
            }
            signalled
                && self.fence.SetEventOnCompletion(value, self.event).is_ok()
                && WaitForSingleObject(self.event, COPY_WAIT_MS) == WAIT_OBJECT_0
        };
        if !copied {
            return;
        }
        ring.publish_gpu(slot, value);
    }
}

fn adapter(luid: u64) -> Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> {
    let id = windows::Win32::Foundation::LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as i32,
    };
    // SAFETY: the factory enumerates one adapter.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        factory.EnumAdapterByLuid(id).ok()
    }
}
