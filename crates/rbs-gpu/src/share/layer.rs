//! UI side: shows the preview as a Core Animation layer of the UI's window (ADR-0034).
//!
//! The layer's contents are the engine's IOSurfaces themselves, so a new frame costs one
//! property change in the UI process: no redraw by the toolkit, no GPU work and no copy.
//! The window server composites the surface with the rest of the window.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Duration;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_io_surface::IOSurfaceRef;
use objc2_quartz_core::{CAAutoresizingMask, CALayer, CATransaction, kCAGravityResize};
use rbs_core::time::Timestamp;

use super::geometry::{MAX_MARKS, Mark, snap};
use super::region::{self, Edges, MAX_CUTOUTS, MAX_MARK_PIECES, MAX_PIECES, MAX_TILES};
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
/// Not `Send`: create and use it on one thread; once attached to a view, the thread that
/// owns the view (the UI's main thread).
/// The layer sits above the toolkit's own layer, so toolkit content drawn over the
/// preview rectangle (an in-window menu, for example) would be hidden under it; the UI
/// passes such content's rectangles to [`PreviewLayer::set_cutouts`] and the layer
/// leaves them out (ADR-0054).
///
/// Sublayers: the picture (the surface fitted into the rectangle), the tiles that
/// replace it while covers cut it (each a rectangular clip around its own picture
/// layer), and the marks' container above them.
pub struct PreviewLayer {
    layer: Retained<CALayer>,
    /// The surface, fitted into the rectangle (ADR-0054 decision 3).
    picture: Retained<CALayer>,
    /// The engine's picture size in pixels.
    image: (u32, u32),
    /// The covers last asked for, in the same units as `set_rect`.
    cutouts: [Option<Edges>; MAX_CUTOUTS],
    /// A cover was not finite or there were more than `MAX_CUTOUTS`: the layer hides.
    cut_hidden: bool,
    /// What the UI asked for with `set_hidden`.
    hidden: bool,
    /// Tile clips and their picture layers, made on first use and reused.
    tiles: Vec<(Retained<CALayer>, Retained<CALayer>)>,
    /// Tiles showing the picture now (0: the picture layer shows it whole).
    tiled: usize,
    /// Scratch for the region and the mark pieces; capacity kept.
    pieces: Vec<Edges>,
    mark_pieces: Vec<Edges>,
    surfaces: Vec<CFRetained<IOSurfaceRef>>,
    /// The frame whose surface is the layer's contents.
    shown: Option<PreviewFrame>,
    /// Replaced frames and when they were replaced. Capacity: the ring size.
    retiring: Vec<(PreviewFrame, Timestamp)>,
    retire_after: Duration,
    /// The last placement: the rectangle, the superlayer's height and whether it is
    /// flipped (the bottom-left origin depends on both).
    placed: Option<(LayerRect, f64, bool)>,
    /// Container of the mark sublayers (ADR-0048), made at the first `set_marks`.
    marks: Option<Retained<CALayer>>,
    /// The mark sublayers, reused; at most [`MAX_MARKS`].
    mark_layers: Vec<Retained<CALayer>>,
    /// The colour each mark sublayer has now, so an unchanged colour makes no `CGColor`.
    mark_colors: Vec<[u8; 4]>,
    /// The marks last asked for, drawn again when the layer moves.
    wanted: [Option<Mark>; MAX_MARKS],
}

/// Default time a replaced surface stays with the UI: two refresh periods at 60 Hz. See
/// [`PreviewLayer::set_retire_after`].
pub const DEFAULT_RETIRE_AFTER: Duration = Duration::from_millis(34);

