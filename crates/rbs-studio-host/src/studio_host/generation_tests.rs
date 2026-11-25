#![allow(clippy::unwrap_used)] // Tests fail by panicking.

//! Group Scroll runs as the studio keys them (ADR-0187): the host owns the generation
//! and run-id allocators and tells the studio which scene a held copy stands for.

use std::cell::Cell;

use rbs_core::time::TestClock;
use rbs_engine::EngineConfig;
use rbs_record::{GroupRunKey, ScrollControl};
use rbs_scene::model::{GroupId, GroupScroll, ScrollDirection};

use super::*;

/// Four items (sound, colour, sound, colour); the two colour items are grouped.
fn grouped() -> Option<(Host, GroupId, Vec<ItemId>)> {
    grouped_failing(None)
}

/// [`grouped`], where engine number `failing` (0: the first) cannot start a studio.
fn grouped_failing(failing: Option<usize>) -> Option<(Host, GroupId, Vec<ItemId>)> {
    let clock = Arc::new(TestClock::new());
    let made = Cell::new(0);
    let host = publish_cost::host_with_engine(4, move || {
        let mut engine = Engine::with_clock(
            Arc::clone(&clock) as Arc<dyn Clock>,
            // Hang guard for a loaded test runner.
            EngineConfig::default().swap_timeout(Duration::from_secs(20)),
        );
        if failing == Some(made.replace(made.get() + 1)) {
            // The studio's render partition exists already: its start fails.
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
    with_group(host)
}

fn with_group(mut host: Host) -> Option<(Host, GroupId, Vec<ItemId>)> {
    let scene = host.scene;
    let all: Vec<_> = host
        .collection
        .scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.id)
        .collect();
    let items = vec![all[1], all[3]];
    let (group, _) = host.group_items(scene, &items, None).unwrap();
    Some((host, group, items))
}

fn set_scroll(host: &mut Host, scene: SceneId, group: GroupId, items: &[ItemId], speed: f32) {
    let canvas = host.canvas;
    host.collection
        .set_group_scroll(
            scene,
            group,
            items,
            canvas,
            Some(GroupScroll::new(ScrollDirection::Left, speed)),
        )
        .unwrap();
    host.apply(&|_| {});
}

fn run_id(host: &Host, scene: SceneId, group: GroupId) -> Option<u64> {
    host.studio
        .group_run(scene, group, host.canvas)
        .map(|r| r.id)
}

fn keys(host: &Host) -> Vec<GroupRunKey> {
    host.studio.group_runs().keys().copied().collect()
}

#[test]
fn held_copy_shares_its_originals_run_until_the_preview_is_edited() {
    let Some((mut host, group, items)) = grouped() else {
        return;
    };
    let scene = host.scene;
    set_scroll(&mut host, scene, group, &items, 80.0);
    let original = keys(&host);
    assert_eq!(original.len(), 1);
    let run = host.studio.group_runs().get(&original[0]).copied();
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    assert!(host.program.held.is_some());
    // The held copy goes on with its original's run: same key, same phase.
    assert_eq!(keys(&host), original);
    assert_eq!(host.studio.group_runs().get(&original[0]).copied(), run);

    // Preview edits, back to the held value too, never reach the held run.
    set_scroll(&mut host, scene, group, &items, 90.0);
    set_scroll(&mut host, scene, group, &items, 80.0);
    assert_eq!(keys(&host), original);
    assert!(host.transition().unwrap());
    host.apply(&|_| {});
    let taken = keys(&host);
    assert!(taken.iter().all(|k| k.scene == scene && k.group == group));
    assert!(taken.iter().any(|k| k.generation > original[0].generation));
}

#[test]
fn ungroup_undo_and_a_copied_scene_start_fresh_runs() {
    let Some((mut host, group, items)) = grouped() else {
        return;
    };
    let scene = host.scene;
    set_scroll(&mut host, scene, group, &items, 80.0);
    let first = run_id(&host, scene, group).unwrap();
    host.ungroup(scene, group).unwrap();
    host.apply(&|_| {});
    assert_eq!(run_id(&host, scene, group), None);
    host.undo_or_redo(true).unwrap();
    host.apply(&|_| {});
    let restored = run_id(&host, scene, group).unwrap();
    assert!(restored > first);

    let copy = host.duplicate_scene(scene).unwrap();
    let copy_group = host.collection.scene(copy).unwrap().groups[0].id;
    host.switch_scene(copy).unwrap();
    host.apply(&|_| {});
    let copied = run_id(&host, copy, copy_group).unwrap();
    assert!(copied > restored);
}

#[test]
fn generations_and_run_ids_are_never_reused_across_restarts_and_switches() {
    let Some((mut host, group, items)) = grouped() else {
        return;
    };
    let scene = host.scene;
    let plain = host.collection.clone();
    set_scroll(&mut host, scene, group, &items, 80.0);
    let first = keys(&host);
    let id = run_id(&host, scene, group).unwrap();
    // A studio restart keeps the run, its generation and its public id.
    host.restart().unwrap();
    assert_eq!(keys(&host), first);
    assert_eq!(run_id(&host, scene, group), Some(id));
    // A live restart and a new API server lifetime: new ids, same generation.
    let canvas = host.canvas;
    host.studio
        .group_scroll_control(scene, group, canvas, ScrollControl::Restart)
        .unwrap();
    let live = run_id(&host, scene, group).unwrap();
    assert!(live > id);
    let run = host.studio.group_runs().get(&first[0]).copied();
    host.studio.renew_group_run_ids();
    let renewed = run_id(&host, scene, group).unwrap();
    assert!(renewed > live);
    assert_eq!(keys(&host), first);
    assert_eq!(host.studio.group_runs().get(&first[0]).copied(), run);

    // Switching to the same collection retires the run: a fresh generation and id.
    let scrolling = host.collection.clone();
    host.switch_collection(scrolling.clone(), None, &|_| {})
        .unwrap();
    host.apply(&|_| {});
    let same = keys(&host);
    assert_eq!(same.len(), 1);
    assert!(same[0].generation > first[0].generation);
    let again = run_id(&host, scene, group).unwrap();
    assert!(again > renewed);

    // A -> B -> A: B has no run; back in A, fresh again.
    host.switch_collection(plain, None, &|_| {}).unwrap();
    host.apply(&|_| {});
    assert!(keys(&host).is_empty());
    host.switch_collection(scrolling, None, &|_| {}).unwrap();
    host.apply(&|_| {});
    let back = keys(&host);
    assert_eq!(back.len(), 1);
    assert!(back[0].generation > same[0].generation);
    assert!(run_id(&host, scene, group).unwrap() > again);
}

#[test]
fn failed_collection_switch_keeps_runs_and_ids() {
    // Engine 0 starts the host, engine 1 the switch (it fails), engine 2 the rollback.
    let Some((mut host, group, items)) = grouped_failing(Some(1)) else {
        return;
    };
    let scene = host.scene;
    set_scroll(&mut host, scene, group, &items, 80.0);
    let before = keys(&host);
    let run = host.studio.group_runs().get(&before[0]).copied();
    let id = run_id(&host, scene, group).unwrap();
    let next = host.collection.clone();
    assert!(host.switch_collection(next, None, &|_| {}).is_err());
    assert_eq!(keys(&host), before);
    assert_eq!(host.studio.group_runs().get(&before[0]).copied(), run);
    assert_eq!(run_id(&host, scene, group), Some(id));
    host.apply(&|_| {});
    assert_eq!(keys(&host), before);
    assert_eq!(run_id(&host, scene, group), Some(id));
}

#[test]
fn held_program_shares_its_originals_run_from_the_first_frame() {
    let Some((mut host, group, items)) = grouped() else {
        return;
    };
    let scene = host.scene;
    set_scroll(&mut host, scene, group, &items, 80.0);
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let saved = host.collection.clone();
    let Some(started) = publish_cost::host_with_document(
        0,
        |c| *c = saved,
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
                // Hang guard for a loaded test runner.
                EngineConfig::default().swap_timeout(Duration::from_secs(20)),
            )
        },
    ) else {
        return;
    };
    assert!(started.program.held.is_some());
    // Without the mapping before the first bind, the copy would key its own scene.
    let at_start = keys(&started);
    assert_eq!(at_start.len(), 1);
    assert_eq!((at_start[0].scene, at_start[0].group), (scene, group));
}

#[test]
fn saved_group_scroll_survives_restart_and_a_canvas_change() {
    let Some((mut host, group, items)) = grouped() else {
        return;
    };
    let scene = host.scene;
    set_scroll(&mut host, scene, group, &items, 80.0);
    let first = run_id(&host, scene, group).unwrap();
    host.restart().unwrap();
    assert_eq!(run_id(&host, scene, group), Some(first));
    host.set_canvas(Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    host.apply(&|_| {});
    assert!(run_id(&host, scene, group).is_some());
    assert!(
        host.collection
            .groups(scene)
            .unwrap()
            .iter()
            .any(|g| g.id == group && g.scrolls.contains_key(&host.canvas))
    );
}
