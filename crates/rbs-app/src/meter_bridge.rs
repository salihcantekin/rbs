//! The Mixer's level bars drawn by a native layer (ADR-0035), so a meter update redraws
//! nothing in Slint.
//!
//! Every level bar (`NativeMeter` in Slint) in the main window tells where it is (`Studio.meter-placed`) and the
//! panel tells where its viewport is (`Studio.mixer-placed`). Before every Slint redraw
//! [`place`] puts the bridge over the viewport, gives it the bars, and hides it while
//! Slint draws something over the Mixer (a popup, a toast, a menu: ADR-0035 decision 4).
//! While the bridge shows, `Studio.native-meters` is true and Slint draws only the
//! bars' backgrounds; [`set_view`] then takes each meter's view for the bridge and the
//! meter rows keep only what Slint still draws (the clip box and the readout).
//!
//! macOS has the layer `rbs_app::native::MeterBridgeLayer` (Core Animation), Windows
//! `rbs_gpu::share::SolidLayer` (DirectComposition, on the device the preview layer
//! made). Elsewhere, in a floating window, and while covered, Slint draws the meters as before (decision 6).
//! Zero cost when unused (P13): the bridge exists only while the Mixer shows in the main
//! window; it adds no thread and no timer.
//!
//! UI thread only.

use std::cell::RefCell;
use std::collections::BTreeMap;

use rbs_ui::layout::Rect;
use rbs_ui::meter::MeterView;

use crate::ui::MainWindow;

/// One bar in window coordinates (logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bar {
    rect: Rect,
    vertical: bool,
}

