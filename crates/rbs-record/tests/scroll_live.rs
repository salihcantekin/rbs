//! Live scroll calls in the studio (ADR-0150 decision 11, spec `control-api.md`,
//! "Protocol 1.94"): pause, resume, speed and restart act on the item's run without an
//! edit; an item without a scroll is refused; a `Once` run's end is reported once, by
//! the control loop's service, on the test clock. Its own test binary, apart from
//! `scroll.rs`, which checks that the device has seen nothing scroll before it.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::media::ContainerId;
use rbs_core::time::Clock;
use rbs_record::{OutputSettings, ScrollControl, ScrollRefused, Studio, StudioSettings};
use rbs_scene::model::{Scroll, ScrollDirection, ScrollRepeat, ScrollStart};
use support::*;

#[test]
fn live_calls_move_the_run_and_a_once_scroll_reports_its_end_once() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        std::sync::Arc::clone(&gpu),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    // A recording makes the plan schedule the compositor and its sources.
    let output = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://scroll-live", ContainerId::Matroska),
        )
        .unwrap();
    let items = col.scene(live).unwrap().items.clone();
    let (display_item, cam_item) = (items[0].id, items[1].id);
    let cam = col.item(cam_item).unwrap().1.layout;

    // No scroll on the item: refused before any run is looked at.
    assert_eq!(
        studio.scroll_control(display_item, canvas, ScrollControl::Pause),
        Err(ScrollRefused::NoScroll)
    );

    let ticker = Scroll::new(ScrollDirection::Left, 20.0).with_ramp_ms(0);
    col.set_item_layout(cam_item, None, cam.with_scroll(Some(ticker)))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let key = *studio.scroll_runs().keys().next().unwrap();

    // Pause stops it at once (no ramp), resume and a live speed move it again; none of
    // them is an edit, so the collection keeps the saved speed.
    clock.advance(Duration::from_millis(100));
    assert_eq!(
        studio.scroll_control(cam_item, canvas, ScrollControl::Pause),
        Ok(1)
    );
    assert!(studio.scroll_runs().is_paused(&key));
    let stopped = studio
        .scroll_runs()
        .get(&key)
        .unwrap()
        .position_at(clock.now());
    clock.advance(Duration::from_millis(100));
    let run = *studio.scroll_runs().get(&key).unwrap();
    assert_eq!(run.position_at(clock.now()), stopped);
    assert_eq!(
        studio.scroll_control(cam_item, canvas, ScrollControl::Speed(40.0)),
        Ok(1)
    );
    assert!(
        studio.scroll_runs().is_paused(&key),
        "speed does not resume"
    );
    assert_eq!(
        studio.scroll_control(cam_item, canvas, ScrollControl::Resume),
        Ok(1)
    );
    assert!(!studio.scroll_runs().is_paused(&key));
    let run = *studio.scroll_runs().get(&key).unwrap();
    assert_eq!(
        run.to,
        f64::from(key.speed()) * 2.0,
        "the live speed, scaled"
    );
    assert_eq!(
        col.item(cam_item).unwrap().1.layout.scroll.unwrap().speed,
        20.0
    );
    clock.advance(Duration::from_millis(100));
    assert_eq!(
        studio.scroll_control(cam_item, canvas, ScrollControl::Restart),
        Ok(1)
    );
    let run = *studio.scroll_runs().get(&key).unwrap();
    assert_eq!(run.anchor, clock.now());
    assert_eq!(run.position_at(clock.now()), 0.0);

    // A fast `Once` scroll from outside: the control loop's service finds its picture
    // gone and reports the items that used the run, once.
    let once = Scroll::new(ScrollDirection::Up, 4000.0)
        .with_repeat(ScrollRepeat::Once)
        .with_start(ScrollStart::Outside)
        .with_ramp_ms(0);
    col.set_item_layout(cam_item, None, cam.with_scroll(Some(once)))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let key = *studio.scroll_runs().keys().next().unwrap();
    assert!(studio.take_scroll_finished().is_empty());
    let ended = run_until(&mut studio, &clock, |s| s.scroll_runs().is_finished(&key));
    assert!(ended, "the once scroll ends");
    let finished = studio.take_scroll_finished();
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0].canvas, canvas);
    assert_eq!(finished[0].items, vec![cam_item]);
    clock.advance(Duration::from_millis(100));
    studio.service();
    assert!(studio.take_scroll_finished().is_empty(), "reported once");
    // A restart starts it again, and its end is reported again.
    assert_eq!(
        studio.scroll_control(cam_item, canvas, ScrollControl::Restart),
        Ok(1)
    );
    assert!(!studio.scroll_runs().is_finished(&key));
    let ended = run_until(&mut studio, &clock, |s| s.scroll_runs().is_finished(&key));
    assert!(ended, "the restarted scroll ends");
    assert_eq!(studio.take_scroll_finished().len(), 1);

    studio.stop_output(output).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
