//! Cue isolation at the Studio transition boundary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use rbs_core::media::{ContainerId, Size};
use rbs_core::time::Clock;
use rbs_graph::node::NodeTypeId;
use rbs_record::{ManualStep, OutputSettings, Studio, StudioSettings, TransitionState};
use rbs_scene::activity::{Run, SourceActivity};
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings};
use rbs_scene::{CollectionTransition, SceneCollection, TransitionKind};
use rbs_sources::types;
use support::{FPS, H, W, engine_at_start, gpu, rig};

#[test]
fn manual_wait_freezes_only_outgoing_cues_and_cancel_restores_live_cues() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (program, _) = col.add_scene("Program", SceneScope::Shared).unwrap();
    let (preview, _) = col.add_scene("Preview", SceneScope::Shared).unwrap();
    let mut add = |name| {
        let (source, _) = col
            .add_source(
                name,
                NodeTypeId::new_static(types::CAMERA_SOURCE),
                Settings::new(),
            )
            .unwrap();
        col.set_source_activity(source, SourceActivity::new(Some(Run::Shown), None, None))
            .unwrap();
        let (item, _) = col
            .add_item(
                program,
                ItemContent::Source(source),
                Layout::new([0.0, 0.0], [W as f32, H as f32]),
                None,
            )
            .unwrap();
        col.set_item_visible(item, false).unwrap();
        (source, item)
    };
    let (expired, expired_item) = add("Expired");
    let (late, late_item) = add("Late");
    let (incoming, incoming_item) = add("Incoming");
    let (preview_item, _) = col
        .add_item(
            preview,
            ItemContent::Source(incoming),
            Layout::new([0.0, 0.0], [W as f32, H as f32]),
            None,
        )
        .unwrap();
    col.set_item_visible(preview_item, false).unwrap();
    col.set_program(canvas, Some(program)).unwrap();
    col.set_preview(canvas, Some(preview)).unwrap();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Fade,
        direct: TransitionKind::Fade,
        ..CollectionTransition::default()
    })
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &rig.adapters.sources, &settings).unwrap();
    let mut output = OutputSettings::new("mem://manual-cues", ContainerId::Matroska);
    output.audio = None;
    let output = studio.start_output(&rig.adapters, &output).unwrap();
    let sources = &rig.adapters.sources;
    assert!(studio.show_cue(expired_item, false, Some(1_000)));
    studio.update(&col, sources).unwrap();
    assert_eq!(
        studio.transition_progress(canvas, 0.4, sources).unwrap(),
        ManualStep::Holding
    );
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    clock.advance(Duration::from_secs(1));
    assert!(studio.expire_cues(clock.now()));
    assert!(studio.show_cue(late_item, false, Some(2_000)));
    assert!(studio.show_cue(incoming_item, false, Some(2_000)));
    assert!(studio.show_cue(preview_item, false, Some(2_000)));
    studio.update(&col, sources).unwrap();
    let active = |s: &Studio, id| s.sources().iter().any(|entry| entry.source == id);
    assert!(active(&studio, expired));
    assert!(!active(&studio, late));
    assert!(active(&studio, incoming));

    col.set_item_visible(late_item, true).unwrap();
    studio.update(&col, sources).unwrap();
    assert!(active(&studio, late));
    col.set_item_visible(late_item, false).unwrap();
    assert!(studio.show_cue(late_item, false, Some(2_000)));
    studio.update(&col, sources).unwrap();
    assert_eq!(
        studio.transition_progress(canvas, 0.0, sources).unwrap(),
        ManualStep::Idle
    );
    assert!(!active(&studio, expired));
    assert!(active(&studio, late));
    studio.stop_output(output).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn timed_transition_keeps_its_outgoing_cued_picture() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (program, _) = col.add_scene("Program", SceneScope::Shared).unwrap();
    let (next, _) = col.add_scene("Next", SceneScope::Shared).unwrap();
    let (source, _) = col
        .add_source(
            "Outgoing",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.set_source_activity(source, SourceActivity::new(Some(Run::Shown), None, None))
        .unwrap();
    let (item, _) = col
        .add_item(
            program,
            ItemContent::Source(source),
            Layout::new([0.0, 0.0], [W as f32, H as f32]),
            None,
        )
        .unwrap();
    col.set_item_visible(item, false).unwrap();
    col.set_program(canvas, Some(program)).unwrap();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Fade,
        direct: TransitionKind::Fade,
        ..CollectionTransition::default()
    })
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &rig.adapters.sources, &settings).unwrap();
    let mut output = OutputSettings::new("mem://timed-cues", ContainerId::Matroska);
    output.audio = None;
    let output = studio.start_output(&rig.adapters, &output).unwrap();
    assert!(studio.show_cue(item, false, Some(1_000)));
    studio.update(&col, &rig.adapters.sources).unwrap();
    col.set_program(canvas, Some(next)).unwrap();
    studio.update(&col, &rig.adapters.sources).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    clock.advance(Duration::from_secs(1));
    assert!(studio.expire_cues(clock.now()));
    studio.update(&col, &rig.adapters.sources).unwrap();
    assert!(studio.sources().iter().any(|entry| entry.source == source));
    studio.stop_output(output).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn preview_cue_moved_to_program_after_manual_capture_stays_outgoing_hidden() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (program, _) = col.add_scene("Program", SceneScope::Shared).unwrap();
    let (preview, _) = col.add_scene("Preview", SceneScope::Shared).unwrap();
    let (global, _) = col.add_scene("Global", SceneScope::Canvas(canvas)).unwrap();
    col.set_globals(canvas, Some(global)).unwrap();
    let mut add = |scene, name| {
        let (source, _) = col
            .add_source(
                name,
                NodeTypeId::new_static(types::CAMERA_SOURCE),
                Settings::new(),
            )
            .unwrap();
        col.set_source_activity(source, SourceActivity::new(Some(Run::Shown), None, None))
            .unwrap();
        let (item, _) = col
            .add_item(
                scene,
                ItemContent::Source(source),
                Layout::new([0.0, 0.0], [W as f32, H as f32]),
                None,
            )
            .unwrap();
        col.set_item_visible(item, false).unwrap();
        (source, item)
    };
    let (outgoing, outgoing_item) = add(program, "Outgoing");
    let (global_source, global_item) = add(global, "Global");
    let (moved, moved_item) = add(preview, "Moved");
    let (incoming, incoming_item) = add(preview, "Incoming");
    col.set_program(canvas, Some(program)).unwrap();
    col.set_preview(canvas, Some(preview)).unwrap();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Fade,
        direct: TransitionKind::Fade,
        ..CollectionTransition::default()
    })
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &rig.adapters.sources, &settings).unwrap();
    let mut output = OutputSettings::new("mem://cue-placement", ContainerId::Matroska);
    output.audio = None;
    let output = studio.start_output(&rig.adapters, &output).unwrap();
    for item in [outgoing_item, global_item, moved_item] {
        assert!(studio.show_cue(item, false, Some(1_000)));
    }
    studio.update(&col, &rig.adapters.sources).unwrap();
    studio
        .transition_progress(canvas, 0.4, &rig.adapters.sources)
        .unwrap();
    let item = col.item(moved_item).unwrap().1.clone();
    let mut preview_items = col.scene(preview).unwrap().items.clone();
    preview_items.retain(|item| item.id != moved_item);
    col.set_scene_items(preview, preview_items).unwrap();
    let mut program_items = col.scene(program).unwrap().items.clone();
    program_items.push(item);
    col.set_scene_items(program, program_items).unwrap();
    clock.advance(Duration::from_secs(1));
    assert!(studio.expire_cues(clock.now()));
    assert!(studio.show_cue(incoming_item, false, Some(1_000)));
    studio.update(&col, &rig.adapters.sources).unwrap();
    let active = |id| studio.sources().iter().any(|entry| entry.source == id);
    assert!(active(outgoing));
    assert!(active(global_source));
    assert!(!active(moved));
    assert!(active(incoming));
    studio.stop_output(output).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
