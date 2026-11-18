//! Solid rectangles as DirectComposition visuals above a window's content: the moving
//! parts of the audio meters on Windows (ADR-0035 decision 6), drawn without the toolkit.
//!
//! Each rectangle is a visual showing a 1 x 1 surface of its colour, scaled to the
//! rectangle, so an update changes transforms only: no drawing, no GPU work in the UI
//! process, and one commit to the desktop compositor. Rectangles sit in groups, each
//! clipped to a rounded rectangle (a meter's bar with its rounded ends).
//!
//! The layer joins the window's one topmost target through the thread's shared
//! composition device ([`Composition`]), which a preview layer made; without one there is
//! no layer and the toolkit draws.

use std::cell::Cell;
use std::num::NonZeroIsize;
use std::rc::Rc;

use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, IDCompositionDevice,
    IDCompositionRectangleClip, IDCompositionSurface, IDCompositionVisual,
};
use windows_numerics::Matrix3x2;

use super::layer::{Composition, LayerRect};
use crate::error::GpuError;

/// Colours one layer keeps surfaces for; a theme change replaces the oldest.
const MAX_COLORS: usize = 12;

/// One clipped group of rectangles, relative to the layer's rectangle (physical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolidGroup {
    /// The group's rectangles are clipped to this rectangle.
    pub clip: LayerRect,
    /// Corner radius of the clip (physical pixels).
    pub radius: f64,
}

/// One rectangle of a group, relative to the layer's rectangle (physical pixels). Edges
/// go to whole pixels, so neighbouring rectangles meet without a gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Solid {
    /// Where it is; an empty or non-finite rectangle shows nothing.
    pub rect: LayerRect,
    /// sRGB, straight alpha.
    pub rgba: [u8; 4],
}

/// A rectangle's visual and what it shows now.
struct Piece {
    visual: IDCompositionVisual,
    /// Snapped edges and colour shown; `None` while it shows nothing.
    shown: Option<([i32; 4], [u8; 4])>,
}

struct Group {
    outer: IDCompositionVisual,
    clip: IDCompositionRectangleClip,
    wanted: Option<SolidGroup>,
    pieces: Vec<Piece>,
}

/// Solid rectangles above a window's content.
///
/// Not `Send`: create and use it on the thread that owns the window (the UI thread). It
/// is above everything the toolkit draws, so the UI hides it
/// ([`SolidLayer::set_hidden`]) while toolkit content overlaps it.
pub struct SolidLayer {
    composition: Rc<Composition>,
    dcomp: IDCompositionDevice,
    /// The layer's root: offset to the layer's rectangle and clipped to it.
    root: IDCompositionVisual,
    root_clip: IDCompositionRectangleClip,
    groups: Vec<Group>,
    colors: Vec<([u8; 4], IDCompositionSurface)>,
    window: Option<(isize, IDCompositionVisual)>,
    in_window: Cell<bool>,
    hidden: Cell<bool>,
    placed: Option<LayerRect>,
}

impl SolidLayer {
    /// A layer on the thread's composition device, not yet in a window.
    ///
    /// `GpuError::Unsupported` when the thread has no composition device (no preview
    /// layer made one, or it was lost); the toolkit then draws.
    pub fn new() -> Result<Self, GpuError> {
        let composition = Composition::current().ok_or(GpuError::Unsupported(
            "no composition device on this thread",
        ))?;
        let dcomp = composition.dcomp.clone();
        // SAFETY: new objects of the shared device; the clip is set on our own root.
        let (root, root_clip) = unsafe {
            let root = dcomp
                .CreateVisual()
                .map_err(|_| GpuError::Unsupported("CreateVisual failed"))?;
            let clip = dcomp
                .CreateRectangleClip()
                .map_err(|_| GpuError::Unsupported("CreateRectangleClip failed"))?;
            root.SetClip(&clip)
                .map_err(|_| GpuError::Unsupported("SetClip failed"))?;
            (root, clip)
        };
        Ok(Self {
            composition,
            dcomp,
            root,
            root_clip,
            groups: Vec::new(),
            colors: Vec::new(),
            window: None,
            in_window: Cell::new(false),
            hidden: Cell::new(false),
            placed: None,
        })
    }

    /// Adds the layer to `hwnd`'s topmost target, beside the window's other layers.
    /// Shows nothing until [`SolidLayer::set_rect`].
    ///
    /// # Safety
    ///
    /// `hwnd` must be a live top-level window of this process, and the call must be made
    /// on the thread that owns it.
    pub unsafe fn attach_to_hwnd(&mut self, hwnd: NonZeroIsize) -> Result<(), GpuError> {
        if self.window.is_some() {
            self.detach();
        }
        // SAFETY: the caller's guarantees are passed on.
        let root = unsafe { self.composition.join(hwnd.get()) }?;
        self.window = Some((hwnd.get(), root));
        self.placed = None;
        self.apply_root();
        self.commit();
        Ok(())
    }

