#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_core::time::TestClock;
use rbs_engine::EngineConfig;
use std::cell::Cell;
use std::collections::BTreeSet;

use super::*;

fn cue_host() -> Option<(Host, Arc<TestClock>, ItemId)> {
    let clock = Arc::new(TestClock::new());
    let host = publish_cost::host_with_clock(2, Arc::clone(&clock))?;
    let (host, item) = prepare_cue(host);
    Some((host, clock, item))
}

fn prepare_cue(mut host: Host) -> (Host, ItemId) {
    let item = host.collection.scene(host.scene).unwrap().items[1].id;
    host.set_item_visible(item, false, 0).unwrap();
    host.set_item_show_for(item, Some(2_000)).unwrap();
    host.apply(&|_| {});
    assert!(host.show_item(item).unwrap());
    host.apply(&|_| {});
    (host, item)
}

fn cue_host_with_clocks(
    clocks: &[Arc<TestClock>],
    failed_replacement: Option<usize>,
) -> Option<(Host, ItemId)> {
    let clocks = clocks.to_vec();
    let index = Cell::new(0);
    let host = publish_cost::host_with_engine(2, move || {
        let at = index.get().min(clocks.len() - 1);
        index.set(at + 1);
        let mut engine = Engine::with_clock(
            Arc::clone(&clocks[at]) as Arc<dyn Clock>,
            // Hang guard for a loaded test runner.
            EngineConfig::default().swap_timeout(Duration::from_secs(20)),
        );
        if failed_replacement == Some(at) {
            engine
                .add_partition::<()>(
                    rbs_record::RENDER,
                    Arc::new(()),
                    (),
                    rbs_engine::Pacing::output(),
                )
                .unwrap();
        }
        engine
    })?;
    Some(prepare_cue(host))
}

#[test]
fn held_cues_survive_preview_changes_and_new_take_captures_once() {
    let Some((mut host, clock, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let first = host.program.held.as_ref().unwrap().cue_items[&item];
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, first])
    );
    host.studio.drop_cue(item);
    host.apply(&|_| {});
    assert_eq!(host.studio.capture_outgoing_cues(), BTreeSet::from([first]));
    clock.advance(Duration::from_millis(500));
    host.studio.show_cue(item, false, Some(4_000));
    host.apply(&|_| {});
    assert_eq!(
        host.studio.cue_wait(clock.now()),
        Some(Duration::from_millis(1_500))
    );
    assert!(host.transition().unwrap());
    host.apply(&|_| {});
    let second = host.program.held.as_ref().unwrap().cue_items[&item];
    assert_ne!(first, second);
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, second])
    );
    host.studio.drop_cue(item);
    host.apply(&|_| {});
    clock.advance(Duration::from_millis(1_500));
    host.studio.expire_cues(clock.now());
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([second])
    );
    host.set_studio_mode(false).unwrap();
    host.apply(&|_| {});
    assert!(host.studio.capture_outgoing_cues().is_empty());
}

