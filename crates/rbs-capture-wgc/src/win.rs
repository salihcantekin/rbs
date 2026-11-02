//! The Windows backend: target enumeration, D3D11 resources and the WGC session.
//!
//! Threads: `open`, `start`, `stop` and `enumerate` run on a control thread;
//! `latest_frame` on the render thread; `FrameArrived` and `Closed` handlers on OS
//! threads owned by WGC. The render thread only touches the lock-free ring. The
//! producer state is behind a mutex that only the OS callback thread and `stop` take.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_capture::{
    CaptureChange, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, ErrorDomain, GpuDeviceId, SyncKind, TargetId, TargetKind, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{ColorSpace, LeaseOwner, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{DriftEstimator, FrameRate};

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LPARAM, LUID, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext,
    ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIDevice, IDXGIFactory4, IDXGIResource1,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextW, IsWindow, IsWindowVisible,
    WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, IInspectable, Interface, PCWSTR};

use crate::handoff::{FrameMeta, Ring};
use crate::{ADAPTER_ID, Consumer, FRAMES_IN_FLIGHT, RING_SLOTS, luid_to_u64};

/// Buffers of the WGC frame pool (the OS-side queue, separate from our ring).
const POOL_BUFFERS: i32 = 2;

/// Highest rate declared: WGC delivers at most once per display refresh.
const MAX_RATE: i64 = 240;

/// Frames published after a texture was replaced (resize) before its handle is closed,
/// so an importer that caches opened handles by value (gpu-compositor.md, Native import,
/// rule 2: entries unseen for 120 frames are dropped) sees no reused handle value.
const RETIRE_AFTER_FRAMES: u64 = 240;

/// Most replaced textures kept at once. Beyond this the oldest is closed early.
const RETIRED_MAX: usize = 64;

/// `GENERIC_ALL`, the access requested for the shared fence handle.
const GENERIC_ALL: u32 = 0x1000_0000;

const DISPLAY_PREFIX: &str = "display:";
const WINDOW_PREFIX: &str = "window:";

fn backend(e: &windows::core::Error) -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::HResult,
        code: i64::from(e.code().0),
        recoverable: false,
    }
}

/// Initialises the Windows Runtime (multithreaded apartment) on the calling thread once.
/// An existing initialisation of either kind is fine: WinRT calls below only need one.
fn ensure_winrt() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    DONE.with(|done| {
        if !done.get() {
            // SAFETY: plain FFI call with no pointers. It fails harmlessly
            // (RPC_E_CHANGED_MODE) when the thread already joined an STA.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            done.set(true);
        }
    });
}

/// A capture target resolved to its OS handle.
#[derive(Clone, Copy)]
enum Resolved {
    Display(HMONITOR),
    Window(HWND),
}

struct Monitor {
    handle: HMONITOR,
    device: String,
    rect: RECT,
    primary: bool,
}

fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|c| *c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        handle: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<Monitor>` passed to `EnumDisplayMonitors`
        // below, alive and exclusively borrowed for the duration of the call.
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a MONITORINFOEXW with `cbSize` set, as the API requires.
        let ok = unsafe {
            GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO)
        };
        if ok.as_bool() {
            list.push(Monitor {
                handle,
                device: wide_to_string(&info.szDevice),
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        BOOL(1)
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only runs during this call and writes through the pointer to
    // `list`, which outlives it.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut _ as isize),
        )
    };
    list
}

fn rect_size(rect: &RECT) -> (u32, u32) {
    (
        u32::try_from(rect.right - rect.left).unwrap_or(0),
        u32::try_from(rect.bottom - rect.top).unwrap_or(0),
    )
}

/// The capturable size of a window: its visible frame bounds.
fn window_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a RECT and the size passed matches it.
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    ok.ok()
        .map(|()| rect_size(&rect))
        .filter(|(w, h)| *w > 0 && *h > 0)
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a valid writable buffer; the API writes at most its length.
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    let len = usize::try_from(len).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// A window a user would want to capture: visible, titled, not a tool window, not
/// cloaked (hidden UWP or other-desktop windows).
fn is_capturable(hwnd: HWND) -> bool {
    // SAFETY: plain query calls on a window handle; invalid handles return false/0.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if hr.is_ok() && cloaked != 0 {
            return false;
        }
    }
    !window_title(hwnd).is_empty()
}

fn windows_list() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HWND>` passed to `EnumWindows` below, alive
        // and exclusively borrowed for the duration of the call.
        let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        if is_capturable(hwnd) {
            list.push(hwnd);
        }
        BOOL(1)
    }
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: as for `monitors`: the callback only runs during this call.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize)) };
    list
}

