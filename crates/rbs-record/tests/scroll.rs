//! Scrolling layers in the studio (ADR-0150 decisions 5 and 7, spec `scene-sources.md`,
//! "Scrolling layers"): one run per key across program scenes, runs kept while an item
//! is faded out, ended when no list draws them, carried across a studio restart; the
//! wrap pipelines are built only once something scrolls. Its own test binary, so the
//! device has seen nothing scroll before it.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::media::ContainerId;
use rbs_core::time::Clock;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::model::{ItemContent, Layout, SceneScope, Scroll, ScrollDirection};
use support::*;

#[test]
fn scroll_runs_follow_the_lists_and_survive_a_restart() {
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
            &OutputSettings::new("mem://scroll", ContainerId::Matroska),
        )
        .unwrap();
    // Nothing scrolls: no run and no wrap pipelines on the device (P13).
    assert!(studio.scroll_runs().is_empty());
    assert!(!gpu.scrolling_ready());

    // The camera's item scrolls: one run, anchored now, and the device gets its wrap
    // pipelines before the list is sent.
    let cam_item = col.scene(live).unwrap().items[1].id;
    let cam = col.item(cam_item).unwrap().1.clone();
    let ticker = Scroll::new(ScrollDirection::Left, 20.0).with_gap(4.0);
    let bar = cam.layout.with_scroll(Some(ticker));
    col.set_item_layout(cam_item, None, bar).unwrap();
    clock.advance(Duration::from_millis(100));
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.scroll_runs().len(), 1);
    assert!(gpu.scrolling_ready());
    let key = *studio.scroll_runs().keys().next().unwrap();
    let run = *studio.scroll_runs().get(&key).unwrap();
    assert_eq!(run.anchor, clock.now());

    // Another scene with the camera at the same size and scroll: a scene change keeps
    // the run, so the ticker does not jump.
    let ItemContent::Source(cam_source) = cam.content else {
        panic!("the camera item places a source")
    };
    let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
    col.add_item(
        other,
        ItemContent::Source(cam_source),
        Layout::new([0.0, 0.0], bar.size).with_scroll(Some(ticker)),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(other)).unwrap();
    clock.advance(Duration::from_millis(100));
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.scroll_runs().get(&key), Some(&run));

    // Faded to 0, it is in no list, but its run goes on.
    let other_item = col.scene(other).unwrap().items[0].id;
    let faded = Layout::new([0.0, 0.0], bar.size)
        .with_scroll(Some(ticker))
        .with_opacity(0.0);
    col.set_item_layout(other_item, None, faded).unwrap();
    clock.advance(Duration::from_millis(100));
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.scroll_runs().get(&key), Some(&run));

    studio.stop_output(output).unwrap();
    // A restart hands the runs over: the new studio's clock is behind the old one, and
    // the position at its start equals the old position at the handover.
    let handover = studio.scroll_handover();
    let was = run.position_at(handover.at);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.scroll_runs = Some(handover);
    let mut studio = Studio::start(
        engine,
        std::sync::Arc::clone(&gpu),
        &col,
        &r.adapters.sources,
        &settings,
    )
    .unwrap();
    let moved = *studio.scroll_runs().get(&key).unwrap();
    assert_eq!(moved.position_at(clock.now()), was);

    // No list draws the key any more: the run ends.
    col.set_item_layout(other_item, None, Layout::new([0.0, 0.0], bar.size))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.scroll_runs().is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
