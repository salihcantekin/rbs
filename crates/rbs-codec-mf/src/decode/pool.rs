//! Native pictures: the MFT decodes with DXVA on a D3D11 device of the render adapter,
//! and each picture is copied on the GPU into an adapter-owned pool of shared NV12
//! textures (backend-ports.md, live video decode invariants 8 and 9).
//!
//! - The decoder's own texture array is its DPB and cannot be shared, so each picture
//!   is copied (cropped to the display size) into a slot texture created with an NT
//!   handle. The worker waits on a D3D11 fence for the copy, so the picture is handed
//!   out `SurfaceSync::Ready`.
//! - Slots are counted by `rbs_capture::handoff::Ring`, as in `rbs-media-mf`: the
//!   worker claims a free slot (one reference) and hands that reference to the frame's
//!   lease. Only a free slot is written; with every slot leased the picture is dropped.
//!   Neither `retain` nor `release` locks or frees.
//! - A slot texture replaced for another size, and the pool of a dropped decoder, go to
//!   a graveyard: kept while a lease refers to them, then for [`HANDLE_GRACE`], so a
//!   handle value an importer may have cached is not reused; freed on the janitor
//!   thread, never on the render thread. The janitor starts on the first burial.
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rbs_capture::handoff::{Grave, Graveyard, Ring};
use rbs_codec::VideoDecodeError;
use rbs_core::media::{
    D3dApi, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync, VideoFormat,
    VideoFrame,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_FENCE_FLAG_NONE, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, MFCreateDXGIDeviceManager,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCWSTR};

use super::Placement;
use super::mft::failed;

/// How long a handed-out texture handle stays open after its last use (the file
/// port's native surface rules: at least 10 s).
const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks stopped decoders.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// How long the worker waits for one GPU copy before it gives up on the picture.
const COPY_WAIT_MS: u32 = 2_000;

static GRAVEYARD: Graveyard = Graveyard::new("rbs-codec-mf-janitor", HANDLE_GRACE, JANITOR_PERIOD);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn device_error(what: &str, e: &windows::core::Error) -> VideoDecodeError {
    VideoDecodeError::Failed {
        reason: format!("{what}: {e}"),
        recoverable: false,
    }
}

/// The adapter of `luid` (the `GpuContext::adapter_luid` encoding: high part in the
/// upper 32 bits).
fn adapter_for(luid: u64) -> windows::core::Result<IDXGIAdapter> {
    let luid = LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as u32 as i32,
    };
    // SAFETY: plain factory calls; the LUID is passed by value.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        factory.EnumAdapterByLuid(luid)
    }
}

/// One slot's texture and its NT handle. Closes the handle when dropped.
struct SlotTexture {
    texture: ID3D11Texture2D,
    handle: u64,
    size: Size,
}

impl Drop for SlotTexture {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: an NT handle this adapter created and has not closed.
            let _ = unsafe { CloseHandle(HANDLE(self.handle as usize as *mut _)) };
        }
    }
}

/// A slot texture moved between the worker and the janitor.
///
/// The texture belongs to a D3D11 device created with multithread protection, so it
/// may be released on any thread, and the adapter never uses it from two threads at
/// once (the worker only while the slot is claimed, the janitor only after the last
/// lease is gone). The NT handle is a plain kernel handle, closable on any thread.
struct Sendable(SlotTexture);

// SAFETY: see the type's doc.
unsafe impl Send for Sendable {}

/// The slots and the textures they name. Frames hold an `Arc` of it in their lease.
struct Pool {
    ring: Ring,
    /// One entry per slot. Only the worker (and the janitor, after the last lease)
    /// locks it, never a lease.
    textures: Mutex<Vec<Option<Sendable>>>,
}

// SAFETY: delegates to `Ring`, whose counts are exact: the worker claims a slot (one
// reference) and hands exactly that reference to one `Lease`; every clone retains and
// every drop releases once. A slot's texture and handle stay in `textures` while the
// slot is referenced: the worker writes or replaces a texture only in a slot it has
// claimed, and the pool lives as long as any lease (which holds an `Arc<Pool>`).
// Neither method locks.
unsafe impl LeaseOwner for Pool {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// What [`Gpu::picture`] made of a decoded sample.
pub(crate) enum GpuPicture {
    /// A native picture in a pool slot.
    Native(VideoFrame),
    /// Every slot is leased: the picture was dropped.
    NoSlot,
    /// The sample is not an NV12 texture of this device: copy it to system memory.
    NotGpu,
}

/// The decode device, the device manager the MFT uses, and the pool.
pub(crate) struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    manager: IMFDXGIDeviceManager,
    fence: ID3D11Fence,
    fence_value: u64,
    event: HANDLE,
    luid: u64,
    slots: usize,
    pool: Arc<Pool>,
    owner: Arc<dyn LeaseOwner>,
}