fn window_id(hwnd: HWND) -> TargetId {
    TargetId::new(format!("{WINDOW_PREFIX}{:x}", hwnd.0 as usize))
}

/// Resolves a target id to its OS handle and current size, if it still exists.
fn resolve(id: &TargetId) -> Option<(Resolved, (u32, u32))> {
    let id = id.as_str();
    if let Some(device) = id.strip_prefix(DISPLAY_PREFIX) {
        return monitors()
            .into_iter()
            .find(|m| m.device == device)
            .map(|m| (Resolved::Display(m.handle), rect_size(&m.rect)));
    }
    let value = usize::from_str_radix(id.strip_prefix(WINDOW_PREFIX)?, 16).ok()?;
    let hwnd = HWND(value as *mut _);
    // SAFETY: `IsWindow` accepts any value and reports whether it names a window.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    Some((Resolved::Window(hwnd), window_size(hwnd)?))
}

fn luid_struct(bytes: [u8; 8]) -> LUID {
    let value = luid_to_u64(bytes);
    LUID {
        LowPart: value as u32,
        HighPart: (value >> 32) as u32 as i32,
    }
}

fn adapter_for(luid: [u8; 8]) -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain factory calls; the LUID is passed by value.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        factory.EnumAdapterByLuid(luid_struct(luid))
    }
}

/// What `open` accepted from the request.
#[derive(Clone, Copy)]
struct Accepted {
    luid: [u8; 8],
    cursor: bool,
    frame_rate: Option<FrameRate>,
}

/// Checks the zero-copy part of a request (capture-port.md, zero-copy negotiation).
fn accept(request: &CaptureRequest) -> Result<Accepted, CaptureError> {
    if !request.pixel_formats.contains(&PixelFormat::Bgra8)
        || !request.memory.contains(&MemoryKind::D3d11)
    {
        return Err(CaptureError::FormatUnsupported);
    }
    let Some(gpu) = request.gpu.as_ref() else {
        return Err(CaptureError::FormatUnsupported);
    };
    let GpuDeviceId::Luid(luid) = gpu.device else {
        return Err(CaptureError::FormatUnsupported);
    };
    if !gpu.sync.contains(&SyncKind::TimelineFence) || adapter_for(luid).is_err() {
        return Err(CaptureError::FormatUnsupported);
    }
    Ok(Accepted {
        luid,
        cursor: request.cursor,
        frame_rate: request.frame_rate,
    })
}

/// Factory of the Windows Graphics Capture adapter.
#[derive(Debug, Default)]
pub struct WgcCaptureFactory;

