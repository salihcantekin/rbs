use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rbs_core::media::{ContainerId, Size};
use rbs_core::time::Clock;
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::activity::{Run, SourceActivity};
use rbs_scene::model::{ItemContent, ItemId, Layout, SceneScope, Settings};
use rbs_sources::types;

use crate::support::{FPS, H, W, engine_at_start, gpu, rig, scene};

#[test]
fn held_cues_have_independent_deadlines_and_wakes() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let (collection, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &collection,
        &rig.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let original = ItemId::from_raw(1);
    let held = ItemId::from_raw(100);

    assert!(studio.show_cue(original, false, Some(2_000)));
    let snapshot = studio
        .capture_held_cues(&BTreeMap::from([(original, held)]))
        .unwrap();
    assert!(studio.replace_held_cues(Some(snapshot)).is_none());
    assert!(studio.drop_cue(original));
    assert_eq!(studio.cue_wait(clock.now()), Some(Duration::from_secs(2)));
    assert_eq!(studio.capture_outgoing_cues(), BTreeSet::from([held]));

    clock.advance(Duration::from_millis(500));
    assert!(studio.show_cue(original, false, Some(3_000)));
    assert_eq!(
        studio.capture_outgoing_cues(),
        BTreeSet::from([original, held])
    );
    assert_eq!(
        studio.cue_wait(clock.now()),
        Some(Duration::from_millis(1_500))
    );
    clock.advance(Duration::from_millis(1_500));
    assert!(studio.expire_cues(clock.now()));
    assert_eq!(studio.capture_outgoing_cues(), BTreeSet::from([original]));
    assert_eq!(
        studio.cue_wait(clock.now()),
        Some(Duration::from_millis(1_500))
    );

    assert!(studio.replace_held_cues(None).is_some());
    assert!(studio.drop_cue(original));
    assert_eq!(studio.cue_wait(clock.now()), None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn held_cue_is_used_by_the_running_program_binder() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let mut collection = SceneCollection::new();
    let (canvas, _) = collection.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (preview, _) = collection.add_scene("Preview", SceneScope::Shared).unwrap();
    let (held_scene, _) = collection.add_scene("Held", SceneScope::Shared).unwrap();
    let (source, _) = collection
        .add_source(
            "Shown camera",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    collection
        .set_source_activity(source, SourceActivity::new(Some(Run::Shown), None, None))
        .unwrap();
    let layout = Layout::new([0.0, 0.0], [W as f32, H as f32]);
    let (original, _) = collection
        .add_item(preview, ItemContent::Source(source), layout, None)
        .unwrap();
    let (held, _) = collection
        .add_item(held_scene, ItemContent::Source(source), layout, None)
        .unwrap();
    collection.set_item_visible(original, false).unwrap();
    collection.set_item_visible(held, false).unwrap();
    collection.set_program(canvas, Some(preview)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio =
        Studio::start(engine, gpu, &collection, &rig.adapters.sources, &settings).unwrap();
    let mut output = OutputSettings::new("mem://held-cue", ContainerId::Matroska);
    output.audio = None;
    let output = studio.start_output(&rig.adapters, &output).unwrap();

    assert!(studio.show_cue(original, false, Some(2_000)));
    studio.update(&collection, &rig.adapters.sources).unwrap();
    let snapshot = studio
        .capture_held_cues(&BTreeMap::from([(original, held)]))
        .unwrap();
    studio.replace_held_cues(Some(snapshot));
    collection.set_program(canvas, Some(held_scene)).unwrap();
    collection.set_preview(canvas, Some(preview)).unwrap();
    assert!(studio.drop_cue(original));
    studio
        .update_same_picture(&collection, &rig.adapters.sources)
        .unwrap();
    assert!(studio.sources().iter().any(|entry| entry.source == source));

    clock.advance(Duration::from_secs(2));
    assert!(studio.expire_cues(clock.now()));
    studio.update(&collection, &rig.adapters.sources).unwrap();
    assert!(!studio.sources().iter().any(|entry| entry.source == source));
    studio.stop_output(output).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn restart_binds_rebased_held_audio_and_video_before_the_first_update() {
    let Some(gpu) = gpu() else { return };
    let rig = rig();
    let mut collection = SceneCollection::new();
    let (canvas, _) = collection.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (preview, _) = collection.add_scene("Preview", SceneScope::Shared).unwrap();
    let (held_scene, _) = collection.add_scene("Held", SceneScope::Shared).unwrap();
    let (source, _) = collection
        .add_source(
            "Mic",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    let (picture, _) = collection
        .add_source(
            "Camera",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    collection
        .set_source_activity(picture, SourceActivity::new(Some(Run::Shown), None, None))
        .unwrap();
    let layout = Layout::new([0.0, 0.0], [W as f32, H as f32]);
    let (original, _) = collection
        .add_item(preview, ItemContent::Source(source), layout, None)
        .unwrap();
    let (held, _) = collection
        .add_item(held_scene, ItemContent::Source(source), layout, None)
        .unwrap();
    let (original_picture, _) = collection
        .add_item(preview, ItemContent::Source(picture), layout, None)
        .unwrap();
    let (held_picture, _) = collection
        .add_item(held_scene, ItemContent::Source(picture), layout, None)
        .unwrap();
    collection.set_item_visible(original, false).unwrap();
    collection.set_item_visible(held, false).unwrap();
    collection
        .set_item_visible(original_picture, false)
        .unwrap();
    collection.set_item_visible(held_picture, false).unwrap();
    collection.set_program(canvas, Some(preview)).unwrap();
    let (engine, old_clock) = engine_at_start();
    let mut old = Studio::start(
        engine,
        gpu.clone(),
        &collection,
        &rig.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert!(old.show_cue(original, false, Some(1_000)));
    assert!(old.show_cue(original_picture, false, Some(1_000)));
    let cues = old
        .capture_held_cues(&BTreeMap::from([
            (original, held),
            (original_picture, held_picture),
        ]))
        .unwrap();
    old.replace_held_cues(Some(cues));
    old_clock.advance(Duration::from_nanos(1));
    let old_now = old_clock.now();

    collection.set_program(canvas, Some(held_scene)).unwrap();
    let (engine, new_clock) = engine_at_start();
    new_clock.advance(Duration::from_secs(10));
    let mut settings = StudioSettings::new(canvas);
    settings.held_cues = old
        .held_cues()
        .map(|cues| cues.rebased(old_now, new_clock.now()));
    let mut restarted =
        Studio::start(engine, gpu, &collection, &rig.adapters.sources, &settings).unwrap();
    assert_eq!(
        restarted.cue_wait(new_clock.now()),
        Some(Duration::from_nanos(999_999_999))
    );
    assert!(
        restarted
            .sources()
            .iter()
            .any(|entry| entry.source == source)
    );
    let mut output = OutputSettings::new("mem://held-restart", ContainerId::Matroska);
    output.audio = None;
    let output = restarted.start_output(&rig.adapters, &output).unwrap();
    assert!(
        restarted
            .sources()
            .iter()
            .any(|entry| entry.source == picture)
    );

    new_clock.advance(Duration::from_nanos(999_999_999));
    assert!(restarted.expire_cues(new_clock.now()));
    restarted
        .update(&collection, &rig.adapters.sources)
        .unwrap();
    assert!(
        !restarted
            .sources()
            .iter()
            .any(|entry| entry.source == source || entry.source == picture)
    );
    restarted.stop_output(output).unwrap();
    let (_, errors) = restarted.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    let (_, errors) = old.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
