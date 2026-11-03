//! Audio meters as Core Animation layers above the window's content (ADR-0035).
//!
//! The toolkit draws what does not move (the meter's background, scale and labels, and
//! the clip indicator's outline) and keeps the accessible element. This layer draws what
//! moves at the meter rate: the lit zones, the peak-hold line and the lit clip
//! indicator. An update is one Core Animation transaction; the toolkit does not redraw.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CAAutoresizingMask, CALayer, CATransaction};
use rbs_ui::layout::Rect;
use rbs_ui::meter::{MeterView, Orientation};
use rbs_ui::tokens::{Color, Tokens};

/// Where one meter's moving parts go, relative to the bridge's rectangle (top-left
/// origin, logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterGeometry {
    /// The bar the zones fill.
    pub bar: Rect,
    /// The clip indicator, filled while clipped.
    pub clip: Rect,
    /// Which way the bar grows.
    pub orientation: Orientation,
}

/// The meter colour tokens of one theme (`color.meter.low`, `.mid`, `.high`, `.clip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterColors {
    /// `color.meter.low`.
    pub low: Color,
    /// `color.meter.mid`.
    pub mid: Color,
    /// `color.meter.high`.
    pub high: Color,
    /// `color.meter.clip`.
    pub clip: Color,
}

impl MeterColors {
    /// The meter colours of `theme` (one of `rbs_ui::tokens::THEMES`), or `None` if the
    /// tokens lack that theme or a meter colour.
    pub fn from_tokens(tokens: &Tokens, theme: &str) -> Option<Self> {
        let colors = &tokens.themes.iter().find(|(name, _)| name == theme)?.1;
        let get = |name: &str| colors.get(name).copied();
        Some(MeterColors {
            low: get("color.meter.low")?,
            mid: get("color.meter.mid")?,
            high: get("color.meter.high")?,
            clip: get("color.meter.clip")?,
        })
    }
}

/// The colours as Core Graphics colours, made once per theme.
struct CgColors {
    zones: [CFRetained<CGColor>; 3],
    clip: CFRetained<CGColor>,
}

impl CgColors {
    fn new(c: &MeterColors) -> Self {
        let cg = |c: Color| {
            let ch = |v: u8| f64::from(v) / 255.0;
            CGColor::new_srgb(ch(c.r), ch(c.g), ch(c.b), ch(c.a))
        };
        CgColors {
            zones: [cg(c.low), cg(c.mid), cg(c.high)],
            clip: cg(c.clip),
        }
    }
}

/// One meter's layers.
struct MeterLayers {
    zones: [Retained<CALayer>; 3],
    peak: Retained<CALayer>,
    clip: Retained<CALayer>,
    geometry: MeterGeometry,
    /// What it shows now; `None` until the first update and after a colour change.
    shown: Option<MeterView>,
    /// Zone whose colour the peak line has.
    peak_zone: usize,
}

/// A bridge of audio meters as Core Animation layers over a window's content.
///
/// Not `Send`: create and use it on the thread that owns the window (the UI's main
/// thread). Like the preview layer (ADR-0034) it sits above the toolkit's layer, so the
/// UI hides it ([`MeterBridgeLayer::set_hidden`]) while toolkit content overlaps it.
pub struct MeterBridgeLayer {
    /// Container over the bridge's rectangle; its sublayers use top-left coordinates
    /// once it is placed ([`MeterBridgeLayer::set_rect`]).
    root: Retained<CALayer>,
    meters: Vec<MeterLayers>,
    colors: CgColors,
    /// The last placement: the rectangle, the superlayer's height and whether it is
    /// flipped (the bottom-left origin depends on both).
    placed: Option<(Rect, f64, bool)>,
}

impl MeterBridgeLayer {
    /// An empty bridge, not yet in a window. Not for real-time threads.
    pub fn new(colors: &MeterColors) -> Self {
        let root = CALayer::new();
        root.setOpaque(false);
        root.setMasksToBounds(true);
        // Above the toolkit's layer even when the toolkit recreates its own.
        root.setZPosition(1.0);
        MeterBridgeLayer {
            root,
            meters: Vec::new(),
            colors: CgColors::new(colors),
            placed: None,
        }
    }

