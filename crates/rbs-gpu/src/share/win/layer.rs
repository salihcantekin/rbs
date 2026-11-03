//! UI side on Windows: shows the preview as a DirectComposition visual of the UI's window
//! whose content is a composition swapchain surface fed with the engine's textures
//! (ADR-0035).
//!
//! The engine's shared textures are the presentation buffers themselves, so a new frame
//! costs one `SetBuffer` and one `Present` in the UI process: no redraw by the toolkit, no
//! GPU work and no copy. The desktop compositor composites the buffer with the window,
//! and reports when it is done with a buffer (`IPresentationBuffer::IsAvailable`), which
//! is when the frame goes back to the engine.
//!
//! Toolkit-neutral: the layer needs only the window's `HWND` and a rectangle in it.

use std::cell::Cell;
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::time::Duration;

use rbs_core::time::Timestamp;
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LUID};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Device1, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, DCompositionCreateSurfaceHandle, IDCompositionDevice,
    IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIDevice, IDXGIFactory4,
};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use super::pipe::Owned;
use super::subscriber::{PreviewFrame, PreviewSubscriber};
use crate::error::GpuError;

/// `COMPOSITIONOBJECT_READ | COMPOSITIONOBJECT_WRITE` (dcomptypes.h).
const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

/// How often the UI checks a replaced frame's buffer while one waits: the desktop
/// compositor gives a buffer back at its next composition after the replacing present,
/// within one refresh period.
pub const DEFAULT_RETIRE_POLL: Duration = Duration::from_millis(4);

/// A rectangle in the window's client area in physical pixels, origin at the top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerRect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// The preview as a DirectComposition visual above a window's content.
///
/// Not `Send`: create and use it on the thread that owns the window (the UI thread).
/// The visual is in a topmost composition target, above everything the toolkit draws, so
/// toolkit content drawn over the preview rectangle (an in-window menu, for example) is
/// hidden while the layer shows; the UI hides the layer while such content overlaps it
/// ([`PreviewLayer::set_hidden`]).
pub struct PreviewLayer {
    _d3d: ID3D11Device,
    /// Keep the engine's textures open as long as their buffers exist.
    _textures: Vec<ID3D11Texture2D>,
    manager: IPresentationManager,
    surface: IPresentationSurface,
    buffers: Vec<IPresentationBuffer>,
    _surface_handle: Owned,
    dcomp: IDCompositionDevice,
    visual: IDCompositionVisual,
    target: Option<IDCompositionTarget>,
    width: u32,
    height: u32,
    shown: Option<PreviewFrame>,
    /// Replaced frames whose buffer the compositor may still read. Capacity: the ring size.
    retiring: Vec<PreviewFrame>,
    /// When `retire` last looked at the waiting frames.
    checked: Timestamp,
    poll: Duration,
    placed: Option<LayerRect>,
    hidden: Cell<bool>,
    lost: bool,
}