impl WgcCaptureFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for WgcCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Windows Graphics Capture"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        ensure_winrt();
        match GraphicsCaptureSession::IsSupported() {
            Ok(true) => {}
            Ok(false) => return Err(ProbeError::new("Windows Graphics Capture is not supported")),
            Err(e) => return Err(ProbeError::new(format!("Windows Graphics Capture: {e}"))),
        }
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        let kinds = ["display", "window"];
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(&kinds))
            .with(caps::TARGET_ENUMERATE, set(&kinds))
            .with(caps::TARGET_PICKER, set(&[]))
            .with(caps::PIXEL_FORMATS, set(&[PixelFormat::Bgra8.as_str()]))
            .with(caps::MEMORY, set(&[MemoryKind::D3d11.as_str()]))
            .with(caps::CURSOR, CapabilityValue::Flag(true))
            .with(caps::FRAME_RATE_MAX, CapabilityValue::Int(MAX_RATE))
            .with(
                caps::FRAMES_IN_FLIGHT,
                CapabilityValue::Int(FRAMES_IN_FLIGHT as i64),
            )
            .with(caps::HDR, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl CaptureFactory for WgcCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        match kind {
            TargetKind::Display => Ok(monitors()
                .into_iter()
                .enumerate()
                .map(|(index, m)| {
                    let (w, h) = rect_size(&m.rect);
                    let name = if m.primary {
                        format!("Display {} (primary)", index + 1)
                    } else {
                        format!("Display {}", index + 1)
                    };
                    CaptureTarget::new(
                        TargetId::new(format!("{DISPLAY_PREFIX}{}", m.device)),
                        TargetKind::Display,
                        name,
                    )
                    .with_size(w, h)
                })
                .collect()),
            TargetKind::Window => Ok(windows_list()
                .into_iter()
                .filter_map(|hwnd| {
                    let (w, h) = window_size(hwnd)?;
                    Some(
                        CaptureTarget::new(window_id(hwnd), TargetKind::Window, window_title(hwnd))
                            .with_size(w, h),
                    )
                })
                .collect()),
            _ => Err(CaptureError::Unsupported("cameras are not captured by WGC")),
        }
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let (_, (w, h)) = resolve(&request.target).ok_or(CaptureError::TargetNotFound)?;
        let accepted = accept(request)?;
        Ok(Box::new(WgcSession {
            target: request.target.clone(),
            accepted,
            clock: Arc::clone(&request.clock),
            format: VideoFormat::new(Size::new(w, h), PixelFormat::Bgra8, ColorSpace::SRGB),
            active: None,
            retired: None,
            next_sequence: 0,
            last_timestamp: None,
            base: CaptureStats::default(),
        }))
    }
}

/// A texture that was replaced after a resize, kept until importers forgot its handle.
struct RetiredTexture {
    _texture: ID3D11Texture2D,
    handle: u64,
    at: u64,
}

/// The D3D objects frames refer to. Owned by [`Shared`], so they live as long as any
/// lease; only the producer (callback thread, or `stop`) locks the mutex.
struct Surfaces {
    textures: Vec<ID3D11Texture2D>,
    handles: Vec<u64>,
    sizes: Vec<(u32, u32)>,
    retired: VecDeque<RetiredTexture>,
    _fence: ID3D11Fence,
    fence_handle: u64,
}

fn close_handle(value: u64) {
    if value != 0 {
        // SAFETY: `value` is an NT handle this adapter created and has not closed.
        let _ = unsafe { CloseHandle(HANDLE(value as usize as *mut _)) };
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            close_handle(handle);
        }
        for retired in self.retired.drain(..) {
            close_handle(retired.handle);
        }
        close_handle(self.fence_handle);
    }
}

/// The ring plus the surfaces it names: the lease owner of every frame.
struct Shared {
    ring: Ring,
    surfaces: Mutex<Surfaces>,
}

impl AsRef<Ring> for Shared {
    fn as_ref(&self) -> &Ring {
        &self.ring
    }
}