#[test]
fn failed_transition_restores_the_installed_held_cues() {
    let Some((mut host, _, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let first = host.program.held.as_ref().unwrap().cue_items[&item];
    host.studio.drop_cue(item);
    // The studio refuses an update that changes its canvas format.
    host.collection
        .set_canvas_format(host.canvas, Size::new(640, 360), FrameRate::FPS_30)
        .unwrap();
    assert!(host.transition().unwrap());
    host.apply(&|_| {});
    assert_eq!(host.studio.capture_outgoing_cues(), BTreeSet::from([first]));
    assert_eq!(host.program.held.as_ref().unwrap().cue_items[&item], first);
}

#[test]
fn collection_switch_does_not_import_previous_cues() {
    let Some((mut host, _, _)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    assert!(!host.studio.capture_outgoing_cues().is_empty());
    let next = host.collection.clone();
    host.switch_collection(next, None, &|_| {}).unwrap();
    host.apply(&|_| {});
    assert!(host.studio.capture_outgoing_cues().is_empty());
}

#[test]
fn restart_rebases_the_installed_held_cue_before_start() {
    let old = Arc::new(TestClock::new());
    old.advance(Duration::from_secs(10));
    let new = Arc::new(TestClock::new());
    new.advance(Duration::from_secs(1));
    let Some((mut host, item)) = cue_host_with_clocks(&[Arc::clone(&old), Arc::clone(&new)], None)
    else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let held = host.program.held.as_ref().unwrap().cue_items[&item];
    host.studio.drop_cue(item);
    old.advance(Duration::from_millis(500) + Duration::from_nanos(7));
    host.restart().unwrap();
    assert_eq!(host.studio.capture_outgoing_cues(), BTreeSet::from([held]));
    assert_eq!(
        host.studio.cue_wait(new.now()),
        Some(Duration::from_millis(1_500) - Duration::from_nanos(7))
    );
    host.apply(&|_| {});
    assert_eq!(
        host.studio.cue_wait(new.now()),
        Some(Duration::from_millis(1_500) - Duration::from_nanos(7))
    );
    new.advance(Duration::from_millis(1_500) - Duration::from_nanos(7));
    assert!(host.studio.expire_cues(new.now()));
    assert!(host.studio.capture_outgoing_cues().is_empty());
}

#[test]
fn restarted_studio_binds_a_source_shown_only_by_the_held_cue() {
    let old = Arc::new(TestClock::new());
    old.advance(Duration::from_secs(8));
    let new = Arc::new(TestClock::new());
    let clocks = [old, Arc::clone(&new)];
    let clocks = clocks.to_vec();
    let index = Cell::new(0);
    let Some(mut host) = publish_cost::host_with_engine(0, move || {
        let at = index.get().min(clocks.len() - 1);
        index.set(at + 1);
        Engine::with_clock(
            Arc::clone(&clocks[at]) as Arc<dyn Clock>,
            // Hang guard for a loaded test runner.
            EngineConfig::default().swap_timeout(Duration::from_secs(20)),
        )
    }) else {
        return;
    };
    let (source, _) = host
        .collection
        .add_source(
            "Microphone",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    let (item, _) = host
        .collection
        .add_item(
            host.scene,
            ItemContent::Source(source),
            Layout::new([0.0, 0.0], [1.0, 1.0]),
            None,
        )
        .unwrap();
    host.set_item_visible(item, false, 0).unwrap();
    host.set_item_show_for(item, Some(2_000)).unwrap();
    host.apply(&|_| {});
    assert!(host.show_item(item).unwrap());
    host.apply(&|_| {});
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    host.studio.drop_cue(item);
    host.apply(&|_| {});
    host.restart().unwrap();
    assert!(
        host.studio
            .sources()
            .iter()
            .any(|entry| entry.source == source)
    );
    new.advance(Duration::from_secs(2));
    assert!(host.studio.expire_cues(new.now()));
    host.apply(&|_| {});
    assert!(
        !host
            .studio
            .sources()
            .iter()
            .any(|entry| entry.source == source)
    );
}

#[test]
fn failed_collection_switch_restores_held_cue_on_the_rollback_clock() {
    let old = Arc::new(TestClock::new());
    old.advance(Duration::from_secs(20));
    let failed = Arc::new(TestClock::new());
    failed.advance(Duration::from_secs(1));
    let rollback = Arc::new(TestClock::new());
    rollback.advance(Duration::from_secs(9));
    let Some((mut host, item)) =
        cue_host_with_clocks(&[Arc::clone(&old), failed, Arc::clone(&rollback)], Some(1))
    else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let held = host.program.held.as_ref().unwrap().cue_items[&item];
    host.studio.drop_cue(item);
    old.advance(Duration::from_millis(500));
    let members: Vec<_> = host
        .collection
        .scene(host.scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.id)
        .collect();
    host.group_items(host.scene, &members, None).unwrap();
    let before = host.collection.clone();
    let next = before.clone();
    assert!(host.switch_collection(next, None, &|_| {}).is_err());
    assert_eq!(host.collection, before);
    assert_eq!(host.studio.capture_outgoing_cues(), BTreeSet::from([held]));
    assert_eq!(
        host.studio.cue_wait(rollback.now()),
        Some(Duration::from_millis(1_500))
    );
    assert_eq!(host.program.held.as_ref().unwrap().cue_items[&item], held);
}

#[test]
fn vertical_activation_defers_a_pending_held_take_without_changing_cues() {
    let Some((mut host, _, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let old = host.program.held.as_ref().unwrap().cue_items[&item];
    host.studio.drop_cue(item);
    host.studio.show_cue(item, false, Some(4_000));
    assert!(host.transition().unwrap());
    let before = host.collection.clone();
    let vertical = VerticalCanvas {
        size: Size::new(720, 1280),
        follow: true,
        layout: None,
    };
    assert!(host.add_vertical(vertical).is_err());
    assert_eq!(host.collection, before);
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, old])
    );
    assert!(host.program.held.is_none());

    host.apply(&|_| {});
    let new = host.program.held.as_ref().unwrap().cue_items[&item];
    assert_ne!(old, new);
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, new])
    );
    assert!(host.add_vertical(vertical).unwrap());
}