    /// Adds the bridge to an AppKit view's backing layer, above its content.
    ///
    /// # Safety
    ///
    /// `ns_view` must point to a live, layer-backed `NSView` (for example the
    /// `AppKitWindowHandle::ns_view` of a window whose renderer uses Metal), and the call
    /// must be made on the main thread.
    pub unsafe fn attach_to_ns_view(&mut self, ns_view: NonNull<c_void>) -> Result<(), String> {
        // SAFETY: the caller guarantees a live NSView on the main thread; `layer` is a
        // property of every NSView and returns its backing CALayer (retained) or nil.
        let root: Option<Retained<CALayer>> = unsafe {
            let view = ns_view.cast::<AnyObject>().as_ref();
            msg_send![view, layer]
        };
        let parent = root.ok_or("the view has no backing layer")?;
        self.attach_to_layer(&parent);
        Ok(())
    }

    /// Adds the bridge to `parent`, above its content (tests; apps use
    /// [`MeterBridgeLayer::attach_to_ns_view`]).
    #[doc(hidden)]
    pub fn attach_to_layer(&mut self, parent: &CALayer) {
        transaction(|| {
            self.root.removeFromSuperlayer();
            parent.addSublayer(&self.root);
        });
        self.placed = None;
    }

    /// Takes the bridge out of its window.
    pub fn detach(&mut self) {
        transaction(|| self.root.removeFromSuperlayer());
        self.placed = None;
    }

    /// The container layer (tests, diagnostics).
    #[doc(hidden)]
    pub fn layer(&self) -> &CALayer {
        &self.root
    }

    /// Places the bridge over `rect` of its superlayer (the view's points, top-left
    /// origin, as the toolkit's logical pixels when its scale factor is the window's). Does nothing when neither the rectangle nor the superlayer changed, so it
    /// can be called before every toolkit redraw.
    pub fn set_rect(&mut self, rect: Rect) {
        let (height, flipped) = self.root.superlayer().map_or((0.0, true), |sup| {
            (sup.bounds().size.height, sup.isGeometryFlipped())
        });
        if self.placed == Some((rect, height, flipped)) {
            return;
        }
        self.placed = Some((rect, height, flipped));
        // Core Animation's default origin is the bottom left; there the bridge keeps its
        // distance to the top while a live resize changes the height before the toolkit
        // redraws.
        let (y, mask) = if flipped {
            (f64::from(rect.y), CAAutoresizingMask::LayerMaxYMargin)
        } else {
            (
                height - f64::from(rect.y) - f64::from(rect.h),
                CAAutoresizingMask::LayerMinYMargin,
            )
        };
        let frame = CGRect::new(
            CGPoint::new(f64::from(rect.x), y),
            CGSize::new(f64::from(rect.w.max(0.0)), f64::from(rect.h.max(0.0))),
        );
        transaction(|| {
            self.root.setAutoresizingMask(mask);
            // Sublayers use top-left coordinates. `geometryFlipped` is relative to the
            // superlayer, so the container flips only when its parent does not (an
            // AppKit view's backing layer is often flipped already).
            self.root.setGeometryFlipped(!flipped);
            self.root.setFrame(frame);
        });
    }