// SAFETY: delegates to `Ring`, whose counting is exact (see its impl). The textures and
// handles a slot names are owned by `surfaces` in the same allocation, so they stay
// valid while any lease (which holds an `Arc<Shared>`) exists. Neither method locks.
unsafe impl LeaseOwner for Shared {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

fn create_shared_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
) -> windows::core::Result<(ID3D11Texture2D, u64)> {
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

/// The WinRT wrapper of the capture device, needed to resize the frame pool.
struct WinrtDevice(IDirect3DDevice);

// SAFETY: the object returned by `CreateDirect3D11DeviceFromDXGIDevice` is agile
// (free-threaded) and wraps a D3D11 device with multithread protection on, so it may be
// used from the WGC callback threads. The binding is not marked `Send` only because the
// interface's agility is not in the metadata.
unsafe impl Send for WinrtDevice {}

/// State of the `FrameArrived` handler.
struct Producer {
    shared: Arc<Shared>,
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    winrt_device: WinrtDevice,
    pool_size: SizeInt32,
    fence_value: u64,
    sequence: u64,
    published: u64,
    clock: Arc<dyn Clock>,
    drift: DriftEstimator,
    last_timestamp: Option<Timestamp>,
    qpc_frequency: u64,
    failed: Arc<AtomicI64>,
    stopped: bool,
}

/// Nanoseconds on the QPC clock, the clock `SystemRelativeTime` is based on.
fn qpc_nanos(frequency: u64) -> u64 {
    let mut counter = 0i64;
    // SAFETY: `counter` is a valid out pointer.
    let _ = unsafe { QueryPerformanceCounter(&mut counter) };
    let counter = u128::from(counter.unsigned_abs());
    u64::try_from(counter * 1_000_000_000 / u128::from(frequency.max(1))).unwrap_or(u64::MAX)
}

impl Producer {
    fn on_frame(&mut self, pool: &Direct3D11CaptureFramePool) {
        if self.stopped {
            return;
        }
        if let Err(e) = self.copy_next(pool) {
            self.failed.store(i64::from(e.code().0), Ordering::Release);
        }
    }

    fn copy_next(&mut self, pool: &Direct3D11CaptureFramePool) -> windows::core::Result<()> {
        let Ok(frame) = pool.TryGetNextFrame() else {
            return Ok(());
        };
        let arrived = self.clock.now();
        let qpc = qpc_nanos(self.qpc_frequency);
        let shared = Arc::clone(&self.shared);
        let ring = &shared.ring;
        let content = frame.ContentSize()?;
        let (Ok(width), Ok(height)) = (u32::try_from(content.Width), u32::try_from(content.Height))
        else {
            return Ok(());
        };
        if width == 0 || height == 0 {
            return Ok(());
        }
        if content != self.pool_size {
            // The target was resized: this frame still has the old buffer size. Drop it
            // and resize the OS pool; the next frame has the new size.
            self.pool_size = content;
            pool.Recreate(
                &self.winrt_device.0,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                POOL_BUFFERS,
                content,
            )?;
            ring.count_drop();
            self.sequence += 1;
            return Ok(());
        }
        let Some(slot) = ring.claim() else {
            // The consumer holds every texture: drop rather than wait (invariant 9).
            ring.count_drop();
            self.sequence += 1;
            return Ok(());
        };
        let source = u64::try_from(frame_time(&frame)).unwrap_or(0);
        let result = self.write_slot(slot, &frame, (width, height));
        // Return the OS buffer to the pool now rather than when the handler returns.
        let _ = frame.Close();
        if let Err(e) = result {
            ring.abandon(slot);
            return Err(e);
        }

        // Clock edge (ADR-0004): pair QPC with the master clock now, then convert the
        // frame's QPC-based capture time.
        let _ = self.drift.observe(qpc, arrived);
        let mapped = self
            .drift
            .mapping()
            .map_or(arrived, |m| m.to_master(source));
        let floor = self
            .last_timestamp
            .map_or(Timestamp::ZERO, |t| t + Duration::from_nanos(1));
        let timestamp = mapped.min(arrived).max(floor);
        self.last_timestamp = Some(timestamp);

        ring.publish(
            slot,
            FrameMeta {
                timestamp: timestamp.as_nanos(),
                sequence: self.sequence,
                source_time: source,
                fence_value: self.fence_value,
                width,
                height,
            },
        );
        self.sequence += 1;
        self.published += 1;
        self.close_retired();
        Ok(())
    }

    /// Copies the frame into the slot's texture (replacing it first if the size
    /// changed) and signals the fence.
    fn write_slot(
        &mut self,
        slot: usize,
        frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
        size: (u32, u32),
    ) -> windows::core::Result<()> {
        let mut surfaces = match self.shared.surfaces.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if surfaces.sizes.get(slot) != Some(&size) {
            let (texture, handle) = create_shared_texture(&self.device, size)?;
            let old_texture = std::mem::replace(&mut surfaces.textures[slot], texture);
            let old_handle = std::mem::replace(&mut surfaces.handles[slot], handle);
            surfaces.sizes[slot] = size;
            if surfaces.retired.len() >= RETIRED_MAX {
                if let Some(oldest) = surfaces.retired.pop_front() {
                    close_handle(oldest.handle);
                }
            }
            surfaces.retired.push_back(RetiredTexture {
                _texture: old_texture,
                handle: old_handle,
                at: self.published,
            });
            // The slot is claimed, so no consumer can observe the handle change.
            self.shared.ring.set_handle(slot, handle);
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the WGC surface is a D3D11 texture on our device.
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: size.0,
            bottom: size.1,
            back: 1,
        };
        self.fence_value += 1;
        // SAFETY: both textures are live on this device; the destination slot is
        // claimed (no reader), and the box lies inside both (the pool matches the
        // content size and the slot texture was sized for it). The context is used only
        // by this handler, serialized by the producer mutex.
        unsafe {
            self.context.CopySubresourceRegion(
                &surfaces.textures[slot],
                0,
                0,
                0,
                0,
                &source,
                0,
                Some(&region),
            );
            self.context.Signal(&self.fence, self.fence_value)?;
            self.context.Flush();
        }
        Ok(())
    }

    fn close_retired(&mut self) {
        let mut surfaces = match self.shared.surfaces.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        while surfaces
            .retired
            .front()
            .is_some_and(|r| self.published - r.at > RETIRE_AFTER_FRAMES)
        {
            if let Some(old) = surfaces.retired.pop_front() {
                close_handle(old.handle);
            }
        }
    }
}

/// The frame's capture time in nanoseconds of QPC time.
fn frame_time(frame: &windows::Graphics::Capture::Direct3D11CaptureFrame) -> i64 {
    frame
        .SystemRelativeTime()
        .map_or(0, |t| t.Duration.saturating_mul(100))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct Active {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived_token: i64,
    closed_token: i64,
    producer: Arc<Mutex<Producer>>,
    consumer: Consumer<Shared>,
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
    failed: Arc<AtomicI64>,
}

struct WgcSession {
    target: TargetId,
    accepted: Accepted,
    clock: Arc<dyn Clock>,
    format: VideoFormat,
    active: Option<Active>,
    /// Surfaces of a stopped capture whose frames were still held; freed on drop or the
    /// next start, so the last lease never frees them on the render thread.
    retired: Option<Arc<Shared>>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    /// Counters of earlier Active periods.
    base: CaptureStats,
}

fn min_interval(rate: FrameRate) -> TimeSpan {
    let per = rate.frame_duration();
    TimeSpan {
        Duration: i64::try_from(per.as_nanos() / 100).unwrap_or(i64::MAX),
    }
}

fn create_item(resolved: Resolved) -> windows::core::Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: the handles were checked by `resolve` just before; WGC validates them too.
    unsafe {
        match resolved {
            Resolved::Display(monitor) => interop.CreateForMonitor(monitor),
            Resolved::Window(hwnd) => interop.CreateForWindow(hwnd),
        }
    }
}

fn create_device(luid: [u8; 8]) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let adapter = adapter_for(luid)?;
    let mut device = None;
    let mut context = None;
    // SAFETY: valid out pointers; the adapter is live.
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
        )?;
    }
    let missing = || windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER);
    Ok((device.ok_or_else(missing)?, context.ok_or_else(missing)?))
}

