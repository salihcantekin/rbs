//! UI side on Windows: shows the preview as a DirectComposition visual of the UI's window
//! whose content is a composition swapchain surface fed with the engine's textures
//! (ADR-0036).
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
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LUID, POINT, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Device1, ID3D11DeviceContext1, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, DCompositionCreateDevice,
    DCompositionCreateSurfaceHandle, IDCompositionDevice, IDCompositionSurface,
    IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIDevice, IDXGIFactory4,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use super::super::geometry::{MAX_MARKS, Mark, snap};
use super::super::region::{self, Edges, MAX_CUTOUTS, MAX_MARK_PIECES};
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
/// hidden while the layer shows; the UI passes such content's rectangles to
/// [`PreviewLayer::set_cutouts`]. Marks are cut around them; the picture hides while
/// one overlaps it, until sharing the surface between clipped visuals is checked on
/// hardware (ADR-0054 decision 5).
pub struct PreviewLayer {
    _d3d: ID3D11Device,
    /// Keep the engine's textures open as long as their buffers exist.
    _textures: Vec<ID3D11Texture2D>,
    manager: IPresentationManager,
    surface: IPresentationSurface,
    buffers: Vec<IPresentationBuffer>,
    _surface_handle: Owned,
    dcomp: IDCompositionDevice,
    /// The target's root: the picture, and above it the marks (ADR-0048).
    root: IDCompositionVisual,
    visual: IDCompositionVisual,
    target: Option<IDCompositionTarget>,
    /// Parent of the mark visuals, above the picture; made at the first `set_marks`.
    overlay: Option<IDCompositionVisual>,
    /// The mark visuals, reused; at most [`MAX_MARKS`].
    mark_visuals: Vec<IDCompositionVisual>,
    /// One 1 x 1 surface per colour the marks use, stretched by each visual.
    mark_colors: Vec<([u8; 4], IDCompositionSurface)>,
    width: u32,
    height: u32,
    shown: Option<PreviewFrame>,
    /// Replaced frames whose buffer the compositor may still read. Capacity: the ring size.
    retiring: Vec<PreviewFrame>,
    /// The frame shown when the layer was detached. Its buffer stays the surface's
    /// current one (it never reports available, and the engine must not reuse it while it
    /// could be presented again), so it is held until a later `show` replaces it or the
    /// layer drops.
    detached: Option<PreviewFrame>,
    /// Attached, but no frame presented since: the visual joins the window only at the
    /// next `show`, so a stale buffer is never shown.
    awaiting_frame: bool,
    /// Signalled by the system when the presentation manager is lost; owned by it.
    lost_event: HANDLE,
    /// When `retire` last looked at the waiting frames.
    checked: Timestamp,
    poll: Duration,
    placed: Option<LayerRect>,
    hidden: Cell<bool>,
    /// The covers last asked for (physical pixels).
    cutouts: [Option<Edges>; MAX_CUTOUTS],
    /// A cover was not finite or there were too many: hidden.
    cut_bad: bool,
    /// A cover overlaps the picture: hidden (ADR-0054 decision 5, Windows interim).
    cut_over: bool,
    /// The marks last asked for, drawn again when the covers change.
    wanted: [Option<Mark>; MAX_MARKS],
    /// Scratch for mark pieces; capacity kept.
    pieces: Vec<Edges>,
    mark_pieces: Vec<(Edges, [u8; 4])>,
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
        // Over remote desktop the presentation factory can report support while the
        // session never shows the buffers (the area stays black), so the toolkit path
        // is used there.
        // SAFETY: reads a system metric; no arguments to uphold.
        if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
            return Err(GpuError::Unsupported(
                "composition swapchain not shown in a remote desktop session",
            ));
        }
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
        // SAFETY: creates a composition surface handle we own (closed on drop), a
        // presentation surface and composition objects bound to it, and a root visual of
        // our own device holding the picture visual.
        let (surface_handle, surface, dcomp, root, visual) = unsafe {
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
            // The source rectangle starts empty, and an empty one shows nothing: the
            // whole buffer is the picture.
            let (width, height) = subscriber.size();
            let source = RECT {
                left: 0,
                top: 0,
                right: i32::try_from(width)
                    .map_err(|_| GpuError::Unsupported("surface too wide"))?,
                bottom: i32::try_from(height)
                    .map_err(|_| GpuError::Unsupported("surface too tall"))?,
            };
            surface
                .SetSourceRect(&source)
                .map_err(fail("SetSourceRect failed"))?;
            let dxgi: IDXGIDevice = d3d.cast().map_err(fail("no IDXGIDevice"))?;
            let dcomp: IDCompositionDevice =
                DCompositionCreateDevice(&dxgi).map_err(fail("DCompositionCreateDevice failed"))?;
            let visual = dcomp.CreateVisual().map_err(fail("CreateVisual failed"))?;
            let root = dcomp.CreateVisual().map_err(fail("CreateVisual failed"))?;
            root.AddVisual(&visual, true, None::<&IDCompositionVisual>)
                .map_err(fail("AddVisual failed"))?;
            let content = dcomp
                .CreateSurfaceFromHandle(handle.raw())
                .map_err(fail("CreateSurfaceFromHandle failed"))?;
            visual
                .SetContent(&content)
                .map_err(fail("SetContent failed"))?;
            (handle, surface, dcomp, root, visual)
        };
        // SAFETY: a query of our own manager; the event stays owned by it.
        let lost_event = unsafe { manager.GetLostEvent() }.map_err(fail("GetLostEvent failed"))?;
        let (width, height) = subscriber.size();
        Ok(Self {
            _d3d: d3d,
            _textures: textures,
            manager,
            surface,
            retiring: Vec::with_capacity(buffers.len()),
            detached: None,
            awaiting_frame: false,
            lost_event,
            buffers,
            _surface_handle: surface_handle,
            dcomp,
            root,
            visual,
            target: None,
            overlay: None,
            mark_visuals: Vec::new(),
            mark_colors: Vec::new(),
            width,
            height,
            shown: None,
            checked: Timestamp::ZERO,
            poll: DEFAULT_RETIRE_POLL,
            placed: None,
            hidden: Cell::new(false),
            cutouts: [None; MAX_CUTOUTS],
            cut_bad: false,
            cut_over: false,
            wanted: [None; MAX_MARKS],
            pieces: Vec::with_capacity(region::MAX_PIECES),
            mark_pieces: Vec::with_capacity(MAX_MARK_PIECES),
            lost: false,
        })
    }

    /// Surfaces the engine's ring needs so that it always finds a free one while this
    /// layer shows frames `frame_interval` apart: the shown frame, the replaced one until
    /// the compositor gives it back (up to one refresh plus a poll), one being written and
    /// one in flight.
    pub fn surfaces_needed(&self, frame_interval: Duration) -> usize {
        crate::share::geometry::ring_needed(frame_interval, frame_interval + self.poll)
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
        // The visual joins the window at the next `show`, never with an old buffer.
        self.awaiting_frame = true;
        Ok(())
    }

    /// Takes the layer out of its window and waits until the compositor has applied
    /// that. Replaced frames go back as usual ([`PreviewLayer::retire`]); the shown one
    /// stays the surface's current buffer, so it is held (one surface) until a later
    /// `show` after a new attach replaces it, or the layer drops. Call it when the preview
    /// panel closes while the window stays open.
    pub fn detach(&mut self, now: Timestamp) {
        self.set_marks(&[]);
        self.leave_window();
        self.placed = None;
        self.awaiting_frame = false;
        if let Some(old) = self.shown.take() {
            self.detached = Some(old);
        }
        self.retire(now);
    }

    /// Takes the visual out of the window and waits until the compositor has applied it
    /// (UI thread, not real-time).
    fn leave_window(&mut self) {
        if let Some(target) = self.target.take() {
            // SAFETY: clears the root of our own target, commits, and waits for the
            // compositor to process the commit.
            unsafe {
                let _ = target.SetRoot(None::<&IDCompositionVisual>);
                let _ = self.dcomp.Commit();
                let _ = self.dcomp.WaitForCommitCompletion();
            }
        }
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
        let (x, y, scale) = crate::share::geometry::fit(
            (rect.x, rect.y, rect.width, rect.height),
            (self.width, self.height),
        );
        // Scale and translation in one matrix with a zero offset, so the result does not
        // depend on the order in which DirectComposition applies offset and transform.
        let m = Matrix3x2 {
            M11: scale as f32,
            M12: 0.0,
            M21: 0.0,
            M22: scale as f32,
            M31: x.round() as f32,
            M32: y.round() as f32,
        };
        // SAFETY: plain property changes of our own visual; `m` outlives the call.
        unsafe {
            let _ = self.visual.SetOffsetX2(0.0);
            let _ = self.visual.SetOffsetY2(0.0);
            let _ = self.visual.SetTransform2(&m);
        }
        self.apply_cutouts();
        self.commit();
    }

    /// Leaves `cutouts` out: toolkit content drawn over the layer (in-window popups,
    /// toasts), in the same coordinates as [`PreviewLayer::set_rect`] (ADR-0054). Covers
    /// grow to whole pixels. Marks are drawn around them; while one overlaps the picture
    /// the layer hides (the Windows interim of ADR-0054 decision 5). A cover that is not
    /// finite, or more than [`MAX_CUTOUTS`], hides the layer. Does nothing when the
    /// covers did not change.
    pub fn set_cutouts(&mut self, cutouts: &[LayerRect]) {
        let mut wanted = [None; MAX_CUTOUTS];
        let mut bad = cutouts.len() > MAX_CUTOUTS;
        for (slot, c) in wanted.iter_mut().zip(cutouts) {
            *slot = Edges::from_rect(c.x, c.y, c.width, c.height).map(|e| e.snapped_outward(1.0));
            bad |= slot.is_none();
        }
        if wanted == self.cutouts && bad == self.cut_bad {
            return;
        }
        self.cutouts = wanted;
        self.cut_bad = bad;
        self.apply_cutouts();
        self.commit();
    }

    /// Hides the picture while a cover overlaps it and draws the marks around the covers.
    fn apply_cutouts(&mut self) {
        let over = self.placed.is_some_and(|rect| {
            let area = Edges {
                x0: rect.x,
                y0: rect.y,
                x1: rect.x + rect.width.max(0.0),
                y1: rect.y + rect.height.max(0.0),
            };
            let picture = region::picture(area, (self.width, self.height), 1.0);
            self.cutouts.iter().flatten().any(|c| c.overlaps(&picture))
        });
        self.cut_over = over;
        self.apply_root();
        self.draw_marks();
    }

    /// Draws `marks` over the picture (a selection's outline and handles, ADR-0048), in
    /// the same coordinates as [`PreviewLayer::set_rect`] (physical pixels of the client
    /// area); they replace the ones drawn before, and an empty slice removes them. At
    /// most [`MAX_MARKS`] are drawn; marks that are not finite or have a negative size
    /// are skipped. Visuals and colour surfaces are made on first use and then reused.
    /// A failure to make one leaves the marks out; the picture is not affected.
    pub fn set_marks(&mut self, marks: &[Mark]) {
        if marks.is_empty() && self.overlay.is_none() {
            return;
        }
        self.wanted = [None; MAX_MARKS];
        for (slot, m) in self
            .wanted
            .iter_mut()
            .zip(marks.iter().filter(|m| m.drawable()))
        {
            *slot = Some(*m);
        }
        self.draw_marks();
        self.commit();
    }

    /// Draws the wanted marks as their pieces outside the covers (ADR-0054 decision 3).
    fn draw_marks(&mut self) {
        if self.overlay.is_none() && self.wanted.iter().all(Option::is_none) {
            return;
        }
        if self.overlay.is_none() {
            // SAFETY: a new visual of our own device, added to our own root above the
            // picture.
            let made = unsafe {
                self.dcomp
                    .CreateVisual()
                    .and_then(|o| self.root.AddVisual(&o, true, &self.visual).map(|()| o))
            };
            match made {
                Ok(o) => self.overlay = Some(o),
                Err(_) => return,
            }
        }
        let mut covers = [Edges {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        }; MAX_CUTOUTS];
        let mut n = 0;
        for (slot, c) in covers.iter_mut().zip(self.cutouts.iter().flatten()) {
            *slot = *c;
            n += 1;
        }
        let covers = covers.get(..n).unwrap_or(&[]);
        let mut cut = std::mem::take(&mut self.mark_pieces);
        let mut scratch = std::mem::take(&mut self.pieces);
        cut.clear();
        for m in self.wanted.iter().flatten() {
            // Edges on whole physical pixels; at least one pixel, so a thin line never
            // rounds away.
            let x0 = snap(m.x, 1.0);
            let y0 = snap(m.y, 1.0);
            let e = Edges {
                x0,
                y0,
                x1: snap(m.x + m.width, 1.0).max(x0 + 1.0),
                y1: snap(m.y + m.height, 1.0).max(y0 + 1.0),
            };
            region::subtract(e, covers, MAX_MARK_PIECES, &mut scratch);
            let room = MAX_MARK_PIECES.saturating_sub(cut.len());
            cut.extend(scratch.iter().take(room).map(|p| (*p, m.rgba)));
        }
        self.pieces = scratch;
        let mut shown = 0;
        for &(piece, rgba) in &cut {
            let Some(content) = self.color_surface(rgba) else {
                continue;
            };
            if shown == self.mark_visuals.len() && !self.add_mark_visual() {
                break;
            }
            let Some(v) = self.mark_visuals.get(shown) else {
                break;
            };
            // A 1 x 1 surface scaled to the piece.
            let x0 = piece.x0;
            let y0 = piece.y0;
            let w = piece.x1 - piece.x0;
            let h = piece.y1 - piece.y0;
            let t = Matrix3x2 {
                M11: w as f32,
                M12: 0.0,
                M21: 0.0,
                M22: h as f32,
                M31: x0 as f32,
                M32: y0 as f32,
            };
            // SAFETY: property changes of our own visual with our own surface; `t`
            // outlives the call.
            unsafe {
                let _ = v.SetContent(&content);
                let _ = v.SetTransform2(&t);
            }
            shown += 1;
        }
        // The rest show nothing: no content.
        for v in self.mark_visuals.iter().skip(shown) {
            // SAFETY: clears the content of our own visual.
            let _ = unsafe { v.SetContent(None::<&windows::core::IUnknown>) };
        }
        self.mark_pieces = cut;
    }

    /// One more mark visual under the overlay; false if it could not be made.
    fn add_mark_visual(&mut self) -> bool {
        let Some(overlay) = &self.overlay else {
            return false;
        };
        // SAFETY: a new visual of our own device, added to our own overlay on top.
        let made = unsafe {
            self.dcomp.CreateVisual().and_then(|v| {
                let _ = v.SetBitmapInterpolationMode(
                    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                );
                overlay
                    .AddVisual(&v, false, None::<&IDCompositionVisual>)
                    .map(|()| v)
            })
        };
        match made {
            Ok(v) => {
                self.mark_visuals.push(v);
                true
            }
            Err(_) => false,
        }
    }

    /// The 1 x 1 surface filled with `rgba` (sRGB, straight alpha), made on first use.
    fn color_surface(&mut self, rgba: [u8; 4]) -> Option<IDCompositionSurface> {
        if let Some((_, s)) = self.mark_colors.iter().find(|(c, _)| *c == rgba) {
            return Some(s.clone());
        }
        let a = f32::from(rgba[3]) / 255.0;
        // Premultiplied, values as stored (the surface is sRGB-encoded 8-bit).
        let color = [
            f32::from(rgba[0]) / 255.0 * a,
            f32::from(rgba[1]) / 255.0 * a,
            f32::from(rgba[2]) / 255.0 * a,
            a,
        ];
        // SAFETY: a surface of our own device, drawn once: `BeginDraw` gives a texture
        // and the offset of our 1 x 1 area in it (it may be part of a larger atlas), so
        // only that pixel is cleared; `EndDraw` hands it back. Every out pointer is valid
        // for its call.
        let surface = unsafe {
            let surface = self
                .dcomp
                .CreateSurface(
                    1,
                    1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .ok()?;
            let mut offset = POINT::default();
            let texture: ID3D11Texture2D = surface.BeginDraw(None, &mut offset).ok()?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            let drawn = self
                ._d3d
                .CreateRenderTargetView(&texture, None, Some(&mut rtv))
                .ok()
                .and(rtv)
                .zip(
                    self._d3d
                        .GetImmediateContext()
                        .ok()
                        .and_then(|c| c.cast::<ID3D11DeviceContext1>().ok()),
                )
                .map(|(rtv, ctx)| {
                    let at = RECT {
                        left: offset.x,
                        top: offset.y,
                        right: offset.x + 1,
                        bottom: offset.y + 1,
                    };
                    ctx.ClearView(&rtv, &color, Some(&[at]));
                })
                .is_some();
            let ended = surface.EndDraw().is_ok();
            if !(drawn && ended) {
                return None;
            }
            surface
        };
        // Bounded: a selection uses two colours; a theme change adds two more.
        if self.mark_colors.len() >= 8 {
            self.mark_colors.remove(0);
        }
        self.mark_colors.push((rgba, surface.clone()));
        Some(surface)
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
    /// ([`PreviewLayer::retire`]). Never waits, never allocates. While the layer is not
    /// in a window (before `attach_to_hwnd` or after `detach`) the frame goes back at
    /// once and nothing is presented, so no frame waits on a surface the compositor does
    /// not show.
    pub fn show(&mut self, frame: PreviewFrame, now: Timestamp) {
        if self.target.is_none() {
            drop(frame);
            self.retire(now);
            return;
        }
        if !self.lost {
            // SAFETY: a zero-timeout check of the manager's own event.
            let signalled = unsafe { WaitForSingleObject(self.lost_event, 0) } == WAIT_OBJECT_0;
            if signalled {
                self.lost = true;
            }
        }
        let buffer = match self.buffers.get(frame.slot()) {
            Some(b) if !self.lost => b,
            _ => {
                // Lost: the system reads none of our buffers any more, so every frame,
                // this one included, goes back at once.
                debug_assert!(self.lost, "frame slot {} outside the ring", frame.slot());
                drop(frame);
                self.shown = None;
                self.detached = None;
                self.retire(now);
                return;
            }
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
            self.shown = None;
            self.detached = None;
            drop(frame);
            self.retire(now);
            return;
        }
        if self.awaiting_frame {
            self.awaiting_frame = false;
            self.apply_root();
            self.commit();
        }
        // Never reallocates: the engine sends at most one frame per surface.
        if let Some(old) = self.shown.replace(frame) {
            self.retiring.push(old);
        }
        if let Some(old) = self.detached.take() {
            self.retiring.push(old);
        }
        self.retire(now);
    }

    /// Gives back replaced frames whose buffer the compositor no longer uses. Returns how
    /// many still wait. Never waits.
    pub fn retire(&mut self, now: Timestamp) -> usize {
        self.checked = now;
        let lost = self.lost;
        let buffers = &self.buffers;
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

    /// When to call [`PreviewLayer::retire`] again, if a replaced frame waits.
    pub fn next_retire(&self) -> Option<Timestamp> {
        (!self.retiring.is_empty()).then(|| self.checked + self.poll)
    }

    /// Replaced frames still waiting (not counting the frame held since `detach`).
    pub fn retiring(&self) -> usize {
        self.retiring.len()
    }

    fn apply_root(&self) {
        if self.awaiting_frame {
            return;
        }
        if let Some(target) = &self.target {
            // SAFETY: sets or clears the root of our own target to our own visual.
            let _ = unsafe {
                if self.hidden.get() || self.cut_bad || self.cut_over {
                    target.SetRoot(None::<&IDCompositionVisual>)
                } else {
                    target.SetRoot(&self.root)
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
        // The frames drop after this and go back to the engine at once, after the
        // compositor has applied the removal. If the window stays open and the compositor
        // scans a buffer out directly, it may show it until the next refresh, so the
        // engine could write a surface still on screen (one torn preview frame at worst).
        // The frame kept since `detach` goes back here too, so the same holds after it.
        self.leave_window();
    }
}
