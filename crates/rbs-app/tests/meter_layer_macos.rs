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
use objc2_quartz_core::CALayer;
use rbs_app::native::{MeterBridgeLayer, MeterColors, MeterGeometry};
use rbs_ui::layout::Rect;
use rbs_ui::meter::{MeterView, Orientation, position};
use rbs_ui::tokens::Tokens;

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
    subs[0].setFrame(moved);
    bridge.show(&[v]);
    assert_eq!(subs[0].frame(), moved);
    // A changed view redraws it.
    bridge.show(&[MeterView { level: 0.4, ..v }]);
    assert_ne!(subs[0].frame(), moved);
}

#[test]
fn placement_converts_to_the_parent_origin() {
    let parent = CALayer::new();
    parent.setFrame(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(800.0, 400.0),
    ));
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
    parent.setGeometryFlipped(true);
    bridge.set_rect(rect);
    assert_eq!(bridge.layer().frame().origin.y, 300.0);
    assert!(!bridge.layer().isGeometryFlipped());
}

#[test]
fn edges_snap_to_device_pixels() {
    let parent = CALayer::new();
    parent.setFrame(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(800.0, 400.0),
    ));
    parent.setGeometryFlipped(true);
    parent.setContentsScale(2.0);
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