impl WgcSession {
    fn activate(&mut self) -> Result<Active, CaptureError> {
        ensure_winrt();
        let (resolved, _) = resolve(&self.target).ok_or(CaptureError::TargetLost)?;
        let item = create_item(resolved).map_err(|e| backend(&e))?;
        let size = item.Size().map_err(|e| backend(&e))?;
        let (device, context) = create_device(self.accepted.luid).map_err(|e| backend(&e))?;
        let start = (|| -> windows::core::Result<_> {
            // WGC uses the device from its own threads.
            let multithread: ID3D11Multithread = context.cast()?;
            // SAFETY: plain call on a live interface.
            let _ = unsafe { multithread.SetMultithreadProtected(true) };
            let context4: ID3D11DeviceContext4 = context.cast()?;
            let device5: ID3D11Device5 = device.cast()?;
            let mut fence: Option<ID3D11Fence> = None;
            // SAFETY: valid out pointer.
            unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)? };
            let fence = fence.ok_or_else(|| {
                windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
            })?;
            // SAFETY: plain call on a live fence; no name or security attributes.
            let fence_handle =
                unsafe { fence.CreateSharedHandle(None, GENERIC_ALL, PCWSTR::null())? };
            let fence_handle = fence_handle.0 as usize as u64;

            let dims = (
                u32::try_from(size.Width).unwrap_or(1).max(1),
                u32::try_from(size.Height).unwrap_or(1).max(1),
            );
            let mut surfaces = Surfaces {
                textures: Vec::with_capacity(RING_SLOTS),
                handles: Vec::with_capacity(RING_SLOTS),
                sizes: Vec::with_capacity(RING_SLOTS),
                retired: VecDeque::with_capacity(RETIRED_MAX),
                _fence: fence.clone(),
                fence_handle,
            };
            for _ in 0..RING_SLOTS {
                let (texture, handle) = create_shared_texture(&device, dims)?;
                surfaces.textures.push(texture);
                surfaces.handles.push(handle);
                surfaces.sizes.push(dims);
            }
            let shared = Arc::new(Shared {
                ring: Ring::new(&surfaces.handles),
                surfaces: Mutex::new(surfaces),
            });

            let dxgi: IDXGIDevice = device.cast()?;
            // SAFETY: plain call on a live DXGI device.
            let winrt: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
            let winrt_device: IDirect3DDevice = winrt.cast()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                POOL_BUFFERS,
                size,
            )?;
            let session = pool.CreateCaptureSession(&item)?;
            Ok((
                shared,
                fence,
                fence_handle,
                winrt_device,
                pool,
                session,
                context4,
            ))
        })();
        let (shared, fence, fence_handle, winrt_device, pool, session, context4) =
            start.map_err(|e| backend(&e))?;

        let mut frequency = 0i64;
        // SAFETY: valid out pointer.
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        let failed = Arc::new(AtomicI64::new(0));
        let lost = Arc::new(AtomicBool::new(false));
        let producer = Arc::new(Mutex::new(Producer {
            shared: Arc::clone(&shared),
            device,
            context: context4,
            fence,
            winrt_device: WinrtDevice(winrt_device),
            pool_size: size,
            fence_value: 0,
            sequence: self.next_sequence,
            published: 0,
            clock: Arc::clone(&self.clock),
            drift: DriftEstimator::new(Duration::from_millis(100)),
            last_timestamp: self.last_timestamp,
            qpc_frequency: frequency.unsigned_abs(),
            failed: Arc::clone(&failed),
            stopped: false,
        }));

        let handler_producer = Arc::clone(&producer);
        let arrived =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                if let Ok(pool) = pool.ok() {
                    lock(&handler_producer).on_frame(pool);
                }
                Ok(())
            });
        let handler_lost = Arc::clone(&lost);
        let closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            handler_lost.store(true, Ordering::Release);
            Ok(())
        });

        let begin = (|| -> windows::core::Result<(i64, i64)> {
            let arrived_token = pool.FrameArrived(&arrived)?;
            let closed_token = item.Closed(&closed)?;
            session.SetIsCursorCaptureEnabled(self.accepted.cursor)?;
            // Not available on every Windows version; the border is cosmetic.
            let _ = session.SetIsBorderRequired(false);
            if let Some(rate) = self.accepted.frame_rate {
                let _ = session.SetMinUpdateInterval(min_interval(rate));
            }
            session.StartCapture()?;
            Ok((arrived_token, closed_token))
        })();
        let (arrived_token, closed_token) = match begin {
            Ok(tokens) => tokens,
            Err(e) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(backend(&e));
            }
        };

        let luid = luid_to_u64(self.accepted.luid);
        // SAFETY: every handle in the ring is an NT handle of a BGRA8 texture on adapter
        // `luid`, owned by `shared.surfaces`, which lives as long as any lease. A
        // texture replaced after a resize is kept (and its handle open) in `retired`.
        // The producer writes only slots it claimed (count 0, so no frame refers to
        // them) and signals `fence_handle`'s fence after each write. The published
        // size is the size of the texture the slot's handle names.
        let consumer =
            unsafe { Consumer::new(Arc::clone(&shared), self.format, luid, fence_handle) };
        Ok(Active {
            item,
            pool,
            session,
            arrived_token,
            closed_token,
            producer,
            consumer,
            shared,
            lost,
            failed,
        })
    }
}