impl PreviewLayer {
    /// A layer for `subscriber`'s surfaces, not yet in a window. Shows nothing until the
    /// first [`PreviewLayer::show`]. Not for real-time threads.
    pub fn new(subscriber: &PreviewSubscriber) -> Self {
        let layer = CALayer::new();
        // Not opaque: letterbox bars beside the image show what the toolkit draws there.
        layer.setOpaque(false);
        // Above the toolkit's layer even when the toolkit recreates its own.
        layer.setZPosition(1.0);
        let picture = picture_layer();
        transaction(|| layer.addSublayer(&picture));
        let surfaces = subscriber.io_surfaces();
        let retiring = Vec::with_capacity(surfaces.len());
        Self {
            layer,
            picture,
            image: subscriber.size(),
            cutouts: [None; MAX_CUTOUTS],
            cut_hidden: false,
            hidden: false,
            tiles: Vec::new(),
            tiled: 0,
            pieces: Vec::with_capacity(MAX_PIECES),
            mark_pieces: Vec::with_capacity(MAX_MARK_PIECES),
            surfaces,
            shown: None,
            retiring,
            retire_after: DEFAULT_RETIRE_AFTER,
            placed: None,
            marks: None,
            mark_layers: Vec::new(),
            mark_colors: Vec::new(),
            wanted: [None; MAX_MARKS],
        }
    }

    /// Surfaces the engine's ring needs so that it always finds a free one while this
    /// layer shows frames `frame_interval` apart: the shown frame, the replaced frames
    /// within the retire time, one being written and one in flight.
    pub fn surfaces_needed(&self, frame_interval: Duration) -> usize {
        super::geometry::ring_needed(frame_interval, self.retire_after)
    }

    /// Surfaces in the engine's ring.
    pub fn surfaces(&self) -> usize {
        self.surfaces.len()
    }

    /// How long a replaced frame stays with the UI before it goes back to the engine.
    ///
    /// Best effort, not a completion signal: Core Animation reports no point at which the
    /// window server stops reading a layer's old contents. [`PreviewLayer::show`] sends
    /// each change to the window server at once; it is applied at the next display
    /// refresh, or the one after if the window server misses it, and the old surface is
    /// read (or scanned out directly) until then. Use two refresh periods of the slowest
    /// display the window is on; the default suits 60 Hz and faster. If it is too short
    /// the worst case is one torn or wrong preview frame on screen, never a memory hazard
    /// (the UI keeps every surface alive). A longer time holds more surfaces
    /// ([`PreviewLayer::surfaces_needed`]).
    pub fn set_retire_after(&mut self, after: Duration) {
        self.retire_after = after;
    }

    /// Adds the layer to an AppKit view's backing layer, above its content.
    ///
    /// # Safety
    ///
    /// `ns_view` must point to a live, layer-backed `NSView` (for example the
    /// `AppKitWindowHandle::ns_view` of a window whose renderer uses Metal), and the call
    /// must be made on the main thread.
    pub unsafe fn attach_to_ns_view(&mut self, ns_view: NonNull<c_void>) -> Result<(), GpuError> {
        // SAFETY: the caller guarantees a live NSView on the main thread; `layer` is a
        // property of every NSView and returns an autoreleased (here retained) CALayer or nil.
        let root: Option<Retained<CALayer>> = unsafe {
            let view = ns_view.cast::<AnyObject>().as_ref();
            msg_send![view, layer]
        };
        let root = root.ok_or(GpuError::Unsupported("the view has no backing layer"))?;
        self.attach_to_layer(&root);
        Ok(())
    }

    /// Adds the layer to `parent`, above its content (tests; apps use
    /// [`PreviewLayer::attach_to_ns_view`]).
    #[doc(hidden)]
    pub fn attach_to_layer(&mut self, parent: &CALayer) {
        transaction(|| {
            self.layer.removeFromSuperlayer();
            parent.addSublayer(&self.layer);
        });
        self.placed = None;
    }

    /// Takes the layer out of its window and moves the shown frame to the retiring ones,
    /// so every frame goes back by the normal rule ([`PreviewLayer::retire`]). Call it
    /// when the preview panel closes while the window stays open; dropping the layer at
    /// once gives frames back that may still be on screen.
    pub fn detach(&mut self, now: Timestamp) {
        self.set_marks(&[]);
        transaction(|| {
            // SAFETY: nil is a valid value for `contents`.
            unsafe { self.picture.setContents(None) };
            for (_, pic) in &self.tiles {
                // SAFETY: as above.
                unsafe { pic.setContents(None) };
            }
            self.layer.removeFromSuperlayer();
        });
        CATransaction::flush();
        self.placed = None;
        if let Some(old) = self.shown.take() {
            self.retiring.push((old, now));
        }
    }