impl Gpu {
    /// A decode device with `slots` pool slots on the adapter of `luid`, or an error
    /// when the adapter is gone or lacks D3D11 video support or fences (the caller
    /// decodes to system memory instead). Slot textures are made on first use.
    pub(crate) fn new(luid: u64, slots: usize) -> Result<Self, VideoDecodeError> {
        let adapter = adapter_for(luid).map_err(|e| device_error("render adapter", &e))?;
        let mut device = None;
        let mut immediate = None;
        // SAFETY: valid out pointers; the adapter is live.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate),
            )
        }
        .map_err(|e| device_error("D3D11 device", &e))?;
        let missing = || device_error("D3D11", &windows::core::Error::empty());
        let device: ID3D11Device = device.ok_or_else(missing)?;
        let context: ID3D11DeviceContext4 = immediate
            .ok_or_else(missing)?
            .cast()
            .map_err(|e| device_error("D3D11 fences", &e))?;
        // The MFT uses the device on its own threads.
        let multithread: ID3D11Multithread = device
            .cast()
            .map_err(|e| device_error("D3D11 multithread", &e))?;
        // SAFETY: plain call on a live device; returns the previous setting.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        let device5: ID3D11Device5 = device
            .cast()
            .map_err(|e| device_error("D3D11 fences", &e))?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: a valid out pointer on a live device.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }
            .map_err(|e| device_error("D3D11 fence", &e))?;
        let fence = fence.ok_or_else(missing)?;
        let mut token = 0u32;
        let mut manager = None;
        // SAFETY: valid out pointers.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| device_error("DXGI device manager", &e))?;
        let manager: IMFDXGIDeviceManager = manager.ok_or_else(missing)?;
        // SAFETY: the token is the one the manager just gave; it takes a reference.
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|e| device_error("DXGI device manager", &e))?;
        // SAFETY: an unnamed auto-reset event; closed in `Drop`.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| device_error("event", &e))?;
        let pool = Arc::new(Pool {
            ring: Ring::new(&vec![0; slots]),
            textures: Mutex::new((0..slots).map(|_| None).collect()),
        });
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&pool) as Arc<dyn LeaseOwner>;
        Ok(Self {
            device,
            context,
            manager,
            fence,
            fence_value: 0,
            event,
            luid,
            slots,
            pool,
            owner,
        })
    }

    /// The manager to hand the MFT (`MFT_MESSAGE_SET_D3D_MANAGER`).
    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// Pool size.
    pub(crate) fn slots(&self) -> usize {
        self.slots
    }

    /// Copies the cropped picture `at` of `sample` into a free slot.
    pub(crate) fn picture(
        &mut self,
        sample: &IMFSample,
        at: Placement,
    ) -> Result<GpuPicture, VideoDecodeError> {
        let size = at.size;
        // SAFETY: plain calls on a live sample and its buffer; `GetResource` writes an
        // owned reference of the requested interface, which `from_raw` takes over.
        let (source, subresource) = unsafe {
            let Ok(buffer) = sample.GetBufferByIndex(0) else {
                return Ok(GpuPicture::NotGpu);
            };
            let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else {
                return Ok(GpuPicture::NotGpu);
            };
            let mut raw = std::ptr::null_mut();
            if dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).is_err() || raw.is_null() {
                return Ok(GpuPicture::NotGpu);
            }
            let texture = ID3D11Texture2D::from_raw(raw);
            let Ok(index) = dxgi.GetSubresourceIndex() else {
                return Ok(GpuPicture::NotGpu);
            };
            (texture, index)
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a valid out pointer on a live texture.
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 || !at.fits(desc.Width, desc.Height) {
            return Ok(GpuPicture::NotGpu);
        }
        let Some(slot) = self.pool.ring.claim() else {
            return Ok(GpuPicture::NoSlot);
        };
        let handle = match self.write_slot(slot, &source, subresource, at) {
            Ok(handle) => handle,
            Err(e) => {
                self.pool.ring.abandon(slot);
                return Err(e);
            }
        };
        // SAFETY: `claim` counted exactly one reference to `slot`, handed over here.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let format = VideoFormat::new(size, PixelFormat::Nv12, at.color);
        let surface = NativeSurface::d3d_shared_handle(handle, true, D3dApi::D3d11, self.luid);
        // SAFETY: the handle names the slot's NV12 texture of `size` on the adapter of
        // `luid`, which the pool keeps (with the handle open) while the slot is
        // referenced; the worker writes it only after every reference is released, and
        // a replaced or dropped texture waits out the grace in the graveyard. The copy
        // has completed (fence waited).
        let frame =
            unsafe { VideoFrame::native(format, at.pts, surface, SurfaceSync::Ready, lease) };
        Ok(GpuPicture::Native(frame))
    }

    /// Writes the cropped picture into the claimed `slot`, replacing its texture first
    /// when the size changed, and waits for the GPU. Returns the slot's handle.
    fn write_slot(
        &mut self,
        slot: usize,
        source: &ID3D11Texture2D,
        subresource: u32,
        at: Placement,
    ) -> Result<u64, VideoDecodeError> {
        let size = at.size;
        let mut textures = lock(&self.pool.textures);
        let entry = textures
            .get_mut(slot)
            .ok_or_else(|| device_error("slot", &windows::core::Error::empty()))?;
        if entry.as_ref().is_none_or(|t| t.0.size != size) {
            let (texture, handle) =
                create_shared_nv12(&self.device, size).map_err(|e| failed("texture", &e))?;
            if let Some(old) = entry.replace(Sendable(SlotTexture {
                texture,
                handle,
                size,
            })) {
                // An importer may have cached the old handle: keep it open for the
                // grace so its value is not reused.
                GRAVEYARD.bury(Box::new(TextureGrave(old)));
            }
        }
        let Some(target) = entry.as_ref() else {
            return Err(device_error("slot", &windows::core::Error::empty()));
        };
        let region = D3D11_BOX {
            left: at.left,
            top: at.top,
            front: 0,
            right: at.left + size.width,
            bottom: at.top + size.height,
            back: 1,
        };
        // SAFETY: both textures are NV12 and live on this device; the destination slot
        // is claimed (no reader) and sized to the picture, and the even-aligned box lies
        // inside the decoded texture (`Placement::fits`, checked by the caller). The device is
        // multithread protected, so the immediate context is serialized with the MFT.
        unsafe {
            self.context.CopySubresourceRegion(
                &target.0.texture,
                0,
                0,
                0,
                0,
                source,
                subresource,
                Some(&region),
            );
        }
        let handle = target.0.handle;
        drop(textures);
        self.wait_for_gpu()?;
        Ok(handle)
    }

    /// Signals the fence after the work queued so far and waits for it.
    fn wait_for_gpu(&mut self) -> Result<(), VideoDecodeError> {
        self.fence_value += 1;
        // SAFETY: plain calls on the live context, fence and event; the device is
        // multithread protected.
        unsafe {
            self.context
                .Signal(&self.fence, self.fence_value)
                .map_err(|e| failed("fence", &e))?;
            self.context.Flush();
            self.fence
                .SetEventOnCompletion(self.fence_value, self.event)
                .map_err(|e| failed("fence", &e))?;
            if WaitForSingleObject(self.event, COPY_WAIT_MS) != WAIT_OBJECT_0 {
                return Err(VideoDecodeError::DeviceLost);
            }
        }
        Ok(())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // Only a pool some picture used needs the grace (a never-used one has no
        // textures and no handed-out handle).
        let used = lock(&self.pool.textures).iter().any(Option::is_some);
        if used {
            GRAVEYARD.bury(Box::new(PoolGrave(Arc::clone(&self.pool))));
        }
        // SAFETY: the event was created in `new` and is closed once, here.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

fn create_shared_nv12(
    device: &ID3D11Device,
    size: Size,
) -> windows::core::Result<(ID3D11Texture2D, u64)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width.max(2),
        Height: size.height.max(2),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let mut texture = None;
    // SAFETY: `desc` is a valid description and `texture` a valid out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    let texture = texture
        .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
    let resource: IDXGIResource1 = texture.cast()?;
    // SAFETY: plain call on a live resource; no name or security attributes.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )?
    };
    Ok((texture, handle.0 as usize as u64))
}

/// A slot texture replaced for another size. Its slot was claimed by the worker, so
/// nobody references it; it is kept a full grace from burial.
struct TextureGrave(Sendable);

impl Grave for TextureGrave {
    fn is_idle(&self) -> bool {
        true
    }
    fn bytes(&self) -> u64 {
        let s = self.0.0.size;
        u64::from(s.width) * u64::from(s.height) * 3 / 2
    }
}

/// The pool of a dropped decoder. Kept while pictures reference it (so the last lease
/// never frees anything on the render thread) and for the grace after that.
struct PoolGrave(Arc<Pool>);

impl Grave for PoolGrave {
    fn is_idle(&self) -> bool {
        self.0.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        lock(&self.0.textures)
            .iter()
            .flatten()
            .map(|t| u64::from(t.0.size.width) * u64::from(t.0.size.height) * 3 / 2)
            .sum()
    }
}
