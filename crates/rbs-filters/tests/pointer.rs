//! The pointer host service (spec `video-filters.md`, "Host service: pointer";
//! ADR-0165 decision 3) on a test pointer type, a scripted input backend and a mapping
//! cell. Ticks are driven on the calling thread at 60 fps from time 0, and every input
//! event is stamped by the test, so nothing depends on real time (ADR-0032). Skips
//! without a GPU adapter.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Test code.

mod common;

use std::sync::Arc;

use rbs_capture::{Cover, MappingCell};
use rbs_core::media::PixelFormat;
use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{FilterType, PointerFeed, Values};
use rbs_input::InputMonitor;

use common::pointer::{LIFE_NS, RingType, Scripted, mapping};

struct Setup {
    ty: RingType,
    rig: Rig,
    script: Scripted,
    monitor: InputMonitor,
    cell: MappingCell,
}

fn setup(fail_start: bool) -> Option<Setup> {
    let ctx = common::gpu()?;
    let script = Scripted::default();
    script.0.lock().unwrap().1 = fail_start;
    let monitor = InputMonitor::new(Box::new(script.clone()));
    let cell = MappingCell::new();
    cell.publish(Some(mapping(Cover::Clear)));
    let ring = RingType::default();
    let ty: Arc<dyn FilterType> = Arc::new(ring.clone());
    let values = Values::defaults(ty.as_ref());
    let feed = PointerFeed::new(monitor.clone(), cell.clone());
    let mut rig = Rig::with_pointer(ctx, ty, values, Some([8, 8]), feed).unwrap();
    // An sRGB-encoded input, so a passed-through frame is told apart by its format.
    rig.set_input(Picture::solid(8, 8, [0.0, 0.5, 0.0, 1.0]).encoded());
    // The first tick starts the schedule; frames flow from the second.
    rig.tick();
    Some(Setup {
        ty: ring,
        rig,
        script,
        monitor,
        cell,
    })
}

fn format(rig: &Rig) -> Option<PixelFormat> {
    rig.output_format().map(|f| f.pixel_format)
}

/// Ticks until the tick's time is past `ns` (60 fps).
fn tick_past(rig: &mut Rig, ns: u64) {
    for _ in 0..100 {
        rig.tick();
        if rig.last_time().as_nanos() > ns {
            return;
        }
    }
    panic!("never reached {ns} ns");
}

#[test]
fn idle_passes_the_input_through_and_a_click_draws() {
    let Some(Setup {
        ty,
        mut rig,
        script,
        monitor,
        ..
    }) = setup(false)
    else {
        return;
    };
    assert!(monitor.is_running(), "subscribed in prepare");
    rig.tick();
    assert_eq!(
        format(&rig),
        Some(PixelFormat::Rgba8),
        "idle: the input itself"
    );
    let idle_key = rig.output_key();
    rig.tick();
    assert_eq!(
        rig.output_key(),
        idle_key,
        "an unchanged idle input is not republished"
    );

    let at = rig.last_time().as_nanos() + 1;
    script.click(at, 102.0, 53.0);
    script.point(at, 104.0, 55.0);
    tick_past(&mut rig, at);
    assert_eq!(
        format(&rig),
        Some(PixelFormat::Rgba16Float),
        "drawing: the type's pass"
    );
    let (picture, _, _) = rig.output().expect("a drawn frame");
    assert_eq!(picture.at(0, 0), [1.0, 0.0, 0.0, 1.0]);
    let s = ty.seen();
    assert_eq!(s.clicks, 1);
    assert_eq!(s.first, Some([2.0, 3.0]), "in picture pixels");
    assert_eq!(s.pointer, Some([4.0, 5.0]));

    // A still input animates: each live tick is a new picture.
    let key = rig.output_key();
    rig.tick();
    assert_ne!(
        rig.output_key(),
        key,
        "a live effect redraws an unchanged input"
    );

    // After the ring's life the input goes out once more, then the node idles.
    tick_past(&mut rig, at + LIFE_NS);
    assert_eq!(
        format(&rig),
        Some(PixelFormat::Rgba8),
        "the last frame is the input"
    );
    let key = rig.output_key();
    rig.tick();
    assert_eq!(rig.output_key(), key);

    rig.disable_filter().unwrap();
    assert!(!monitor.is_running(), "unsubscribed in release");
}

#[test]
fn clicks_outside_the_picture_or_over_a_covering_window_are_dropped() {
    let Some(Setup {
        ty,
        mut rig,
        script,
        cell,
        ..
    }) = setup(false)
    else {
        return;
    };
    rig.tick();
    let at = rig.last_time().as_nanos() + 1;
    script.click(at, 10.0, 10.0);
    tick_past(&mut rig, at);
    assert_eq!(ty.seen().clicks, 0, "outside the picture");
    assert_eq!(format(&rig), Some(PixelFormat::Rgba8));

    cell.publish(Some(mapping(Cover::Covered)));
    let at = rig.last_time().as_nanos() + 1;
    script.click(at, 102.0, 53.0);
    script.point(at, 102.0, 53.0);
    tick_past(&mut rig, at);
    let s = ty.seen();
    assert_eq!((s.clicks, s.pointer), (0, None), "covered");
    assert_eq!(format(&rig), Some(PixelFormat::Rgba8));
}

#[test]
fn a_failed_subscription_is_counted_and_the_node_passes_through() {
    let Some(Setup {
        mut rig, monitor, ..
    }) = setup(true)
    else {
        return;
    };
    assert!(!monitor.is_running());
    assert_eq!(rig.stats.pointer_faults(), 1);
    rig.tick();
    assert_eq!(format(&rig), Some(PixelFormat::Rgba8));
    assert_eq!(rig.filter_health().map(|h| h.0), Some(0), "not an error");
}

#[test]
fn without_a_feed_a_pointer_type_passes_through() {
    let Some(ctx) = common::gpu() else {
        return;
    };
    let ty: Arc<dyn FilterType> = Arc::new(RingType::default());
    let values = Values::defaults(ty.as_ref());
    let mut rig = Rig::new(ctx, ty, values, Some([8, 8])).unwrap();
    rig.set_input(Picture::solid(8, 8, [0.0, 0.5, 0.0, 1.0]).encoded());
    rig.tick();
    rig.tick();
    assert_eq!(format(&rig), Some(PixelFormat::Rgba8));
    assert_eq!(rig.stats.pointer_faults(), 0);
}