    /// The underlying layer (tests, diagnostics).
    #[doc(hidden)]
    pub fn layer(&self) -> &CALayer {
        &self.layer
    }

    /// The layer showing the whole picture while no cover cuts it (tests).
    #[doc(hidden)]
    pub fn picture_layer(&self) -> &CALayer {
        &self.picture
    }

    /// The tiles showing the picture now, each its clip and its picture layer (tests).
    #[doc(hidden)]
    pub fn tile_layers(&self) -> impl Iterator<Item = (&CALayer, &CALayer)> {
        self.tiles
            .iter()
            .take(self.tiled)
            .map(|(t, p)| (&**t, &**p))
    }

    /// Places the layer over `rect` of its superlayer. Does nothing when neither the
    /// rectangle nor the superlayer's height changed, so it can be called on every
    /// toolkit redraw. `rect` is in the view's points (logical pixels of the toolkit when
    /// its scale factor is the window's).
    pub fn set_rect(&mut self, rect: LayerRect) {
        // `contentsAreFlipped`: the superlayer's effective orientation (its ancestors'
        // flips combined), not only its own `geometryFlipped`.
        let (height, flipped) = self.layer.superlayer().map_or((0.0, true), |sup| {
            (sup.bounds().size.height, sup.contentsAreFlipped())
        });
        if self.placed == Some((rect, height, flipped)) {
            return;
        }
        self.placed = Some((rect, height, flipped));
        // Core Animation's default origin is the bottom left; there the layer keeps its
        // distance to the top while a live resize changes the height before the toolkit
        // redraws.
        let (y, mask) = if flipped {
            (rect.y, CAAutoresizingMask::LayerMaxYMargin)
        } else {
            (
                height - rect.y - rect.height,
                CAAutoresizingMask::LayerMinYMargin,
            )
        };
        let frame = CGRect::new(
            CGPoint::new(rect.x, y),
            CGSize::new(rect.width.max(0.0), rect.height.max(0.0)),
        );
        transaction(|| {
            self.layer.setAutoresizingMask(mask);
            self.layer.setFrame(frame);
        });
        self.arrange();
    }

    /// Leaves `cutouts` out of the picture and the marks: toolkit content drawn over the
    /// layer (in-window popups, toasts), in the same coordinates as
    /// [`PreviewLayer::set_rect`] (ADR-0054). Covers grow to whole device pixels. While
    /// no cover overlaps the picture it stays one layer; otherwise it is shown as tiles
    /// around the covers (rectangular clips sharing the surface, no copy and no mask).
    /// A cover that is not finite, or more than [`MAX_CUTOUTS`] covers, hides the layer.
    /// Does nothing when the covers did not change.
    pub fn set_cutouts(&mut self, cutouts: &[LayerRect]) {
        let mut wanted = [None; MAX_CUTOUTS];
        let mut bad = cutouts.len() > MAX_CUTOUTS;
        for (slot, c) in wanted.iter_mut().zip(cutouts) {
            *slot = Edges::from_rect(c.x, c.y, c.width, c.height);
            bad |= slot.is_none();
        }
        if wanted == self.cutouts && bad == self.cut_hidden {
            return;
        }
        self.cutouts = wanted;
        self.cut_hidden = bad;
        self.arrange();
    }

