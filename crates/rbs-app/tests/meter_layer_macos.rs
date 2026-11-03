//! Conformance of the native meter bridge (ADR-0035, `docs/specs/ui-design-system.md`,
//! "AudioMeter"): layer frames follow `rbs_ui::meter`, placement converts to Core
//! Animation's origin, and unchanged input changes nothing. macOS only; no window needed.
#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_range_loop,
    missing_docs
)]

use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::{CALayer, CATransaction};
use rbs_app::native::{MeterBridgeLayer, MeterColors, MeterGeometry};
use rbs_ui::layout::Rect;
use rbs_ui::meter::{MeterView, Orientation, position};
use rbs_ui::tokens::Tokens;

/// Changes layers the way the bridge does: in an explicit transaction without
/// animations (cargo runs tests off the main thread, where no run loop would commit an
/// implicit transaction).
fn tx(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}

fn parent(flipped: bool, scale: f64) -> Retained<CALayer> {
    let parent = CALayer::new();
    tx(|| {
        parent.setFrame(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(800.0, 400.0),
        ));
        parent.setGeometryFlipped(flipped);
        parent.setContentsScale(scale);
    });
    parent
}

/// Sublayers per meter: three zones, the peak line, the clip indicator.
const PER_METER: usize = 5;

fn colors() -> MeterColors {
    MeterColors::from_tokens(&Tokens::built_in().unwrap(), "dark").unwrap()
}

fn sublayers(bridge: &MeterBridgeLayer) -> Vec<Retained<CALayer>> {
    // SAFETY: reading the sublayers of a layer only this thread uses.
    let subs = unsafe { bridge.layer().sublayers() };
    subs.map(|a| a.to_vec()).unwrap_or_default()
}

fn geometry(i: usize) -> MeterGeometry {
    let x = 10.0 + 20.0 * i as f32;
    MeterGeometry {
        bar: Rect {
            x,
            y: 10.0,
            w: 8.0,
            h: 100.0,
        },
        clip: Rect {
            x,
            y: 0.0,
            w: 8.0,
            h: 6.0,
        },
        orientation: Orientation::Vertical,
    }
}

fn close(a: CGRect, b: Rect) -> bool {
    let d = |p: f64, q: f32| (p - f64::from(q)).abs() < 1e-3;
    d(a.origin.x, b.x) && d(a.origin.y, b.y) && d(a.size.width, b.w) && d(a.size.height, b.h)
}

#[test]
fn layers_follow_the_meter_view() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.set_meters(&[geometry(0), geometry(1)]);
    assert_eq!(bridge.meters(), 2);
    assert_eq!(sublayers(&bridge).len(), 2 * PER_METER);

    let a = MeterView {
        level: position(-12.0),
        peak: position(-3.0),
        clipped: false,
    };
    let b = MeterView {
        level: 0.0,
        peak: 0.0,
        clipped: true,
    };
    bridge.show(&[a, b]);
    let subs = sublayers(&bridge);

    let fa = a.frames(geometry(0).bar, Orientation::Vertical);
    for z in 0..2 {
        assert!(!subs[z].isHidden(), "zone {z} lit");
        assert!(close(subs[z].frame(), fa.zones[z]), "zone {z} frame");
    }
    assert!(subs[2].isHidden(), "high zone unlit at -12 dB");
    assert!(!subs[3].isHidden());
    assert!(close(subs[3].frame(), fa.peak.unwrap()));
    assert!(subs[4].isHidden(), "not clipped");

    // Meter b: silent, clip lit at its rectangle.
    for k in 0..4 {
        assert!(subs[PER_METER + k].isHidden(), "part {k} of a silent meter");
    }
    assert!(!subs[PER_METER + 4].isHidden());
    assert!(close(subs[PER_METER + 4].frame(), geometry(1).clip));
}

#[test]
fn meter_count_follows_the_geometry() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.set_meters(&(0..4).map(geometry).collect::<Vec<_>>());
    assert_eq!(sublayers(&bridge).len(), 4 * PER_METER);
    bridge.set_meters(&[geometry(0)]);
    assert_eq!(bridge.meters(), 1);
    assert_eq!(sublayers(&bridge).len(), PER_METER);
    // Same geometry again: nothing changes (the layers are the same objects).
    let before = sublayers(&bridge);
    bridge.set_meters(&[geometry(0)]);
    let after = sublayers(&bridge);
    assert!(
        before
            .iter()
            .zip(&after)
            .all(|(x, y)| std::ptr::eq(&**x, &**y))
    );
}