#[test]
fn projector_close_is_deferred_until_the_pending_held_take_is_applied() {
    let Some((mut host, _, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    host.open_projector(42, OutputCanvas::Main, &|_| {});
    host.sync_projectors(&|_| {});
    let picture = host.projectors[0].picture.unwrap();
    let old = host.program.held.as_ref().unwrap().cue_items[&item];
    host.studio.drop_cue(item);
    host.studio.show_cue(item, false, Some(4_000));
    assert!(host.transition().unwrap());
    host.close_projector(42);
    assert_eq!(host.projectors[0].picture, Some(picture));
    assert!(host.projectors[0].closing);
    assert!(host.studio.pictures().contains(&picture));
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, old])
    );
    host.apply(&|_| {});
    assert!(host.projectors.is_empty());
    assert!(!host.studio.pictures().contains(&picture));
    let new = host.program.held.as_ref().unwrap().cue_items[&item];
    assert_ne!(old, new);
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, new])
    );
}

#[test]
fn vertical_preview_refuses_a_fallback_recopy_until_it_is_applied() {
    let Some((mut host, _, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    assert!(
        host.add_vertical(VerticalCanvas {
            size: Size::new(720, 1280),
            follow: true,
            layout: None,
        })
        .unwrap()
    );
    let old = host.program.held.as_ref().unwrap().cue_items[&item];
    let name = host.program.held.as_ref().unwrap().scene.name.clone();
    host.collection
        .add_scene(&name, SceneScope::Shared)
        .unwrap();
    assert!(
        run_copy(
            &host.collection,
            host.canvas,
            host.program.held.as_ref().unwrap()
        )
        .is_none()
    );
    assert!(host.set_vertical_picture(true).is_err());
    assert!(!host.verticals.vertical_picture);
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, old])
    );

    host.apply(&|_| {});
    assert_ne!(host.program.held.as_ref().unwrap().scene.name, name);
    assert!(host.set_vertical_picture(true).is_ok());
}

#[test]
fn startup_with_a_held_program_restores_the_saved_vertical_canvas() {
    let clock = Arc::new(TestClock::new());
    let Some(host) = publish_cost::host_with_document(
        2,
        |collection| {
            let main = collection.canvases().next().unwrap().id;
            let scene = collection.canvas(main).unwrap().program.unwrap();
            collection.set_preview(main, Some(scene)).unwrap();
            let (vertical, _) = collection
                .add_canvas("Vertical", Size::new(720, 1280), FrameRate::FPS_30)
                .unwrap();
            collection.set_program(vertical, Some(scene)).unwrap();
        },
        move || {
            Engine::with_clock(
                Arc::clone(&clock) as Arc<dyn Clock>,
                // Hang guard for a loaded test runner.
                EngineConfig::default().swap_timeout(Duration::from_secs(20)),
            )
        },
    ) else {
        return;
    };
    assert!(host.program.held.is_some());
    assert!(host.program.applied_cues.is_some());
    assert!(host.verticals.vertical.is_some());
}

#[test]
fn projector_close_keeps_its_picture_while_the_held_program_cannot_be_bound() {
    let Some((mut host, _, item)) = cue_host() else {
        return;
    };
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    host.open_projector(42, OutputCanvas::Main, &|_| {});
    host.sync_projectors(&|_| {});
    let picture = host.projectors[0].picture.unwrap();
    let held = host.program.held.as_ref().unwrap().cue_items[&item];
    let before = host.collection.clone();
    // Another program not yet applied: the held copy the studio runs cannot be bound.
    let (other, _) = host
        .collection
        .add_scene("Other", SceneScope::Shared)
        .unwrap();
    host.collection
        .set_program(host.canvas, Some(other))
        .unwrap();
    host.close_projector(42);
    assert!(host.projectors[0].closing);
    assert!(host.studio.pictures().contains(&picture));
    assert_eq!(
        host.studio.capture_outgoing_cues(),
        BTreeSet::from([item, held])
    );
    host.collection = before;
    host.sync_projectors(&|_| {});
    assert!(host.projectors.is_empty());
    assert!(!host.studio.pictures().contains(&picture));
}