#[derive(Default)]
struct State {
    /// The Mixer shows in the main window and the platform has a layer.
    wanted: bool,
    /// The shown bars by key (source id, -1 for the master).
    bars: BTreeMap<i32, Bar>,
    /// The Mixer panel in the window.
    viewport: Option<Rect>,
    /// The bars' keys in the bridge's order (ascending), and their views.
    keys: Vec<i32>,
    views: Vec<MeterView>,
    /// The bars or the viewport changed since the bridge last got them.
    dirty: bool,
    /// What `Studio.native-meters` says: the bridge draws the moving parts.
    native: bool,
    #[cfg(any(target_os = "macos", windows))]
    layer: Option<imp::Layer>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Whether this platform has a meter layer.
const PLATFORM: bool = cfg!(any(target_os = "macos", windows));

/// A bar in the main window's Mixer was placed (or moved). `shown: false` (a zero-wide
/// cell) or an empty size takes it out.
pub fn placed(key: i32, rect: Rect, vertical: bool, shown: bool) {
    with(move |st| {
        let usable = shown && rect.w > 0.0 && rect.h > 0.0;
        let changed = if usable {
            st.bars.insert(key, Bar { rect, vertical }) != Some(Bar { rect, vertical })
        } else {
            st.bars.remove(&key).is_some()
        };
        st.dirty |= changed;
        if changed {
            st.apply();
        }
    });
}

/// The main window's Mixer panel was placed.
pub fn viewport(rect: Rect) {
    with(move |st| {
        if st.viewport != Some(rect) {
            st.viewport = Some(rect);
            st.dirty = true;
            st.apply();
        }
    });
}

/// The Mixer's bars were made again: forgets them all and asks each to tell again
/// (bars that went away tell nothing).
pub fn relayout(window: &MainWindow) {
    with(|st| {
        st.bars.clear();
        st.dirty = true;
    });
    let studio = slint::ComponentHandle::global::<crate::ui::Studio>(window);
    studio.set_meter_epoch(studio.get_meter_epoch().wrapping_add(1));
}

/// Whether the Mixer shows in the main window. When it stops showing the bridge goes
/// away and Slint draws the meters again.
pub fn set_wanted(window: &MainWindow, wanted: bool) {
    let wanted = wanted && PLATFORM;
    let unset = STATE.with(|s| {
        let Ok(mut st) = s.try_borrow_mut() else {
            return false;
        };
        if st.wanted == wanted {
            return false;
        }
        st.wanted = wanted;
        if !wanted {
            st.bars.clear();
            st.viewport = None;
            st.keys.clear();
            st.views.clear();
            #[cfg(any(target_os = "macos", windows))]
            {
                st.layer = None;
            }
        }
        st.dirty = true;
        !wanted && std::mem::take(&mut st.native)
    });
    if unset {
        slint::ComponentHandle::global::<crate::ui::Studio>(window).set_native_meters(false);
    }
}

/// Takes `view` for meter `key` when the bridge draws it, and says so; then the row
/// keeps its zones and peak as they are (Slint does not draw them). Call [`flush`] after
/// the last meter of a tick.
pub fn set_view(key: i32, view: &rbs_ui::meter::MeterView) -> bool {
    STATE.with(|s| {
        let Ok(mut st) = s.try_borrow_mut() else {
            return false;
        };
        if !st.native {
            return false;
        }
        let Ok(i) = st.keys.binary_search(&key) else {
            // A bar that has not told where it is yet: Slint does not draw it either
            // while native, so it waits for its place.
            return true;
        };
        // The clip box is Slint's (it changes rarely); the layer has no clip rectangle.
        st.views[i] = rbs_ui::meter::MeterView {
            clipped: false,
            ..*view
        };
        true
    })
}

/// Shows the views [`set_view`] took, in one transaction.
pub fn flush() {
    #[cfg(any(target_os = "macos", windows))]
    STATE.with(|s| {
        if let Ok(mut st) = s.try_borrow_mut() {
            let st = &mut *st;
            if st.native
                && let Some(layer) = st.layer.as_mut()
            {
                layer.bridge.show(&st.views);
            }
        }
    });
}

/// Before every redraw of the main window: places the bridge and decides whether it or
/// Slint draws the meters.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub fn place(window: &MainWindow) {
    #[cfg(any(target_os = "macos", windows))]
    {
        let native = STATE.with(|s| {
            let Ok(mut st) = s.try_borrow_mut() else {
                return None;
            };
            let st = &mut *st;
            let was = st.native;
            st.native = st.wanted && imp::place(st, window);
            (st.native != was).then_some(st.native)
        });
        if let Some(native) = native {
            // Not from inside the rendering notifier: the change redraws the window.
            let weak = slint::ComponentHandle::as_weak(window);
            let timer = slint::Timer::default();
            crate::entry::start(
                &timer,
                slint::TimerMode::SingleShot,
                std::time::Duration::ZERO,
                move || {
                    if let Some(w) = weak.upgrade() {
                        slint::ComponentHandle::global::<crate::ui::Studio>(&w)
                            .set_native_meters(native);
                    }
                },
            );
            std::mem::forget(timer);
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = window;
}

fn with(f: impl FnOnce(&mut State) + Send + 'static) {
    let later = STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut st) => {
            if st.wanted {
                f(&mut st);
            }
            None
        }
        Err(_) => Some(f),
    });
    // Told while the state was in use (a bar made during a meter tick): next turn.
    if let Some(f) = later {
        let _ = crate::entry::invoke(move || {
            STATE.with(|s| {
                if let Ok(mut st) = s.try_borrow_mut()
                    && st.wanted
                {
                    f(&mut st);
                }
            });
        });
    }
}

impl State {
    /// The bars relative to the viewport, in key order, and the keys and views to match.
    /// The views of bars that stay keep their last value.
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    fn sync_keys(&mut self) {
        if self.keys.iter().copied().eq(self.bars.keys().copied()) {
            return;
        }
        let old_keys = std::mem::take(&mut self.keys);
        let old_views = std::mem::take(&mut self.views);
        for &k in self.bars.keys() {
            self.keys.push(k);
            let v = old_keys
                .binary_search(&k)
                .ok()
                .and_then(|i| old_views.get(i).copied())
                .unwrap_or(MeterView::SILENT);
            self.views.push(v);
        }
    }

    /// Gives a changed layout to the bridge at once (it redraws without Slint).
    fn apply(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        if self.dirty && self.layer.is_some() {
            imp::apply(self);
        }
    }
}

#[cfg(target_os = "macos")]
use mac as imp;
#[cfg(windows)]
use win as imp;

#[cfg(target_os = "macos")]
mod mac {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rbs_app::native::{MeterBridgeLayer, MeterColors, MeterGeometry};
    use rbs_ui::layout::Rect;
    use rbs_ui::meter::Orientation;
    use slint::ComponentHandle;

    use super::{State, overlaps};
    use crate::ui::{MainWindow, Theme, Tokens};