    /// Takes the layer out of its window (UI thread, not real-time).
    pub fn detach(&mut self) {
        if let Some((hwnd, window_root)) = self.window.take() {
            // SAFETY: removes our own root from the window's shared root (same device),
            // drops the target if no other layer uses it, and commits.
            unsafe {
                if self.in_window.replace(false) {
                    let _ = window_root.RemoveVisual(&self.root);
                }
                self.composition.leave(hwnd);
                let _ = self.dcomp.Commit();
            }
        }
        self.placed = None;
    }

    /// Places the layer over `rect` of the window's client area (physical pixels);
    /// groups and rectangles are relative to it and clipped to it. Does nothing when the
    /// rectangle did not change, so it can be called on every toolkit redraw.
    pub fn set_rect(&mut self, rect: LayerRect) {
        if self.placed == Some(rect) {
            return;
        }
        let finite = [rect.x, rect.y, rect.width, rect.height]
            .iter()
            .all(|v| v.is_finite());
        if !finite {
            return;
        }
        self.placed = Some(rect);
        let x = rect.x.round() as f32;
        let y = rect.y.round() as f32;
        // SAFETY: property changes of our own visual and clip.
        unsafe {
            let _ = self.root.SetOffsetX2(x);
            let _ = self.root.SetOffsetY2(y);
            let _ = self.root_clip.SetLeft2(0.0);
            let _ = self.root_clip.SetTop2(0.0);
            let _ = self.root_clip.SetRight2(rect.width.max(0.0).round() as f32);
            let _ = self
                .root_clip
                .SetBottom2(rect.height.max(0.0).round() as f32);
        }
        self.apply_root();
        self.commit();
    }

    /// Sets the groups, in order (later ones above). Adds or removes groups to match;
    /// a group whose clip changed is clipped again. Allocates when the count changes, so
    /// call it on layout changes, not per update. Commit with [`SolidLayer::commit`].
    pub fn set_groups(&mut self, groups: &[SolidGroup]) {
        while self.groups.len() > groups.len() {
            if let Some(g) = self.groups.pop() {
                // SAFETY: removes our own group visual from our own root.
                let _ = unsafe { self.root.RemoveVisual(&g.outer) };
            }
        }
        while self.groups.len() < groups.len() {
            // SAFETY: new objects of our own device, joined to our own root on top.
            let made = unsafe {
                (|| {
                    let outer = self.dcomp.CreateVisual().ok()?;
                    let clip = self.dcomp.CreateRectangleClip().ok()?;
                    outer.SetClip(&clip).ok()?;
                    self.root
                        .AddVisual(&outer, false, None::<&IDCompositionVisual>)
                        .ok()?;
                    Some(Group {
                        outer,
                        clip,
                        wanted: None,
                        pieces: Vec::new(),
                    })
                })()
            };
            match made {
                Some(g) => self.groups.push(g),
                None => break,
            }
        }
        for (g, want) in self.groups.iter_mut().zip(groups) {
            if g.wanted == Some(*want) {
                continue;
            }
            g.wanted = Some(*want);
            let c = want.clip;
            let r = if want.radius.is_finite() {
                want.radius.max(0.0) as f32
            } else {
                0.0
            };
            // SAFETY: property changes of our own clip.
            unsafe {
                let _ = g.clip.SetLeft2(c.x.round() as f32);
                let _ = g.clip.SetTop2(c.y.round() as f32);
                let _ = g.clip.SetRight2((c.x + c.width).round() as f32);
                let _ = g.clip.SetBottom2((c.y + c.height).round() as f32);
                for set in [
                    IDCompositionRectangleClip::SetTopLeftRadiusX2,
                    IDCompositionRectangleClip::SetTopLeftRadiusY2,
                    IDCompositionRectangleClip::SetTopRightRadiusX2,
                    IDCompositionRectangleClip::SetTopRightRadiusY2,
                    IDCompositionRectangleClip::SetBottomLeftRadiusX2,
                    IDCompositionRectangleClip::SetBottomLeftRadiusY2,
                    IDCompositionRectangleClip::SetBottomRightRadiusX2,
                    IDCompositionRectangleClip::SetBottomRightRadiusY2,
                ] {
                    let _ = set(&g.clip, r);
                }
            }
        }
    }

    /// Groups in the layer.
    pub fn groups(&self) -> usize {
        self.groups.len()
    }