    /// Places the picture or its tiles and the marks for the current rectangle and
    /// covers, in one transaction.
    fn arrange(&mut self) {
        let Some((rect, _, _)) = self.placed else {
            self.apply_hidden();
            return;
        };
        let flipped = self.layer.contentsAreFlipped();
        let scale = self
            .layer
            .superlayer()
            .map_or(1.0, |sup| sup.contentsScale());
        let area = Edges {
            x0: rect.x,
            y0: rect.y,
            x1: rect.x + rect.width.max(0.0),
            y1: rect.y + rect.height.max(0.0),
        };
        let picture = region::picture(area, self.image, scale);
        let mut covers = [area; MAX_CUTOUTS];
        let mut n = 0;
        for (slot, c) in covers.iter_mut().zip(self.cutouts.iter().flatten()) {
            *slot = c.snapped_outward(scale);
            n += 1;
        }
        let covers = covers.get(..n).unwrap_or(&[]);
        region::subtract(picture, covers, MAX_TILES, &mut self.pieces);
        let whole = matches!(self.pieces.as_slice(), [p] if *p == picture);
        while !whole && self.tiles.len() < self.pieces.len() {
            let tile = CALayer::new();
            tile.setMasksToBounds(true);
            let pic = picture_layer();
            transaction(|| {
                tile.addSublayer(&pic);
                // Below the marks' container (z 1), above nothing else.
                self.layer.insertSublayer_atIndex(&tile, 0);
            });
            self.tiles.push((tile, pic));
        }
        let tiled = if whole { 0 } else { self.pieces.len() };
        let shown = self.shown_contents();
        let height = rect.height.max(0.0);
        let at = |e: &Edges, origin: (f64, f64), parent_h: f64| {
            let top = e.y0 - origin.1;
            let bottom = e.y1 - origin.1;
            let y = if flipped { top } else { parent_h - bottom };
            CGRect::new(
                CGPoint::new(e.x0 - origin.0, y),
                CGSize::new(e.x1 - e.x0, bottom - top),
            )
        };
        let root = (rect.x, rect.y);
        let pieces = &self.pieces;
        let tiles = &self.tiles;
        let previous = self.tiled;
        transaction(|| {
            self.picture.setFrame(at(&picture, root, height));
            self.picture.setHidden(tiled > 0);
            for (i, (tile, pic)) in tiles.iter().enumerate() {
                match pieces.get(i).filter(|_| i < tiled) {
                    Some(piece) => {
                        tile.setFrame(at(piece, root, height));
                        pic.setFrame(at(&picture, (piece.x0, piece.y0), piece.y1 - piece.y0));
                        if i >= previous {
                            // A new tile shows the frame on screen at once.
                            // SAFETY: `shown` is nil or the IOSurface of the shown frame,
                            // which `contents` accepts (see `show`).
                            unsafe { pic.setContents(shown) };
                        }
                        tile.setHidden(false);
                    }
                    None if i < previous => {
                        // No longer needed: it holds no surface.
                        // SAFETY: nil is a valid value for `contents`.
                        unsafe { pic.setContents(None) };
                        tile.setHidden(true);
                    }
                    None => {}
                }
            }
        });
        self.tiled = tiled;
        self.apply_hidden();
        self.draw_marks();
    }

    /// The shown frame's surface as `contents`, if a frame is shown.
    fn shown_contents(&self) -> Option<&AnyObject> {
        let frame = self.shown.as_ref()?;
        let surface: &IOSurfaceRef = self.surfaces.get(frame.slot())?;
        // SAFETY: an IOSurfaceRef is toll-free bridged with the Objective-C `IOSurface`
        // class; the reference lives as long as `self.surfaces`.
        Some(unsafe { &*(surface as *const IOSurfaceRef).cast::<AnyObject>() })
    }

    fn apply_hidden(&self) {
        let hidden = self.hidden || self.cut_hidden;
        if self.layer.isHidden() != hidden {
            transaction(|| self.layer.setHidden(hidden));
        }
    }