impl PreviewLayer {
    /// A layer for `subscriber`'s surfaces, not yet in a window. Shows nothing until the
    /// first [`PreviewLayer::show`]. Not for real-time threads.
    ///
    /// `GpuError::Unsupported` when the system or the adapter has no composition
    /// swapchain (for example in a remote desktop session or on a software adapter); the
    /// UI then shows the surfaces through the toolkit
    /// ([`PreviewSubscriber::textures`]).
    pub fn new(subscriber: &PreviewSubscriber) -> Result<Self, GpuError> {
        let fail = |what: &'static str| move |_: windows::core::Error| GpuError::Unsupported(what);
        let luid = subscriber.adapter_luid();
        let luid = LUID {
            LowPart: luid as u32,
            HighPart: (luid >> 32) as i32,
        };
        // SAFETY: plain factory creation and an adapter lookup by LUID.
        let adapter: IDXGIAdapter = unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(fail("CreateDXGIFactory2 failed"))?;
            factory
                .EnumAdapterByLuid(luid)
                .map_err(fail("the engine's adapter is not present"))?
        };
        let mut d3d: Option<ID3D11Device> = None;
        // SAFETY: creates a device on `adapter`; the out pointer is valid for the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )
        }
        .map_err(fail("D3D11CreateDevice failed"))?;
        let d3d = d3d.ok_or(GpuError::Unsupported(
            "D3D11CreateDevice returned no device",
        ))?;
        let d3d1: ID3D11Device1 = d3d.cast().map_err(fail("no ID3D11Device1"))?;

        let mut textures = Vec::with_capacity(subscriber.surfaces());
        for slot in 0..subscriber.surfaces() {
            let handle = subscriber
                .surface_handle(slot)
                .ok_or(GpuError::Unsupported("missing surface handle"))?;
            // SAFETY: `handle` is an NT handle of a shared texture that the subscriber owns
            // and keeps open; opening adds a reference and does not take the handle.
            let tex: ID3D11Texture2D =
                unsafe { d3d1.OpenSharedResource1(HANDLE(handle as usize as *mut c_void)) }
                    .map_err(|e| GpuError::Import(e.to_string()))?;
            textures.push(tex);
        }

        // SAFETY: the factory, manager and buffers are created from our live device and
        // textures; every call only reads its arguments.
        let (manager, buffers) = unsafe {
            let factory: IPresentationFactory =
                CreatePresentationFactory(&d3d).map_err(fail("no composition swapchain"))?;
            if factory.IsPresentationSupported() == 0 {
                return Err(GpuError::Unsupported(
                    "composition swapchain not supported on this adapter",
                ));
            }
            let manager = factory
                .CreatePresentationManager()
                .map_err(fail("CreatePresentationManager failed"))?;
            let buffers = textures
                .iter()
                .map(|t| manager.AddBufferFromResource(t))
                .collect::<windows::core::Result<Vec<_>>>()
                .map_err(fail("AddBufferFromResource failed"))?;
            (manager, buffers)
        };
        // SAFETY: creates a composition surface handle we own (closed on drop) and a
        // presentation surface and composition objects bound to it.
        let (surface_handle, surface, dcomp, visual) = unsafe {
            let handle = Owned(
                DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)
                    .map_err(fail("DCompositionCreateSurfaceHandle failed"))?,
            );
            let surface = manager
                .CreatePresentationSurface(handle.raw())
                .map_err(fail("CreatePresentationSurface failed"))?;
            surface
                .SetAlphaMode(DXGI_ALPHA_MODE_IGNORE)
                .map_err(fail("SetAlphaMode failed"))?;
            surface
                .SetColorSpace(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709)
                .map_err(fail("SetColorSpace failed"))?;
            let dxgi: IDXGIDevice = d3d.cast().map_err(fail("no IDXGIDevice"))?;
            let dcomp: IDCompositionDevice =
                DCompositionCreateDevice(&dxgi).map_err(fail("DCompositionCreateDevice failed"))?;
            let visual = dcomp.CreateVisual().map_err(fail("CreateVisual failed"))?;
            let content = dcomp
                .CreateSurfaceFromHandle(handle.raw())
                .map_err(fail("CreateSurfaceFromHandle failed"))?;
            visual
                .SetContent(&content)
                .map_err(fail("SetContent failed"))?;
            (handle, surface, dcomp, visual)
        };
        let (width, height) = subscriber.size();
        Ok(Self {
            _d3d: d3d,
            _textures: textures,
            manager,
            surface,
            retiring: Vec::with_capacity(buffers.len()),
            buffers,
            _surface_handle: surface_handle,
            dcomp,
            visual,
            target: None,
            width,
            height,
            shown: None,
            checked: Timestamp::ZERO,
            poll: DEFAULT_RETIRE_POLL,
            placed: None,
            hidden: Cell::new(false),
            lost: false,
        })
    }

    /// Surfaces the engine's ring needs so that it always finds a free one while this
    /// layer shows frames `frame_interval` apart: the shown frame, the replaced one until
    /// the compositor gives it back (up to one refresh plus a poll), one being written and
    /// one in flight.
    pub fn surfaces_needed(&self, frame_interval: Duration) -> usize {
        let interval = frame_interval.as_nanos().max(1);
        let retiring = (frame_interval + self.poll).as_nanos().div_ceil(interval);
        usize::try_from(retiring)
            .unwrap_or(usize::MAX)
            .saturating_add(3)
    }

    /// Surfaces in the engine's ring.
    pub fn surfaces(&self) -> usize {
        self.buffers.len()
    }

    /// How often [`PreviewLayer::next_retire`] asks to be called while a frame waits.
    pub fn set_retire_poll(&mut self, poll: Duration) {
        self.poll = poll.max(Duration::from_millis(1));
    }

    /// Adds the layer to a window as a topmost composition target, above the window's
    /// own content (the toolkit's swapchain).
    ///
    /// # Safety
    ///
    /// `hwnd` must be a live top-level window of this process (for example the
    /// `Win32WindowHandle::hwnd` of the toolkit's window), and the call must be made on
    /// the thread that owns it.
    pub unsafe fn attach_to_hwnd(&mut self, hwnd: NonZeroIsize) -> Result<(), GpuError> {
        let hwnd = HWND(hwnd.get() as *mut c_void);
        // SAFETY: the caller guarantees a live window; a window has at most one topmost
        // target, which this creates.
        let target = unsafe { self.dcomp.CreateTargetForHwnd(hwnd, true) }
            .map_err(|_| GpuError::Unsupported("CreateTargetForHwnd failed"))?;
        self.target = Some(target);
        self.placed = None;
        self.apply_root();
        self.commit();
        Ok(())
    }

    /// Takes the layer out of its window and moves the shown frame to the retiring ones,
    /// so every frame goes back once the compositor is done with it
    /// ([`PreviewLayer::retire`]). Call it when the preview panel closes while the window
    /// stays open.
    pub fn detach(&mut self, now: Timestamp) {
        if let Some(target) = self.target.take() {
            // SAFETY: clears the root of our own target.
            let _ = unsafe { target.SetRoot(None::<&IDCompositionVisual>) };
            self.commit();
        }
        self.placed = None;
        if let Some(old) = self.shown.take() {
            self.retiring.push(old);
        }
        self.retire(now);
    }

    /// Places the layer over `rect` of the window's client area (physical pixels; the
    /// toolkit's logical rectangle times its scale factor). The image keeps its aspect
    /// ratio and is centred; letterbox bars show what the toolkit draws there. Does
    /// nothing when the rectangle did not change, so it can be called on every toolkit
    /// redraw.
    pub fn set_rect(&mut self, rect: LayerRect) {
        if self.placed == Some(rect) {
            return;
        }
        self.placed = Some(rect);
        let (bw, bh) = (f64::from(self.width), f64::from(self.height));
        let scale = (rect.width / bw).min(rect.height / bh).max(0.0);
        let x = rect.x + (rect.width - bw * scale) / 2.0;
        let y = rect.y + (rect.height - bh * scale) / 2.0;
        let m = Matrix3x2 {
            M11: scale as f32,
            M12: 0.0,
            M21: 0.0,
            M22: scale as f32,
            M31: 0.0,
            M32: 0.0,
        };
        // SAFETY: plain property changes of our own visual; `m` outlives the call.
        unsafe {
            let _ = self.visual.SetOffsetX2(x.round() as f32);
            let _ = self.visual.SetOffsetY2(y.round() as f32);
            let _ = self.visual.SetTransform2(&m);
        }
        self.commit();
    }

    /// Hides or shows the layer (for content the toolkit draws over the preview).
    pub fn set_hidden(&self, hidden: bool) {
        if self.hidden.get() != hidden {
            self.hidden.set(hidden);
            self.apply_root();
            self.commit();
        }
    }

    /// Whether the presentation manager was lost (device removed, display change): the
    /// layer shows nothing more and the UI recreates it or falls back to the toolkit.
    pub fn lost(&self) -> bool {
        self.lost
    }

    /// Shows `frame` from `now` (any monotonic clock, the same one for every call). The
    /// replaced frame goes back to the engine once the compositor no longer reads it
    /// ([`PreviewLayer::retire`]). Never waits, never allocates.
    pub fn show(&mut self, frame: PreviewFrame, now: Timestamp) {
        let Some(buffer) = self.buffers.get(frame.slot()) else {
            debug_assert!(false, "frame slot {} outside the ring", frame.slot());
            self.retire(now);
            return;
        };
        // SAFETY: `buffer` belongs to this manager; the engine wrote it completely before
        // sending the frame (it waits for its GPU work), so it is ready to present.
        let r = unsafe {
            self.surface
                .SetBuffer(buffer)
                .and_then(|()| self.manager.Present())
        };
        if r.is_err() {
            self.lost = true;
        }
        if let Some(old) = self.shown.replace(frame) {
            // Never reallocates: the engine sends at most one frame per surface.
            self.retiring.push(old);
        }
        self.retire(now);
    }

    /// Gives back replaced frames whose buffer the compositor no longer uses. Returns how
    /// many still wait. Never waits.
    pub fn retire(&mut self, now: Timestamp) -> usize {
        self.checked = now;
        let buffers = &self.buffers;
        let lost = self.lost;
        self.retiring.retain(|f| {
            // A lost manager reads nothing any more.
            !lost
                && buffers.get(f.slot()).is_some_and(|b| {
                    // SAFETY: a plain query of our own buffer.
                    unsafe { b.IsAvailable() }.is_ok_and(|available| available == 0)
                })
        });
        self.retiring.len()
    }

    /// When to call [`PreviewLayer::retire`] again, if a frame waits.
    pub fn next_retire(&self) -> Option<Timestamp> {
        (!self.retiring.is_empty()).then(|| self.checked + self.poll)
    }

    /// Replaced frames still waiting.
    pub fn retiring(&self) -> usize {
        self.retiring.len()
    }

    fn apply_root(&self) {
        if let Some(target) = &self.target {
            // SAFETY: sets or clears the root of our own target to our own visual.
            let _ = unsafe {
                if self.hidden.get() {
                    target.SetRoot(None::<&IDCompositionVisual>)
                } else {
                    target.SetRoot(&self.visual)
                }
            };
        }
    }

    fn commit(&self) {
        // SAFETY: commits our own device's pending changes; does not wait.
        let _ = unsafe { self.dcomp.Commit() };
    }
}

impl Drop for PreviewLayer {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            // SAFETY: clears the root of our own target.
            let _ = unsafe { target.SetRoot(None::<&IDCompositionVisual>) };
            self.commit();
        }
        // The frames drop after this and go back to the engine at once. If the window
        // stays open, the compositor may show the last one until its next composition, so
        // the engine could write a surface still on screen (one torn preview frame at
        // worst); `detach` and waiting for `retiring() == 0` first avoids that.
    }
}