#[test]
fn unchanged_views_change_nothing() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.set_meters(&[geometry(0)]);
    let v = MeterView {
        level: 0.5,
        peak: 0.6,
        clipped: false,
    };
    bridge.show(&[v]);
    let subs = sublayers(&bridge);
    // Move a layer behind the bridge's back; an unchanged view must not touch it.
    let moved = CGRect::new(CGPoint::new(1.0, 2.0), CGSize::new(3.0, 4.0));
    tx(|| subs[0].setFrame(moved));
    bridge.show(&[v]);
    assert_eq!(subs[0].frame(), moved);
    // A changed view redraws it.
    bridge.show(&[MeterView { level: 0.4, ..v }]);
    assert_ne!(subs[0].frame(), moved);
}

#[test]
fn placement_converts_to_the_parent_origin() {
    let parent = parent(false, 1.0);
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&parent);
    let rect = Rect {
        x: 20.0,
        y: 300.0,
        w: 400.0,
        h: 60.0,
    };
    bridge.set_rect(rect);
    let f = bridge.layer().frame();
    // Bottom-left origin: 400 - 300 - 60 = 40.
    assert_eq!(
        (f.origin.x, f.origin.y, f.size.width, f.size.height),
        (20.0, 40.0, 400.0, 60.0)
    );
    // Under an unflipped parent the container flips, so meters use top-left coordinates.
    assert!(bridge.layer().isGeometryFlipped());
    // A flipped parent (an AppKit view's backing layer often is) keeps the top-left
    // origin, and the container must not flip again (it would turn the meters upside
    // down: seen on the reference Mac).
    tx(|| parent.setGeometryFlipped(true));
    bridge.set_rect(rect);
    assert_eq!(bridge.layer().frame().origin.y, 300.0);
    assert!(!bridge.layer().isGeometryFlipped());
}

#[test]
fn edges_snap_to_device_pixels() {
    let parent = parent(true, 2.0);
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&parent);
    bridge.set_rect(Rect {
        x: 10.3,
        y: 20.0,
        w: 300.0,
        h: 56.0,
    });
    let on_grid = |v: f64| ((v * 2.0) - (v * 2.0).round()).abs() < 1e-9;
    let f = bridge.layer().frame();
    assert!(on_grid(f.origin.x) && on_grid(f.origin.x + f.size.width));
    bridge.set_meters(&[MeterGeometry {
        bar: Rect {
            x: 8.0,
            y: 16.0,
            w: 13.37,
            h: 32.0,
        },
        clip: Rect {
            x: 8.0,
            y: 8.0,
            w: 13.37,
            h: 6.0,
        },
        orientation: Orientation::Vertical,
    }]);
    bridge.show(&[MeterView {
        level: 0.77,
        peak: 0.93,
        clipped: true,
    }]);
    for l in sublayers(&bridge).iter().filter(|l| !l.isHidden()) {
        let r = l.frame();
        for edge in [
            r.origin.x,
            r.origin.y,
            r.origin.x + r.size.width,
            r.origin.y + r.size.height,
        ] {
            assert!(on_grid(edge), "{r:?}");
        }
    }
}

#[test]
fn colours_come_from_the_theme_tokens() {
    let tokens = Tokens::built_in().unwrap();
    for theme in rbs_ui::tokens::THEMES {
        assert!(
            MeterColors::from_tokens(&tokens, theme).is_some(),
            "{theme}"
        );
    }
    assert!(MeterColors::from_tokens(&tokens, "sepia").is_none());
}

#[test]
fn flips_of_all_ancestors_count() {
    // A flipped grandparent and an unflipped parent: the parent's space is top-left in
    // effect, so the bridge must neither convert y nor flip again.
    let grand = parent(true, 1.0);
    let middle = parent(false, 1.0);
    tx(|| grand.addSublayer(&middle));
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&middle);
    bridge.set_rect(Rect {
        x: 0.0,
        y: 300.0,
        w: 400.0,
        h: 60.0,
    });
    assert!(middle.contentsAreFlipped());
    assert_eq!(bridge.layer().frame().origin.y, 300.0);
    assert!(!bridge.layer().isGeometryFlipped());
    assert!(bridge.layer().contentsAreFlipped());
}

#[test]
fn geometry_changes_redraw_at_once() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.set_meters(&[geometry(0)]);
    let v = MeterView {
        level: 0.5,
        peak: 0.6,
        clipped: true,
    };
    bridge.show(&[v]);
    // Moved meter, no update after it (inactive audio): the layers follow anyway.
    let moved = geometry(3);
    bridge.set_meters(&[moved]);
    let subs = sublayers(&bridge);
    let f = v.frames(moved.bar, Orientation::Vertical);
    assert!(close(subs[0].frame(), f.zones[0]));
    assert!(close(subs[3].frame(), f.peak.unwrap()));
    assert!(close(subs[4].frame(), moved.clip));
}