    /// Draws `marks` over the picture (a selection's outline and handles, ADR-0048), in
    /// the same coordinates as [`PreviewLayer::set_rect`]; they replace the ones drawn
    /// before, and an empty slice removes them. At most [`MAX_MARKS`] are drawn; marks
    /// that are not finite or have a negative size are skipped. The sublayers are made
    /// on first use and then reused: no Rust heap allocation after that.
    pub fn set_marks(&mut self, marks: &[Mark]) {
        if marks.is_empty() && self.marks.is_none() {
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
    }

    fn draw_marks(&mut self) {
        let Some((rect, _, _)) = self.placed else {
            return;
        };
        let wanted = self.wanted;
        let count = wanted.iter().filter(|m| m.is_some()).count();
        if count == 0 && self.marks.is_none() {
            return;
        }
        let scale = self
            .layer
            .superlayer()
            .map_or(1.0, |sup| sup.contentsScale());
        // Each mark as its pieces outside the covers (ADR-0054 decision 3): edges on the
        // window's device pixel grid; at most `MAX_MARK_PIECES`, the smallest left out.
        let mut covers = [Edges {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        }; MAX_CUTOUTS];
        let mut n = 0;
        for (slot, c) in covers.iter_mut().zip(self.cutouts.iter().flatten()) {
            *slot = c.snapped_outward(scale);
            n += 1;
        }
        let covers = covers.get(..n).unwrap_or(&[]);
        let mut cut = std::mem::take(&mut self.mark_pieces);
        let mut colored: [(usize, [u8; 4]); MAX_MARKS] = [(0, [0; 4]); MAX_MARKS];
        let mut piece_count = 0;
        let mut scratch = std::mem::take(&mut self.pieces);
        cut.clear();
        for (slot, m) in colored.iter_mut().zip(wanted.iter().flatten()) {
            let e = Edges {
                x0: snap(m.x, scale),
                y0: snap(m.y, scale),
                x1: snap(m.x + m.width, scale),
                y1: snap(m.y + m.height, scale),
            };
            region::subtract(e, covers, MAX_MARK_PIECES, &mut scratch);
            let room = MAX_MARK_PIECES.saturating_sub(cut.len());
            cut.extend(scratch.iter().take(room));
            piece_count = cut.len();
            *slot = (piece_count, m.rgba);
        }
        self.pieces = scratch;
        let count = piece_count;
        if self.marks.is_none() {
            let c = CALayer::new();
            c.setZPosition(1.0);
            transaction(|| self.layer.addSublayer(&c));
            self.marks = Some(c);
        }
        let Some(container) = &self.marks else {
            return;
        };
        while self.mark_layers.len() < count {
            let l = CALayer::new();
            transaction(|| container.addSublayer(&l));
            self.mark_layers.push(l);
            self.mark_colors.push([0, 0, 0, 0]);
        }
        // The preview layer's own orientation decides where its sublayers' origin is.
        let flipped = self.layer.contentsAreFlipped();
        let height = rect.height.max(0.0);
        let layers = &self.mark_layers;
        let colors = &mut self.mark_colors;
        let marks_of = &colored;
        let pieces = &cut;
        transaction(|| {
            container.setFrame(CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(rect.width.max(0.0), height),
            ));
            for (i, piece) in pieces.iter().enumerate() {
                let (Some(l), Some(c)) = (layers.get(i), colors.get_mut(i)) else {
                    break;
                };
                // The colour of the mark this piece belongs to.
                let rgba = marks_of
                    .iter()
                    .find(|(end, _)| i < *end)
                    .map_or([0; 4], |(_, rgba)| *rgba);
                // Relative to the layer.
                let x0 = piece.x0 - rect.x;
                let top = piece.y0 - rect.y;
                let bottom = piece.y1 - rect.y;
                let y = if flipped { top } else { height - bottom };
                l.setFrame(CGRect::new(
                    CGPoint::new(x0, y),
                    CGSize::new(piece.x1 - piece.x0, bottom - top),
                ));
                if *c != rgba {
                    *c = rgba;
                    let ch = |v: u8| f64::from(v) / 255.0;
                    let color =
                        CGColor::new_srgb(ch(rgba[0]), ch(rgba[1]), ch(rgba[2]), ch(rgba[3]));
                    l.setBackgroundColor(Some(&color));
                }
                l.setHidden(false);
            }
            for l in layers.iter().skip(pieces.len()) {
                l.setHidden(true);
            }
        });
        self.mark_pieces = cut;
    }

