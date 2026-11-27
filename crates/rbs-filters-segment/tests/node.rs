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
fn it_keeps_the_plain_picture_until_the_first_matte_then_cuts_the_person_out() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    // Just turned on, no matte yet: the picture that was on screen before stays, so the
    // layer does not vanish (ADR-0199).
    // (The schedule starts on the first tick, the filter runs from the second.)
    rig.tick();
    rig.tick();
    let plain = rig.output().expect("the plain picture stays");
    assert!(
        (plain.at(13, 4)[1] - 0.5).abs() < 0.02,
        "plain: {:?}",
        plain.at(13, 4)
    );
    assert_eq!(rig.stats.frames(), 0);
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
fn the_person_cross_fades_in_from_the_plain_picture() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_first_output());
    // The first frame with a matte is one tick (1/60 s) into the 0.2 s cross-fade from
    // the plain picture: the room is still almost all there, the person is whole.
    let first = rig.output().unwrap();
    let room = first.at(13, 4);
    assert!(room[3] > 0.8 && room[3] < 1.0, "room fades out: {room:?}");
    assert!((first.at(2, 4)[1] - 0.5).abs() < 0.02, "person kept");
    // The picture does not change, but the fade runs on the master clock.
    rig.finish_fade();
    let done = rig.output().unwrap();
    assert!(
        done.at(13, 4).iter().all(|c| c.abs() < 0.02),
        "room cut out"
    );
    assert!((done.at(2, 4)[3] - 1.0).abs() < 0.02, "person opaque");
}

#[test]
fn the_plain_picture_is_kept_half_a_second_at_most() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.log.hold.store(true, Ordering::Relaxed);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    // The schedule starts on the first tick, the filter runs from the second. 0.5 s at
    // 60 fps is 30 ticks: the plain picture until then, nothing after.
    rig.tick();
    for _ in 0..30 {
        rig.tick();
        assert!(
            lock(&rig.last).is_some(),
            "tick {}: plain picture",
            rig.tick
        );
    }
    rig.tick();
    assert!(
        lock(&rig.last).is_none(),
        "a slow session shows nothing, not the room"
    );
    // When the matte comes, the person fades in from nothing.
    rig.log.hold.store(false, Ordering::Relaxed);
    assert!(rig.until_first_output());
    let room = rig.output().unwrap().at(13, 4);
    assert!(room.iter().all(|c| c.abs() < 0.02), "no room: {room:?}");
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
    assert!(rig.until_first_output());
    let time = lock(&rig.last).as_ref().unwrap().timestamp();
    let made_at = (0..40u64)
        .find(|n| Tick::of(&cadence, *n).time == time)
        .expect("the output carries the time of one of the pictures");
    let out = rig.output().unwrap();
    // The output shows the picture of its own time, not the newest one (premultiplied:
    // the fade-in has only begun).
    let px = out.at(2, 4);
    let g = px[1] / px[3];
    assert!(
        (g - level(made_at)).abs() < 0.02,
        "tick {made_at}: green {g}"
    );
}

/// Linear light of a limited-range BT.709 grey `y` (sRGB-encoded transfer).
fn grey_linear(y: u8) -> f32 {
    let e = ((f32::from(y) - 16.0) / 219.0).clamp(0.0, 1.0);
    if e <= 0.04045 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

#[test]
fn exact_mode_works_on_a_4_2_0_camera_picture() {
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_graph::runtime::Tick;

    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    // A camera that delivers NV12 (the MacBook camera does): a different grey every
    // tick. The matte of an early tick arrives later; its own picture must be drawn.
    let grey = |n: u64| 60 + 20 * n as u8;
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    for n in 0..6u64 {
        rig.set_nv12(16, 8, [grey(n), 128, 128]);
        rig.tick();
        rig.ctx.wait_idle().unwrap();
    }
    assert!(rig.until_first_output());
    let time = lock(&rig.last).as_ref().unwrap().timestamp();
    let made_at = (0..40u64)
        .find(|n| Tick::of(&cadence, *n).time == time)
        .expect("the output carries the time of one of the pictures");
    let out = rig.output().unwrap();
    let px = out.at(2, 4);
    let g = px[1] / px[3];
    assert!(
        (g - grey_linear(grey(made_at))).abs() < 0.03,
        "tick {made_at}: {g} against {}",
        grey_linear(grey(made_at))
    );
    assert!(
        made_at < 5,
        "tick {made_at}: the newest picture was drawn, not the one the matte came from"
    );
}

#[test]
fn a_colour_background_fills_where_the_person_is_not() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_values(&[
        ("background", rbs_params::Value::Choice(2)),
        (
            "background_colour",
            rbs_params::Value::Color([255, 0, 0, 255]),
        ),
    ]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    let out = rig.output().unwrap();
    let kept = out.at(2, 4);
    assert!((kept[1] - 0.5).abs() < 0.02, "person kept: {kept:?}");
    let room = out.at(13, 4);
    assert!(
        room[0] > 0.9 && room[1] < 0.02 && room[3] > 0.9,
        "red behind: {room:?}"
    );
}

#[test]
fn a_blurred_background_is_the_room_not_the_person() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_values(&[("background", rbs_params::Value::Choice(1))]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    let out = rig.output().unwrap();
    let room = out.at(13, 4);
    // The room is the same flat colour, so the blur of it is that colour, and opaque.
    assert!(
        (room[1] - 0.5).abs() < 0.05 && room[3] > 0.9,
        "blur: {room:?}"
    );
}

#[test]
fn a_join_that_is_not_a_turn_on_shows_nothing_until_the_first_matte() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    assert!(rig.until_output());
    // Left and joined again without a turn-on (a scene switch, a restart): the room was
    // not on screen, so it must not show before the matte.
    rig.disable();
    let frames = rig.stats.frames();
    rig.enable(false);
    for _ in 0..3 {
        rig.tick();
        rig.ctx.wait_idle().unwrap();
        if rig.stats.frames() > frames {
            break;
        }
        assert!(lock(&rig.last).is_none(), "the plain picture must not show");
    }
}

#[test]
fn a_size_change_before_the_first_matte_ends_the_plain_picture() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx, [8, 4]);
    rig.log.hold.store(true, Ordering::Relaxed);
    rig.set_input(Picture::solid(16, 8, [0.25, 0.5, 0.125, 1.0]));
    rig.tick();
    rig.tick();
    assert!(
        lock(&rig.last).is_some(),
        "plain picture right after the turn-on"
    );
    rig.set_input(Picture::solid(32, 16, [0.25, 0.5, 0.125, 1.0]));
    rig.tick();
    assert!(
        lock(&rig.last).is_none(),
        "a size change never shows the plain input"
    );
}