#[test]
fn a_scale_change_resnaps_existing_meters() {
    let parent = parent(true, 1.0);
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&parent);
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 60.0,
    };
    bridge.set_rect(rect);
    let g = MeterGeometry {
        bar: Rect {
            x: 8.3,
            y: 16.0,
            w: 10.0,
            h: 32.0,
        },
        ..geometry(0)
    };
    bridge.set_meters(&[g]);
    bridge.show(&[MeterView {
        level: 0.5,
        peak: 0.5,
        clipped: false,
    }]);
    assert_eq!(sublayers(&bridge)[0].frame().origin.x, 8.0);
    // Window moves to a 2x display: the next placement re-snaps without an update.
    tx(|| parent.setContentsScale(2.0));
    bridge.set_rect(rect);
    assert_eq!(sublayers(&bridge)[0].frame().origin.x, 8.5);
}

#[test]
fn colour_changes_apply_at_once() {
    let tokens = Tokens::built_in().unwrap();
    let dark = MeterColors::from_tokens(&tokens, "dark").unwrap();
    let light = MeterColors::from_tokens(&tokens, "light").unwrap();
    let mut bridge = MeterBridgeLayer::new(&dark);
    bridge.set_meters(&[geometry(0)]);
    // Peak in the high zone: the line takes the high colour.
    bridge.show(&[MeterView {
        level: 0.95,
        peak: 0.95,
        clipped: true,
    }]);
    bridge.set_colors(&light);
    let subs = sublayers(&bridge);
    let srgb = |c: rbs_ui::tokens::Color| {
        let ch = |v: u8| f64::from(v) / 255.0;
        CGColor::new_srgb(ch(c.r), ch(c.g), ch(c.b), ch(c.a))
    };
    let is = |l: &CALayer, c: rbs_ui::tokens::Color| {
        CGColor::equal_to_color(l.backgroundColor().as_deref(), Some(&srgb(c)))
    };
    assert!(is(&subs[0], light.low));
    assert!(is(&subs[1], light.mid));
    assert!(is(&subs[2], light.high));
    assert!(is(&subs[3], light.high), "peak line in the high zone");
    assert!(is(&subs[4], light.clip));
}

#[test]
fn horizontal_meters_grow_rightward() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    let g = MeterGeometry {
        bar: Rect {
            x: 10.0,
            y: 10.0,
            w: 200.0,
            h: 6.0,
        },
        clip: Rect {
            x: 212.0,
            y: 10.0,
            w: 6.0,
            h: 6.0,
        },
        orientation: Orientation::Horizontal,
    };
    bridge.set_meters(&[g]);
    let v = MeterView {
        level: 0.5,
        peak: 0.8,
        clipped: false,
    };
    bridge.show(&[v]);
    let subs = sublayers(&bridge);
    let f = v.frames(g.bar, Orientation::Horizontal);
    assert!(close(subs[0].frame(), f.zones[0]));
    assert_eq!(subs[0].frame().size.width, 100.0);
    assert!(close(subs[3].frame(), f.peak.unwrap()));
}

#[test]
fn fewer_views_leave_the_other_meters_alone() {
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.set_meters(&[geometry(0), geometry(1)]);
    let v = MeterView {
        level: 0.5,
        peak: 0.5,
        clipped: false,
    };
    bridge.show(&[v, v]);
    let second = sublayers(&bridge)[PER_METER].frame();
    bridge.show(&[MeterView { level: 0.2, ..v }]);
    assert_eq!(sublayers(&bridge)[PER_METER].frame(), second);
}

#[test]
fn input_that_is_not_finite_never_reaches_core_animation() {
    // Core Animation throws (and aborts the process) on a frame that is not finite.
    let parent = parent(true, 2.0);
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&parent);
    bridge.set_rect(Rect {
        x: f32::NAN,
        y: 0.0,
        w: 10.0,
        h: 10.0,
    });
    let bad = MeterGeometry {
        bar: Rect {
            x: f32::INFINITY,
            y: 0.0,
            w: 8.0,
            h: 40.0,
        },
        clip: Rect {
            x: f32::NAN,
            y: 0.0,
            w: 8.0,
            h: 6.0,
        },
        orientation: Orientation::Vertical,
    };
    bridge.set_meters(&[bad]);
    bridge.show(&[MeterView {
        level: f32::NAN,
        peak: 0.5,
        clipped: true,
    }]);
    // Reaching here is the test; the broken parts are hidden.
    assert!(sublayers(&bridge).iter().all(|l| l.isHidden()));
}

#[test]
fn detach_and_drop_leave_the_parent() {
    let parent = parent(true, 1.0);
    let count = |p: &CALayer| {
        // SAFETY: reading the sublayers of a layer only this thread uses.
        unsafe { p.sublayers() }.map_or(0, |a| a.count())
    };
    let mut bridge = MeterBridgeLayer::new(&colors());
    bridge.attach_to_layer(&parent);
    assert_eq!(count(&parent), 1);
    bridge.detach();
    assert_eq!(count(&parent), 0);
    bridge.attach_to_layer(&parent);
    drop(bridge);
    assert_eq!(count(&parent), 0);
}