    pub(super) struct Layer {
        pub(super) bridge: MeterBridgeLayer,
        attached: bool,
        colors: MeterColors,
        geometry: Vec<MeterGeometry>,
        covers: Vec<Rect>,
    }

    fn colors(window: &MainWindow) -> MeterColors {
        let theme = window.global::<Theme>();
        let c = |c: slint::Color| {
            let c = c.to_argb_u8();
            rbs_ui::tokens::Color {
                r: c.red,
                g: c.green,
                b: c.blue,
                a: c.alpha,
            }
        };
        MeterColors {
            low: c(theme.get_meter_low()),
            mid: c(theme.get_meter_mid()),
            high: c(theme.get_meter_high()),
            clip: c(theme.get_meter_clip()),
        }
    }

    /// Places the bridge; true when it shows (attached, over a viewport, not covered).
    pub(super) fn place(st: &mut State, window: &MainWindow) -> bool {
        let Some(viewport) = st.viewport.filter(|v| v.w > 0.0 && v.h > 0.0) else {
            if let Some(l) = st.layer.as_ref() {
                l.bridge.set_hidden(true);
            }
            return false;
        };
        let colors = colors(window);
        let layer = st.layer.get_or_insert_with(|| Layer {
            bridge: MeterBridgeLayer::new(&colors),
            attached: false,
            colors,
            geometry: Vec::new(),
            covers: Vec::new(),
        });
        if !layer.attached {
            let window_handle = window.window().window_handle();
            let Ok(handle) = window_handle.window_handle() else {
                return false;
            };
            let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                return false;
            };
            // SAFETY: the handle is the live NSView of this window, which the renderer
            // made layer-backed; this runs on the main thread that owns it.
            if let Err(e) = unsafe { layer.bridge.attach_to_ns_view(appkit.ns_view) } {
                crate::studio_ui::log(&format!("meters: cannot attach the layer: {e}"));
                return false;
            }
            layer.attached = true;
            st.dirty = true;
        }
        if layer.colors != colors {
            layer.colors = colors;
            layer.bridge.set_colors(&colors);
        }
        // The well's rounded ends (`radius.sm`, as `NativeMeter` in Slint); does nothing
        // when unchanged.
        layer
            .bridge
            .set_corner_radius(window.global::<Tokens>().get_radius_sm());
        crate::preview::covers(window, &mut layer.covers);
        let covered = layer.covers.iter().any(|c| overlaps(*c, viewport));
        layer.bridge.set_hidden(covered);
        if covered {
            return false;
        }
        if st.dirty {
            apply(st);
        }
        true
    }

    /// Gives the viewport and the bars to the bridge.
    pub(super) fn apply(st: &mut State) {
        st.sync_keys();
        let Some(viewport) = st.viewport else { return };
        let Some(layer) = st.layer.as_mut() else {
            return;
        };
        st.dirty = false;
        layer.bridge.set_rect(viewport);
        layer.geometry.clear();
        layer
            .geometry
            .extend(st.bars.values().map(|b| MeterGeometry {
                bar: Rect {
                    x: b.rect.x - viewport.x,
                    y: b.rect.y - viewport.y,
                    w: b.rect.w,
                    h: b.rect.h,
                },
                // The clip box stays Slint's.
                clip: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                },
                orientation: if b.vertical {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                },
            }));
        layer.bridge.set_meters(&layer.geometry);
        layer.bridge.show(&st.views);
    }
}

/// Whether `a` (not empty) overlaps `b`.
#[cfg(any(target_os = "macos", windows))]
fn overlaps(a: Rect, b: Rect) -> bool {
    a.w > 0.0
        && a.h > 0.0
        && a.x < b.x + b.w
        && b.x < a.x + a.w
        && a.y < b.y + b.h
        && b.y < a.y + a.h
}

/// Windows: the moving parts as DirectComposition visuals in the window's one topmost
/// target, on the composition device the preview layer made (ADR-0036); while there is
/// none, Slint draws.
#[cfg(windows)]
mod win {
    use std::num::NonZeroIsize;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rbs_gpu::share::{LayerRect, Solid, SolidGroup, SolidLayer};
    use rbs_ui::layout::Rect;
    use rbs_ui::meter::{MeterView, Orientation};
    use slint::ComponentHandle;

    use super::{State, overlaps};
    use crate::ui::{MainWindow, Theme, Tokens};

