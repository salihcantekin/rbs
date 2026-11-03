//! UI side: shows the preview as a Core Animation layer of the UI's window (ADR-0034).
//!
//! The layer's contents are the engine's IOSurfaces themselves, so a new frame costs one
//! property change in the UI process: no redraw by the toolkit, no GPU work and no copy.
//! The window server composites the surface with the rest of the window.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_io_surface::IOSurfaceRef;
use objc2_quartz_core::{CALayer, CATransaction, kCAGravityResizeAspect};

use super::subscriber::{PreviewFrame, PreviewSubscriber};
use crate::error::GpuError;

/// A rectangle in the view's points, origin at the top left (what UI toolkits use).
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

/// The preview as a Core Animation layer above a window's content.
///
/// Not `Send`: create and use it on the thread that owns the view (the UI's main thread).
/// The layer sits above the toolkit's own layer, so toolkit content drawn over the
/// preview rectangle (an in-window menu, for example) is hidden while the layer shows;
/// the UI hides the layer while such content overlaps it ([`PreviewLayer::set_hidden`]).
pub struct PreviewLayer {
    layer: Retained<CALayer>,
    surfaces: Vec<CFRetained<IOSurfaceRef>>,
    /// The frame whose surface is the layer's contents.
    shown: Option<PreviewFrame>,
    /// Replaced frames the window server may still read. Capacity: the ring size.
    retiring: Vec<PreviewFrame>,
    rect: Option<LayerRect>,
}

impl PreviewLayer {
    /// A layer for `subscriber`'s surfaces, not yet in a window. Shows nothing until the
    /// first [`PreviewLayer::show`]. Not for real-time threads.
    pub fn new(subscriber: &PreviewSubscriber) -> Self {
        let layer = CALayer::new();
        // SAFETY: `kCAGravityResizeAspect` is a constant NSString exported by
        // QuartzCore; reading it has no other requirement.
        layer.setContentsGravity(unsafe { kCAGravityResizeAspect });
        // The surfaces hold opaque pixels (spec: composited over opaque black).
        layer.setOpaque(true);
        // Above the toolkit's layer even when the toolkit recreates its own.
        layer.setZPosition(1.0);
        let surfaces = subscriber.io_surfaces();
        let retiring = Vec::with_capacity(surfaces.len());
        Self {
            layer,
            surfaces,
            shown: None,
            retiring,
            rect: None,
        }
    }

    /// Adds the layer to an AppKit view's backing layer, above its content.
    ///
    /// # Safety
    ///
    /// `ns_view` must point to a live, layer-backed `NSView` (for example the
    /// `AppKitWindowHandle::ns_view` of a window whose renderer uses Metal), and the call
    /// must be made on the main thread.
    pub unsafe fn attach_to_ns_view(&self, ns_view: NonNull<c_void>) -> Result<(), GpuError> {
        // SAFETY: the caller guarantees a live NSView on the main thread; `layer` is a
        // property of every NSView and returns an autoreleased (here retained) CALayer or nil.
        let root: Option<Retained<CALayer>> = unsafe {
            let view = ns_view.cast::<AnyObject>().as_ref();
            msg_send![view, layer]
        };
        let root = root.ok_or(GpuError::Unsupported("the view has no backing layer"))?;
        self.layer.removeFromSuperlayer();
        root.addSublayer(&self.layer);
        Ok(())
    }

    /// The underlying layer (tests, diagnostics).
    pub fn layer(&self) -> &CALayer {
        &self.layer
    }

    /// Places the layer over `rect` of its superlayer. Does nothing when the rectangle is
    /// unchanged, so it can be called on every toolkit redraw.
    pub fn set_rect(&mut self, rect: LayerRect) {
        if self.rect == Some(rect) {
            return;
        }
        self.rect = Some(rect);
        let mut y = rect.y;
        if let Some(sup) = self.layer.superlayer()
            && !sup.isGeometryFlipped()
        {
            // Core Animation's default origin is the bottom left.
            y = sup.bounds().size.height - rect.y - rect.height;
        }
        let frame = CGRect::new(
            CGPoint::new(rect.x, y),
            CGSize::new(rect.width.max(0.0), rect.height.max(0.0)),
        );
        transaction(|| self.layer.setFrame(frame));
    }

    /// Forgets the placed rectangle, so the next [`PreviewLayer::set_rect`] applies even if
    /// equal (call it when the window was resized: the bottom-left origin moved).
    pub fn invalidate_rect(&mut self) {
        self.rect = None;
    }

    /// Hides or shows the layer (for content the toolkit draws over the preview).
    pub fn set_hidden(&self, hidden: bool) {
        if self.layer.isHidden() != hidden {
            transaction(|| self.layer.setHidden(hidden));
        }
    }

    /// Shows `frame`. The replaced frame goes back to the engine once the window server
    /// no longer reads its surface ([`PreviewLayer::retire`]). Never waits, never allocates.
    pub fn show(&mut self, frame: PreviewFrame) {
        let Some(surface) = self.surfaces.get(frame.slot()) else {
            return;
        };
        let contents: &IOSurfaceRef = surface;
        // SAFETY: an IOSurfaceRef is toll-free bridged with the Objective-C `IOSurface`
        // class, which `CALayer.contents` accepts on macOS; the layer retains it.
        unsafe {
            let object = (contents as *const IOSurfaceRef).cast::<AnyObject>();
            transaction(|| self.layer.setContents(Some(&*object)));
        }
        if let Some(old) = self.shown.replace(frame) {
            // Never reallocates: the engine sends at most one frame per surface, so at
            // most ring size - 1 frames retire while one is shown.
            self.retiring.push(old);
        }
        self.retire();
    }

    /// Releases replaced frames whose surface the window server no longer uses. Returns
    /// how many still wait. Never waits.
    pub fn retire(&mut self) -> usize {
        let surfaces = &self.surfaces;
        self.retiring
            .retain(|f| surfaces.get(f.slot()).is_some_and(|s| s.is_in_use()));
        self.retiring.len()
    }

    /// Replaced frames still waiting for the window server.
    pub fn retiring(&self) -> usize {
        self.retiring.len()
    }

    /// Whether the engine may be out of free surfaces until [`PreviewLayer::retire`] runs
    /// again: every surface but the shown one is retiring. The UI then polls `retire` on a
    /// short timer; otherwise the next frame's [`PreviewLayer::show`] retires.
    pub fn engine_may_starve(&self) -> bool {
        !self.retiring.is_empty() && self.retiring.len() + 1 >= self.surfaces.len()
    }
}

impl Drop for PreviewLayer {
    fn drop(&mut self) {
        transaction(|| {
            // SAFETY: nil is a valid value for `contents`.
            unsafe { self.layer.setContents(None) };
            self.layer.removeFromSuperlayer();
        });
        // The frames drop after this and go back to the engine. The layer has left the
        // window, so nothing shows their surfaces any more and an engine write to one the
        // window server still reads for a moment cannot be seen.
    }
}

/// Runs `f` in an explicit transaction without implicit animations (contents and frame
/// changes would otherwise fade and slide).
fn transaction(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}