    /// Shows `solids` in group `group` (later ones above), replacing what it showed.
    /// Rectangles that did not change cost nothing. Allocates only when a group needs
    /// more visuals than ever before, or for a colour not seen before. Commit with
    /// [`SolidLayer::commit`].
    pub fn set_solids(&mut self, group: usize, solids: &[Solid]) {
        let Some(g) = self.groups.get_mut(group) else {
            return;
        };
        let mut i = 0;
        for s in solids {
            let Some(edges) = snapped(s.rect) else {
                continue;
            };
            if i == g.pieces.len() {
                // SAFETY: a new visual of our own device, added on top of our own group.
                let made = unsafe {
                    self.dcomp.CreateVisual().ok().filter(|v| {
                        let _ = v.SetBitmapInterpolationMode(
                            DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        );
                        g.outer
                            .AddVisual(v, false, None::<&IDCompositionVisual>)
                            .is_ok()
                    })
                };
                match made {
                    Some(visual) => g.pieces.push(Piece {
                        visual,
                        shown: None,
                    }),
                    None => break,
                }
            }
            let Some(p) = g.pieces.get_mut(i) else { break };
            i += 1;
            if p.shown == Some((edges, s.rgba)) {
                continue;
            }
            let color_changed = p.shown.is_none_or(|(_, c)| c != s.rgba);
            if color_changed {
                let surface = match self.colors.iter().find(|(c, _)| *c == s.rgba) {
                    Some((_, surface)) => Some(surface.clone()),
                    None => self.composition.solid_surface(s.rgba).inspect(|surface| {
                        if self.colors.len() >= MAX_COLORS {
                            self.colors.remove(0);
                        }
                        self.colors.push((s.rgba, surface.clone()));
                    }),
                };
                let Some(surface) = surface else {
                    // SAFETY: clears the content of our own visual.
                    let _ = unsafe { p.visual.SetContent(None::<&windows::core::IUnknown>) };
                    p.shown = None;
                    continue;
                };
                // SAFETY: our own visual shows our own surface.
                let _ = unsafe { p.visual.SetContent(&surface) };
            }
            let [x0, y0, x1, y1] = edges;
            let t = Matrix3x2 {
                M11: (x1 - x0) as f32,
                M12: 0.0,
                M21: 0.0,
                M22: (y1 - y0) as f32,
                M31: x0 as f32,
                M32: y0 as f32,
            };
            // SAFETY: a property change of our own visual; `t` outlives the call.
            let _ = unsafe { p.visual.SetTransform2(&t) };
            p.shown = Some((edges, s.rgba));
        }
        for p in g.pieces.iter_mut().skip(i) {
            if p.shown.take().is_some() {
                // SAFETY: clears the content of our own visual.
                let _ = unsafe { p.visual.SetContent(None::<&windows::core::IUnknown>) };
            }
        }
    }

    /// Hands the pending changes to the desktop compositor; does not wait.
    pub fn commit(&self) {
        // SAFETY: commits the shared device's pending changes; does not wait.
        let _ = unsafe { self.dcomp.Commit() };
    }

    /// Hides or shows the layer (for content the toolkit draws over it), at once.
    pub fn set_hidden(&self, hidden: bool) {
        if self.hidden.get() != hidden {
            self.hidden.set(hidden);
            self.apply_root();
            self.commit();
        }
    }

    /// Whether the composition device was lost: the layer shows nothing more and the UI
    /// drops it (the toolkit draws).
    pub fn lost(&self) -> bool {
        self.composition.lost()
    }

    fn apply_root(&self) {
        let Some((_, window_root)) = &self.window else {
            return;
        };
        let shown = !self.hidden.get() && self.placed.is_some();
        if shown == self.in_window.get() {
            return;
        }
        // SAFETY: adds or removes our own root under the window's shared root, both of
        // the shared device.
        let done = unsafe {
            if shown {
                window_root.AddVisual(&self.root, true, None::<&IDCompositionVisual>)
            } else {
                window_root.RemoveVisual(&self.root)
            }
        };
        if done.is_ok() {
            self.in_window.set(shown);
        }
    }
}

impl Drop for SolidLayer {
    fn drop(&mut self) {
        self.detach();
    }
}

/// `rect`'s edges on whole pixels, at least one pixel wide and high; `None` when it is
/// empty or not finite.
fn snapped(rect: LayerRect) -> Option<[i32; 4]> {
    let finite = [rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|v| v.is_finite());
    if !finite || rect.width <= 0.0 || rect.height <= 0.0 {
        return None;
    }
    let x0 = rect.x.round() as i32;
    let y0 = rect.y.round() as i32;
    let x1 = ((rect.x + rect.width).round() as i32).max(x0 + 1);
    let y1 = ((rect.y + rect.height).round() as i32).max(y0 + 1);
    Some([x0, y0, x1, y1])
}
