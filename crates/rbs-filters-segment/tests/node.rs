//! The background removal node on the graph runtime with a fake session: readback of
//! the small picture, the matte upload, the composite and the lifecycle (a disabled
//! filter holds no session).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;

use common::{Rig, gpu, lock};
use rbs_core::media::PixelFormat;
use rbs_filters::conformance::Picture;

#[test]
fn it_publishes_nothing_until_a_matte_exists_then_cuts_the_person_out() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    // The first tick has no matte: nothing is published, never the plain picture.
    rig.tick();
    assert!(
        lock(&rig.last).is_none(),
        "the unfiltered picture must not show"
    );
    assert!(rig.until_output(), "no output after a matte");
    let out = rig.output().unwrap();
    assert_eq!((out.width, out.height), (16, 8));
    // Left half is the person: the colour stays; the right half is background.
    let kept = out.at(2, 4);
    assert!((kept[1] - 0.5).abs() < 0.02, "person kept: {kept:?}");
    let cut = out.at(13, 4);
    assert!(
        cut.iter().all(|c| c.abs() < 0.02),
        "background cut: {cut:?}"
    );
    assert!(rig.stats.mattes() >= 1 && rig.stats.frames() >= 1);
}

#[test]
fn the_session_gets_a_small_bgra_picture() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    // Linear (0.25, 0.5, 0.125): sRGB-encoded about (137, 188, 99).
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    let (size, format, px) = lock(&rig.log.last).unwrap();
    assert_eq!((size.width, size.height), (8, 4));
    assert_eq!(format, PixelFormat::Bgra8);
    let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
    assert!((i32::from(b) - 99).abs() <= 3, "blue {b}");
    assert!((i32::from(g) - 188).abs() <= 3, "green {g}");
    assert!((i32::from(r) - 137).abs() <= 3, "red {r}");
    assert_eq!(a, 255);
}

#[test]
fn a_disabled_filter_holds_no_session() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    assert_eq!(rig.log.opened.load(Ordering::Relaxed), 1);
    assert_eq!(rig.log.warmed.load(Ordering::Relaxed), 1);
    assert_eq!(rig.log.closed.load(Ordering::Relaxed), 0);
    rig.disable();
    assert_eq!(rig.log.closed.load(Ordering::Relaxed), 1);
    let _ = &rig.values;
}

#[test]
fn a_size_change_of_the_same_aspect_keeps_the_matte() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    let resets = rig.log.resets.load(Ordering::Relaxed);
    rig.set_input(Picture::solid(32, 16, [0.5, 0.5, 0.5, 1.0]));
    // The layer is never blank because of a size change: the next tick already has a
    // picture of the new size, drawn with the matte from before.
    rig.tick();
    let out = rig.output().expect("a picture at once");
    assert_eq!((out.width, out.height), (32, 16));
    assert_eq!(rig.log.resets.load(Ordering::Relaxed), resets);
    assert!((out.at(4, 8)[1] - 0.5).abs() < 0.02);
}

#[test]
fn a_new_aspect_resets_the_session_and_waits_for_a_new_matte() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    let resets = rig.log.resets.load(Ordering::Relaxed);
    rig.set_input(Picture::solid(8, 16, [0.5, 0.5, 0.5, 1.0]));
    rig.tick();
    assert!(
        lock(&rig.last).is_none(),
        "the old matte does not fit the new aspect"
    );
    assert!(rig.log.resets.load(Ordering::Relaxed) > resets);
    assert!(rig.until_output());
    let out = rig.output().unwrap();
    assert_eq!((out.width, out.height), (8, 16));
}

#[test]
fn exact_mode_draws_the_picture_the_matte_was_made_from() {
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_graph::runtime::Tick;

    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    // A different green level every tick; the matte of an early tick arrives later.
    let level = |n: u64| 0.1 + 0.05 * n as f32;
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    for n in 0..6u64 {
        rig.set_input(Picture::solid(16, 8, [0.0, level(n), 0.0, 1.0]));
        rig.tick();
        rig.ctx.wait_idle().unwrap();
    }
    assert!(rig.until_output());
    let time = lock(&rig.last).as_ref().unwrap().timestamp();
    let made_at = (0..40u64)
        .find(|n| Tick::of(&cadence, *n).time == time)
        .expect("the output carries the time of one of the pictures");
    let out = rig.output().unwrap();
    // The output shows the picture of its own time, not the newest one.
    let g = out.at(2, 4)[1];
    assert!(
        (g - level(made_at)).abs() < 0.02,
        "tick {made_at}: green {g}"
    );
}