    /// One bar: where it is relative to the viewport (logical pixels), which way it grows
    /// and what it shows now.
    struct Meter {
        bar: Rect,
        orientation: Orientation,
        shown: Option<MeterView>,
    }

    /// The bars on a [`SolidLayer`], one clipped group each: the three zones and the
    /// peak line.
    pub(super) struct Bridge {
        solids: SolidLayer,
        meters: Vec<Meter>,
        /// `color.meter.low`, `.mid`, `.high`.
        zones: [[u8; 4]; 3],
        /// Window scale factor: logical to physical pixels.
        scale: f32,
        /// The bar's rounded ends (`radius.sm`, logical pixels).
        radius: f32,
        groups: Vec<SolidGroup>,
        scratch: Vec<Solid>,
    }

    impl Bridge {
        /// Shows one update: `views[i]` on bar `i`, in one commit; bars whose view did
        /// not change cost nothing. No heap allocation once the visuals exist.
        pub(super) fn show(&mut self, views: &[MeterView]) {
            let mut changed = false;
            for (i, (m, v)) in self.meters.iter_mut().zip(views).enumerate() {
                if m.shown == Some(*v) {
                    continue;
                }
                m.shown = Some(*v);
                changed = true;
                draw(
                    &mut self.solids,
                    &mut self.scratch,
                    i,
                    m,
                    *v,
                    self.zones,
                    self.scale,
                );
            }
            if changed {
                self.solids.commit();
            }
        }

        /// Draws every bar again (geometry, scale or colour change).
        fn redraw(&mut self) {
            for (i, m) in self.meters.iter().enumerate() {
                if let Some(v) = m.shown {
                    draw(
                        &mut self.solids,
                        &mut self.scratch,
                        i,
                        m,
                        v,
                        self.zones,
                        self.scale,
                    );
                }
            }
        }
    }

    fn physical(r: Rect, scale: f32) -> LayerRect {
        LayerRect {
            x: f64::from(r.x * scale),
            y: f64::from(r.y * scale),
            width: f64::from(r.w * scale),
            height: f64::from(r.h * scale),
        }
    }

    fn draw(
        solids: &mut SolidLayer,
        scratch: &mut Vec<Solid>,
        i: usize,
        m: &Meter,
        v: MeterView,
        zones: [[u8; 4]; 3],
        scale: f32,
    ) {
        let f = v.frames(m.bar, m.orientation);
        scratch.clear();
        for (r, rgba) in f.zones.iter().zip(zones) {
            scratch.push(Solid {
                rect: physical(*r, scale),
                rgba,
            });
        }
        if let Some(p) = f.peak {
            scratch.push(Solid {
                rect: physical(p, scale),
                rgba: zones.get(f.peak_zone).copied().unwrap_or(zones[0]),
            });
        }
        solids.set_solids(i, scratch);
    }

    pub(super) struct Layer {
        pub(super) bridge: Bridge,
        attached: bool,
        covers: Vec<Rect>,
    }

    fn zone_colors(window: &MainWindow) -> [[u8; 4]; 3] {
        let theme = window.global::<Theme>();
        let c = |c: slint::Color| {
            let c = c.to_argb_u8();
            [c.red, c.green, c.blue, c.alpha]
        };
        [
            c(theme.get_meter_low()),
            c(theme.get_meter_mid()),
            c(theme.get_meter_high()),
        ]
    }