impl CaptureSession for WgcSession {
    fn format(&self) -> VideoFormat {
        self.active
            .as_ref()
            .map_or(self.format, |a| a.consumer.format())
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Ok(());
        }
        self.retired = None;
        let active = self.activate()?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = active.pool.RemoveFrameArrived(active.arrived_token);
        let _ = active.item.RemoveClosed(active.closed_token);
        let _ = active.session.Close();
        let _ = active.pool.Close();
        {
            // Waits for a handler that is still running, then makes later ones no-ops.
            let mut producer = lock(&active.producer);
            producer.stopped = true;
            self.next_sequence = producer.sequence;
            self.last_timestamp = producer.last_timestamp;
        }
        let ring = &active.shared.ring;
        let waiting = ring.clear();
        self.base.delivered += active.consumer.delivered();
        self.base.dropped += ring.dropped() + u64::from(waiting);
        self.format = active.consumer.format();
        if !ring.is_idle() {
            self.retired = Some(Arc::clone(&active.shared));
        }
    }

    fn latest_frame(&mut self) -> Result<Option<rbs_core::media::VideoFrame>, CaptureError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if active.lost.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        let code = active.failed.load(Ordering::Acquire);
        if code != 0 {
            return Err(CaptureError::Backend {
                domain: ErrorDomain::HResult,
                code,
                recoverable: false,
            });
        }
        Ok(active.consumer.next())
    }

    fn stats(&self) -> CaptureStats {
        let mut stats = self.base;
        if let Some(active) = self.active.as_ref() {
            stats.delivered += active.consumer.delivered();
            stats.dropped += active.shared.ring.dropped();
        }
        stats
    }

    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError> {
        match change {
            CaptureChange::Cursor(on) => {
                self.accepted.cursor = on;
                if let Some(active) = self.active.as_ref() {
                    active
                        .session
                        .SetIsCursorCaptureEnabled(on)
                        .map_err(|e| backend(&e))?;
                }
                Ok(())
            }
            CaptureChange::FrameRate(rate) => {
                self.accepted.frame_rate = Some(rate);
                if let Some(active) = self.active.as_ref() {
                    // Not available on every Windows version: WGC then delivers at the
                    // display rate, which the "upper bound" rule allows.
                    let _ = active.session.SetMinUpdateInterval(min_interval(rate));
                }
                Ok(())
            }
            _ => Err(CaptureError::Unsupported("change not supported")),
        }
    }
}

impl Drop for WgcSession {
    fn drop(&mut self) {
        self.stop();
    }
}
