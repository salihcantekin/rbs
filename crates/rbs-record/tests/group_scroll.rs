//! Group Scroll in the studio (ADR-0185, ADR-0187, ADR-0193): one run per canvas,
//! scene, group and settings generation shared by program and preview; a settings edit
//! starts a new run, a member edit keeps the phase; live calls act on the run only; an
//! oversized group is reported; the runs survive a studio restart. Its own test binary.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::time::Clock;
use rbs_record::{
    GroupFailure, GroupFailureReason, GroupOccurrence, ScrollControl, ScrollRefused, Studio,
    StudioSettings,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::{CanvasId, GroupId, GroupScroll, ItemId, Layout, SceneId, ScrollDirection};
use support::*;

/// Groups the display and the camera of [`scene`]'s program.
fn group(col: &mut SceneCollection, canvas: CanvasId) -> (SceneId, GroupId, Vec<ItemId>) {
    let scene = col.canvas(canvas).unwrap().program.unwrap();
    let items: Vec<_> = col.scene(scene).unwrap().items[..2]
        .iter()
        .map(|i| i.id)
        .collect();
    let (group, _) = col.group_items(scene, &items, None).unwrap();
    (scene, group, items)
}

fn set_scroll(col: &mut SceneCollection, at: (SceneId, GroupId, &[ItemId], CanvasId), speed: f32) {
    let mut scroll = GroupScroll::new(ScrollDirection::Left, speed);
    scroll.ramp_ms = 0;
    col.set_group_scroll(at.0, at.1, at.2, at.3, Some(scroll))
        .unwrap();
}

#[test]
fn group_runs_are_shared_versioned_controlled_and_handed_over() {
    let Some(gpu) = gpu().filter(|_| has_preview()) else {
        return;
    };
    let r = rig();
    let (mut col, canvas) = scene();
    let (scene, group, items) = group(&mut col, canvas);
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    settings.scene_preview = true;
    settings.preview = Some(rbs_gpu::share::PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 3,
    });
    let mut studio = Studio::start(
        engine,
        std::sync::Arc::clone(&gpu),
        &col,
        &r.adapters.sources,
        &settings,
    )
    .unwrap();
    // A group without Scroll: no run (zero cost).
    assert_eq!(studio.group_runs().keys().count(), 0);
    assert_eq!(
        studio.group_scroll_control(scene, group, canvas, ScrollControl::Pause),
        Err(ScrollRefused::NoScroll)
    );

    // Program and preview draw the same group with the same settings: one run.
    col.set_preview(canvas, Some(scene)).unwrap();
    set_scroll(&mut col, (scene, group, &items, canvas), 10.0);
    clock.advance(Duration::from_millis(100));
    studio.update(&col, &r.adapters.sources).unwrap();
    let keys: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(keys.len(), 1, "{keys:?}");
    let key = keys[0];
    assert_eq!((key.canvas, key.scene, key.group), (canvas, scene, group));
    assert!(
        studio.scene_preview().is_some(),
        "the preview draws the group too"
    );
    let first = *studio.group_runs().get(&key).unwrap();
    assert_eq!(first.anchor, clock.now());
    let state = studio.group_run(scene, group, canvas).unwrap();
    assert!(!state.paused);
    assert_eq!(state.speed, 10.0);
    assert!(
        studio
            .group_render_failures(scene, group, canvas)
            .is_empty()
    );

    // A member's bounds change: same run, same phase.
    clock.advance(Duration::from_millis(100));
    let cam = col.item(items[1]).unwrap().1.layout;
    col.set_item_layout(items[1], None, Layout::new([1.0, 2.0], cam.size))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.group_runs().get(&key), Some(&first));
    // The compositors draw the span: no acquire or pass fails.
    let mut ticks = 0;
    assert!(run_until(&mut studio, &clock, |_| {
        ticks += 1;
        ticks > 4
    }));
    assert!(
        studio
            .group_render_failures(scene, group, canvas)
            .is_empty()
    );

    // Live calls act on the run, never on the saved settings.
    assert_eq!(
        studio.group_scroll_control(scene, group, canvas, ScrollControl::Pause),
        Ok(())
    );
    let paused = *studio.group_runs().get(&key).unwrap();
    clock.advance(Duration::from_millis(100));
    assert_eq!(
        paused.position_at(clock.now()),
        paused.position_at(clock.now() - Duration::from_millis(100))
    );
    assert!(studio.group_run(scene, group, canvas).unwrap().paused);
    studio
        .group_scroll_control(scene, group, canvas, ScrollControl::Speed(30.0))
        .unwrap();
    assert!(
        studio.group_run(scene, group, canvas).unwrap().paused,
        "speed does not resume"
    );
    studio
        .group_scroll_control(scene, group, canvas, ScrollControl::Resume)
        .unwrap();
    let state = studio.group_run(scene, group, canvas).unwrap();
    assert_eq!((state.paused, state.speed), (false, 30.0));
    assert_eq!(studio.group_runs().get(&key).unwrap().to, first.to * 3.0);
    studio
        .group_scroll_control(scene, group, canvas, ScrollControl::Restart)
        .unwrap();
    let restarted = studio.group_run(scene, group, canvas).unwrap();
    assert_ne!(restarted.id, state.id, "a restart is a new incarnation");
    assert_eq!(restarted.speed, 10.0);
    assert_eq!(studio.group_runs().get(&key).unwrap().anchor, clock.now());
    let saved = col.scene(scene).unwrap().groups[0].scrolls[&canvas];
    assert_eq!(saved.speed, 10.0);

    // A restart hands the runs over, moved to the new clock.
    let handover = studio.scroll_handover();
    let was = studio
        .group_runs()
        .get(&key)
        .unwrap()
        .position_at(handover.at);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    let (engine, clock) = engine_at_start();
    settings.scroll_runs = Some(handover);
    let mut studio = Studio::start(
        engine,
        std::sync::Arc::clone(&gpu),
        &col,
        &r.adapters.sources,
        &settings,
    )
    .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let moved = *studio.group_runs().get(&key).unwrap();
    assert_eq!(moved.position_at(clock.now()), was);

    // A settings edit is a new generation: a new run, the old one ends.
    clock.advance(Duration::from_millis(100));
    set_scroll(&mut col, (scene, group, &items, canvas), 20.0);
    studio.update(&col, &r.adapters.sources).unwrap();
    let keys: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].generation > key.generation);
    assert_eq!(
        studio.group_runs().get(&keys[0]).unwrap().anchor,
        clock.now()
    );

    // Over the per-intermediate limit: no run, reported for program and preview.
    let huge = Layout::new([0.0, 0.0], [4000.0, 4000.0]);
    col.set_item_layout(items[0], None, huge).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.group_runs().keys().count(), 0);
    let failures = studio.group_render_failures(scene, group, canvas);
    let too_large = |occurrence| GroupFailure {
        occurrence,
        reason: GroupFailureReason::TooLarge,
    };
    assert_eq!(
        failures,
        [
            too_large(GroupOccurrence::Program),
            too_large(GroupOccurrence::Preview)
        ]
    );

    // Group Scroll removed: no run, nothing reported.
    col.set_group_scroll(scene, group, &items, canvas, None)
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.group_runs().keys().count(), 0);
    assert!(
        studio
            .group_render_failures(scene, group, canvas)
            .is_empty()
    );
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// A studio with `col`'s program drawn and its preview shown, and a Fade transition.
/// The preview node exists on Windows and Apple platforms only; elsewhere these
/// tests skip like they do without a GPU adapter.
fn has_preview() -> bool {
    cfg!(any(windows, target_vendor = "apple"))
}