    /// Hides or shows the layer (while the picture is not shown at all, for example its
    /// panel is not in the window). Content drawn over the picture is cut out instead
    /// ([`PreviewLayer::set_cutouts`]).
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        self.apply_hidden();
    }

    /// Shows `frame` from `now` (any monotonic clock, the same one for every call). The
    /// replaced frame goes back to the engine once the retire time has passed
    /// ([`PreviewLayer::retire`]). Never waits, never allocates.
    pub fn show(&mut self, frame: PreviewFrame, now: Timestamp) {
        let Some(surface) = self.surfaces.get(frame.slot()) else {
            // The subscriber only delivers slots of its ring.
            debug_assert!(false, "frame slot {} outside the ring", frame.slot());
            self.retire(now);
            return;
        };
        let contents: &IOSurfaceRef = surface;
        // SAFETY: an IOSurfaceRef is toll-free bridged with the Objective-C `IOSurface`
        // class, which `CALayer.contents` accepts on macOS; the layer retains it.
        unsafe {
            let object = &*(contents as *const IOSurfaceRef).cast::<AnyObject>();
            let tiles = self.tiles.iter().take(self.tiled);
            transaction(|| {
                self.picture.setContents(Some(object));
                for (_, pic) in tiles {
                    pic.setContents(Some(object));
                }
            });
        }
        // Send the change now, not at the end of the run-loop turn, so the retire time
        // counts from when the window server has it.
        CATransaction::flush();
        if let Some(old) = self.shown.replace(frame) {
            // Never reallocates: the engine sends at most one frame per surface, so at
            // most ring size - 1 frames retire while one is shown.
            self.retiring.push((old, now));
        }
        self.retire(now);
    }

    /// Gives back replaced frames whose retire time has passed by `now`. Returns how
    /// many still wait. Never waits.
    pub fn retire(&mut self, now: Timestamp) -> usize {
        let after = self.retire_after;
        self.retiring
            .retain(|(_, replaced)| now.saturating_since(*replaced) < after);
        self.retiring.len()
    }

    /// When the oldest replaced frame can go back, if any waits.
    pub fn next_retire(&self) -> Option<Timestamp> {
        self.retiring
            .iter()
            .map(|(_, replaced)| *replaced + self.retire_after)
            .min()
    }

    /// Replaced frames still waiting.
    pub fn retiring(&self) -> usize {
        self.retiring.len()
    }

    /// Diagnostics: the shown frame, the retiring ones and one the engine may be writing
    /// fill the ring, so the engine may find no free surface. The UI does not use it to
    /// decide when to retire: while [`PreviewLayer::retiring`] is not zero it calls
    /// [`PreviewLayer::retire`] at [`PreviewLayer::next_retire`] (a one-shot timer).
    /// Waiting for the next frame instead holds surfaces up to a frame longer and makes
    /// the engine drop frames (ADR-0034).
    pub fn engine_may_starve(&self) -> bool {
        !self.retiring.is_empty() && self.retiring.len() + 2 >= self.surfaces.len()
    }
}

impl Drop for PreviewLayer {
    fn drop(&mut self) {
        transaction(|| {
            // SAFETY: nil is a valid value for `contents`.
            unsafe { self.picture.setContents(None) };
            for (_, pic) in &self.tiles {
                // SAFETY: as above.
                unsafe { pic.setContents(None) };
            }
            self.layer.removeFromSuperlayer();
        });
        // The frames drop after this and go back to the engine at once. If the window
        // stays open, the window server may show the last one until its next refresh, so
        // the engine could write a surface still on screen (one torn preview frame at
        // worst); `detach` first avoids that.
    }
}

/// A layer that shows the surface stretched to its frame (the frame has the picture's
/// aspect ratio, ADR-0054 decision 3).
fn picture_layer() -> Retained<CALayer> {
    let l = CALayer::new();
    // SAFETY: `kCAGravityResize` is a constant NSString exported by QuartzCore; reading
    // it has no other requirement.
    l.setContentsGravity(unsafe { kCAGravityResize });
    l.setOpaque(false);
    l
}

/// Runs `f` in an explicit transaction without implicit animations (contents and frame
/// changes would otherwise fade and slide).
fn transaction(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}