    /// Sets the meters' geometry, in the bridge's coordinates. Adds or removes meters to
    /// match; does nothing when nothing changed. Allocates when the count changes, so call
    /// it on layout changes, not per update.
    pub fn set_meters(&mut self, geometry: &[MeterGeometry]) {
        let same = self.meters.len() == geometry.len()
            && self
                .meters
                .iter()
                .zip(geometry)
                .all(|(m, g)| m.geometry == *g);
        if same {
            return;
        }
        transaction(|| {
            while self.meters.len() > geometry.len() {
                if let Some(m) = self.meters.pop() {
                    for l in m.zones.iter().chain([&m.peak, &m.clip]) {
                        l.removeFromSuperlayer();
                    }
                }
            }
            while let Some(g) = geometry.get(self.meters.len()).copied() {
                let layer = |color: &CGColor| {
                    let l = CALayer::new();
                    l.setBackgroundColor(Some(color));
                    l.setHidden(true);
                    self.root.addSublayer(&l);
                    l
                };
                let zones = [0, 1, 2].map(|z| layer(&self.colors.zones[z]));
                let peak = layer(&self.colors.zones[0]);
                let clip = layer(&self.colors.clip);
                self.meters.push(MeterLayers {
                    zones,
                    peak,
                    clip,
                    geometry: g,
                    shown: None,
                    peak_zone: 0,
                });
            }
            for (m, g) in self.meters.iter_mut().zip(geometry) {
                if m.geometry != *g || m.shown.is_none() {
                    m.geometry = *g;
                    m.clip.setFrame(cg_rect(g.clip));
                    // Redraw with the new geometry at the next update.
                    m.shown = None;
                }
            }
        });
    }

    /// Changes the colours (theme change). The next [`MeterBridgeLayer::show`] redraws
    /// every meter.
    pub fn set_colors(&mut self, colors: &MeterColors) {
        self.colors = CgColors::new(colors);
        transaction(|| {
            for m in &mut self.meters {
                for (l, c) in m.zones.iter().zip(&self.colors.zones) {
                    l.setBackgroundColor(Some(c));
                }
                m.peak
                    .setBackgroundColor(Some(&self.colors.zones[m.peak_zone]));
                m.clip.setBackgroundColor(Some(&self.colors.clip));
                m.shown = None;
            }
        });
    }

    /// Shows one update: `views[i]` on meter `i` (extra views are ignored, missing ones
    /// leave their meters as they are). One transaction; meters whose view did not change
    /// cost nothing. Never allocates.
    pub fn show(&mut self, views: &[MeterView]) {
        let changed = self
            .meters
            .iter()
            .zip(views)
            .any(|(m, v)| m.shown != Some(*v));
        if !changed {
            return;
        }
        transaction(|| {
            for (m, v) in self.meters.iter_mut().zip(views) {
                if m.shown == Some(*v) {
                    continue;
                }
                m.shown = Some(*v);
                let f = v.frames(m.geometry.bar, m.geometry.orientation);
                for (l, r) in m.zones.iter().zip(f.zones) {
                    let empty = r.w <= 0.0 || r.h <= 0.0;
                    set_hidden(l, empty);
                    if !empty {
                        l.setFrame(cg_rect(r));
                    }
                }
                match f.peak {
                    None => set_hidden(&m.peak, true),
                    Some(r) => {
                        if m.peak_zone != f.peak_zone {
                            m.peak_zone = f.peak_zone;
                            m.peak
                                .setBackgroundColor(Some(&self.colors.zones[f.peak_zone]));
                        }
                        m.peak.setFrame(cg_rect(r));
                        set_hidden(&m.peak, false);
                    }
                }
                set_hidden(&m.clip, !v.clipped);
            }
        });
    }

    /// Hides or shows the bridge (for content the toolkit draws over it).
    pub fn set_hidden(&self, hidden: bool) {
        if self.root.isHidden() != hidden {
            transaction(|| self.root.setHidden(hidden));
        }
    }

    /// Meters in the bridge.
    pub fn meters(&self) -> usize {
        self.meters.len()
    }
}

impl Drop for MeterBridgeLayer {
    fn drop(&mut self) {
        transaction(|| self.root.removeFromSuperlayer());
    }
}

fn set_hidden(layer: &CALayer, hidden: bool) {
    if layer.isHidden() != hidden {
        layer.setHidden(hidden);
    }
}

fn cg_rect(r: Rect) -> CGRect {
    CGRect::new(
        CGPoint::new(f64::from(r.x), f64::from(r.y)),
        CGSize::new(f64::from(r.w.max(0.0)), f64::from(r.h.max(0.0))),
    )
}

/// Runs `f` in an explicit transaction without implicit animations (frame and colour
/// changes would otherwise animate).
fn transaction(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}