    /// Places the bridge; true when it shows (attached, over a viewport, not covered).
    pub(super) fn place(st: &mut State, window: &MainWindow) -> bool {
        if st.layer.as_ref().is_some_and(|l| l.bridge.solids.lost()) {
            // The device went away: Slint draws until the preview makes a new one.
            st.layer = None;
        }
        let Some(viewport) = st.viewport.filter(|v| v.w > 0.0 && v.h > 0.0) else {
            if let Some(l) = st.layer.as_ref() {
                l.bridge.solids.set_hidden(true);
            }
            return false;
        };
        if st.layer.is_none() {
            // No composition device yet (the preview makes it): Slint draws for now.
            let Ok(solids) = SolidLayer::new() else {
                return false;
            };
            st.layer = Some(Layer {
                bridge: Bridge {
                    solids,
                    meters: Vec::new(),
                    zones: zone_colors(window),
                    scale: 1.0,
                    radius: 0.0,
                    groups: Vec::new(),
                    scratch: Vec::with_capacity(4),
                },
                attached: false,
                covers: Vec::new(),
            });
        }
        let Some(layer) = st.layer.as_mut() else {
            return false;
        };
        if !layer.attached {
            let window_handle = window.window().window_handle();
            let Ok(handle) = window_handle.window_handle() else {
                return false;
            };
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return false;
            };
            let hwnd: NonZeroIsize = win32.hwnd;
            // SAFETY: the handle is the live top-level window of this process; this runs
            // on the UI thread that owns it.
            if let Err(e) = unsafe { layer.bridge.solids.attach_to_hwnd(hwnd) } {
                crate::studio_ui::log(&format!("meters: cannot attach the layer: {e}"));
                st.layer = None;
                return false;
            }
            layer.attached = true;
            st.dirty = true;
        }
        let zones = zone_colors(window);
        let scale = window.window().scale_factor();
        let radius = window.global::<Tokens>().get_radius_sm();
        let bridge = &mut layer.bridge;
        if bridge.zones != zones {
            bridge.zones = zones;
            for m in &mut bridge.meters {
                m.shown = None;
            }
            st.dirty = true;
        }
        if bridge.scale != scale || bridge.radius != radius {
            bridge.scale = scale;
            bridge.radius = radius;
            st.dirty = true;
        }
        crate::preview::covers(window, &mut layer.covers);
        let covered = layer.covers.iter().any(|c| overlaps(*c, viewport));
        layer.bridge.solids.set_hidden(covered);
        if covered {
            return false;
        }
        if st.dirty {
            apply(st);
        }
        true
    }

    /// Gives the viewport and the bars to the bridge.
    pub(super) fn apply(st: &mut State) {
        st.sync_keys();
        let Some(viewport) = st.viewport else { return };
        let Some(layer) = st.layer.as_mut() else {
            return;
        };
        st.dirty = false;
        let b = &mut layer.bridge;
        let scale = b.scale;
        b.solids.set_rect(physical(viewport, scale));
        b.meters.clear();
        b.meters.extend(st.bars.values().map(|bar| {
            let rel = Rect {
                x: bar.rect.x - viewport.x,
                y: bar.rect.y - viewport.y,
                w: bar.rect.w,
                h: bar.rect.h,
            };
            Meter {
                bar: rel,
                orientation: if bar.vertical {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                },
                shown: None,
            }
        }));
        b.groups.clear();
        b.groups.extend(b.meters.iter().map(|m| SolidGroup {
            clip: physical(m.bar, scale),
            radius: f64::from(b.radius * scale),
        }));
        b.solids.set_groups(&b.groups);
        for (m, v) in b.meters.iter_mut().zip(&st.views) {
            m.shown = Some(*v);
        }
        b.redraw();
        b.solids.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32) -> Rect {
        Rect {
            x,
            y: 0.0,
            w: 10.0,
            h: 100.0,
        }
    }

    #[test]
    fn keys_follow_the_bars_and_keep_the_views_of_bars_that_stay() {
        let mut st = State::default();
        let lit = MeterView {
            level: 0.5,
            peak: 0.6,
            clipped: false,
        };
        st.bars.insert(
            3,
            Bar {
                rect: rect(0.0),
                vertical: true,
            },
        );
        st.bars.insert(
            -1,
            Bar {
                rect: rect(20.0),
                vertical: true,
            },
        );
        st.sync_keys();
        assert_eq!(st.keys, vec![-1, 3], "the master first, then by id");
        st.views[1] = lit;
        st.bars.insert(
            7,
            Bar {
                rect: rect(40.0),
                vertical: true,
            },
        );
        st.sync_keys();
        assert_eq!(st.keys, vec![-1, 3, 7]);
        assert_eq!(st.views[1], lit, "source 3 keeps its view");
        assert_eq!(st.views[2], MeterView::SILENT, "a new bar starts silent");
    }

    #[test]
    fn nothing_is_drawn_natively_until_the_bridge_shows() {
        STATE.with(|s| *s.borrow_mut() = State::default());
        let view = MeterView {
            level: 0.5,
            peak: 0.5,
            clipped: false,
        };
        assert!(
            !set_view(3, &view),
            "Slint draws while the bridge is not shown"
        );
        placed(3, rect(0.0), true, true);
        assert!(
            STATE.with(|s| s.borrow().bars.is_empty()),
            "bars are not kept while the Mixer is not wanted (P13)"
        );
    }
}