fn transition_studio(
    col: &mut SceneCollection,
    canvas: CanvasId,
) -> (Studio, rbs_core::time::TestClock) {
    col.set_transition(rbs_scene::CollectionTransition {
        kind: rbs_scene::TransitionKind::Fade,
        direct: rbs_scene::TransitionKind::Fade,
        ..rbs_scene::CollectionTransition::default()
    })
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    settings.scene_preview = true;
    settings.preview = Some(rbs_gpu::share::PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 3,
    });
    let gpu = gpu().unwrap();
    let r = rig();
    let studio = Studio::start(engine, gpu, col, &r.adapters.sources, &settings).unwrap();
    (studio, clock)
}

#[test]
fn an_outgoing_snapshot_keeps_its_group_run_when_its_scene_is_edited() {
    if gpu().is_none() || !has_preview() {
        return;
    }
    let r = rig();
    let sources = &r.adapters.sources;
    let (mut col, canvas) = scene();
    let (scene, group, items) = group(&mut col, canvas);
    let (next, _) = col
        .add_scene("Next", rbs_scene::model::SceneScope::Shared)
        .unwrap();
    set_scroll(&mut col, (scene, group, &items, canvas), 10.0);
    let (mut studio, clock) = transition_studio(&mut col, canvas);

    // Timed: the outgoing scene is also the preview's; editing it mid-transition gives
    // the preview a new run and leaves the outgoing snapshot on its own.
    col.set_preview(canvas, Some(scene)).unwrap();
    studio.update(&col, sources).unwrap();
    let old: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(old.len(), 1);
    col.set_program(canvas, Some(next)).unwrap();
    studio.update(&col, sources).unwrap();
    assert!(studio.transition_running(canvas));
    clock.advance(Duration::from_millis(10));
    set_scroll(&mut col, (scene, group, &items, canvas), 20.0);
    studio.update(&col, sources).unwrap();
    let keys: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.contains(&old[0]), "the outgoing run is kept");
    assert!(
        studio
            .group_render_failures(scene, group, canvas)
            .is_empty()
    );
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_t_bar_outgoing_program_keeps_the_group_run_it_started_with() {
    if gpu().is_none() || !has_preview() {
        return;
    }
    let r = rig();
    let sources = &r.adapters.sources;
    let (mut col, canvas) = scene();
    let (scene, group, items) = group(&mut col, canvas);
    let (next, _) = col
        .add_scene("Next", rbs_scene::model::SceneScope::Shared)
        .unwrap();
    set_scroll(&mut col, (scene, group, &items, canvas), 10.0);
    col.set_preview(canvas, Some(next)).unwrap();
    let (mut studio, clock) = transition_studio(&mut col, canvas);
    studio.update(&col, sources).unwrap();
    let old: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(old.len(), 1);
    // The first t > 0 captures the program's group settings; a later edit of the live
    // program starts a new run and leaves the outgoing one.
    studio.transition_progress(canvas, 0.4, sources).unwrap();
    clock.advance(Duration::from_millis(10));
    set_scroll(&mut col, (scene, group, &items, canvas), 20.0);
    studio.update(&col, sources).unwrap();
    let keys: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.contains(&old[0]), "the outgoing run is kept");
    // Cancelled: the program alone again, on its live run.
    studio.transition_progress(canvas, 0.0, sources).unwrap();
    studio.update(&col, sources).unwrap();
    let keys: Vec<_> = studio.group_runs().keys().copied().collect();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].generation > old[0].generation);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
