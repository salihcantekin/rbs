#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_api::request::{ApplyGroup, GetGroup, GroupOperation, GroupScroll as ApiGroupScroll};
use rbs_api::types::{OutputCanvas, ScrollDirection as ApiDirection};
use rbs_core::time::TestClock;
use rbs_scene::model::GroupId;

use super::super::*;

/// Four items (sound, colour, sound, colour); the two colour items are grouped.
fn grouped() -> Option<(Host, Arc<TestClock>, GroupId, Vec<ItemId>)> {
    let clock = Arc::new(TestClock::new());
    let mut host = publish_cost::host_with_clock(4, Arc::clone(&clock))?;
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
    Some((host, clock, group, items))
}

fn apply(host: &Host, group: GroupId, items: &[ItemId], operation: GroupOperation) -> ApplyGroup {
    ApplyGroup {
        scene: host.scene.get(),
        group: group.get(),
        items: items.iter().map(|i| i.get()).collect(),
        canvas: OutputCanvas::Main,
        operation,
        gesture: 0,
    }
}

#[test]
fn refused_preflight_changes_nothing() {
    let Some((mut host, _, group, items)) = grouped() else {
        return;
    };
    for &item in &items {
        host.set_item_visible(item, false, 0).unwrap();
    }
    host.set_item_show_for(items[0], Some(1_000)).unwrap();
    host.set_item_locked(items[1], true, 0).unwrap();
    let before = host.collection.clone();
    let step = host.edits.history.next_undo().cloned();
    let refusals = [
        apply(&host, group, &items[..1], GroupOperation::Show {}),
        apply(&host, group, &[items[0], items[0]], GroupOperation::Show {}),
        apply(&host, group, &items, GroupOperation::Show {}),
        apply(
            &host,
            group,
            &items,
            GroupOperation::Move { dx: 5.0, dy: 0.0 },
        ),
        apply(
            &host,
            group,
            &items,
            GroupOperation::SetShowFor {
                show_for_ms: Some(0),
            },
        ),
    ];
    for request in refusals {
        assert!(host.group_apply(&request).is_err(), "{request:?}");
    }
    let mut gestured = apply(&host, group, &items, GroupOperation::Show {});
    gestured.gesture = 7;
    assert!(matches!(
        host.group_apply(&gestured),
        Err(Refusal::Invalid(_))
    ));
    assert!(host.collection == before);
    assert_eq!(host.edits.history.next_undo().cloned(), step);
    let now = host.clock.now();
    assert!(
        items
            .iter()
            .all(|&i| host.studio.cue_remaining_ms(i, now).is_none())
    );
}

#[test]
fn show_starts_every_hidden_member_at_one_instant_and_leaves_held_cues() {
    let Some((mut host, clock, group, items)) = grouped() else {
        return;
    };
    for &item in &items {
        host.set_item_visible(item, false, 0).unwrap();
    }
    host.set_item_show_for(items[0], Some(2_000)).unwrap();
    host.set_item_show_for(items[1], Some(3_000)).unwrap();
    host.apply(&|_| {});
    host.set_studio_mode(true).unwrap();
    host.apply(&|_| {});
    let held = host.studio.held_cues().cloned();
    let show = apply(&host, group, &items, GroupOperation::Show {});
    assert!(host.group_apply(&show).unwrap());
    let left = |host: &Host| {
        let now = clock.now();
        items
            .iter()
            .map(|&i| host.studio.cue_remaining_ms(i, now))
            .collect::<Vec<_>>()
    };
    assert_eq!(left(&host), [Some(2_000), Some(3_000)]);
    clock.advance(Duration::from_millis(500));
    assert_eq!(left(&host), [Some(1_500), Some(2_500)]);
    assert!(host.group_apply(&show).unwrap());
    assert_eq!(left(&host), [Some(2_000), Some(3_000)]);
    assert_eq!(host.studio.held_cues().cloned(), held);
    host.apply(&|_| {});
    let state = host
        .group_get(&GetGroup {
            scene: host.scene.get(),
            group: group.get(),
            canvas: OutputCanvas::Main,
        })
        .unwrap();
    assert!(state.members.iter().all(|m| m.cued && !m.visible));
    assert!(state.run.is_none() && state.render_failures.is_empty());
}

#[test]
fn show_duration_and_visibility_are_one_undo_step_each() {
    let Some((mut host, _, group, items)) = grouped() else {
        return;
    };
    let set = GroupOperation::SetShowFor {
        show_for_ms: Some(4_000),
    };
    assert!(
        host.group_apply(&apply(&host, group, &items, set.clone()))
            .unwrap()
    );
    assert!(!host.group_apply(&apply(&host, group, &items, set)).unwrap());
    let durations = |host: &Host| {
        items
            .iter()
            .map(|&i| host.collection.item(i).unwrap().1.show_for_ms)
            .collect::<Vec<_>>()
    };
    assert_eq!(durations(&host), [Some(4_000), Some(4_000)]);
    let hide = GroupOperation::SetVisible { visible: false };
    assert!(
        host.group_apply(&apply(&host, group, &items, hide))
            .unwrap()
    );
    host.undo_or_redo(true).unwrap();
    assert!(
        items
            .iter()
            .all(|&i| host.collection.item(i).unwrap().1.visible)
    );
    host.undo_or_redo(true).unwrap();
    assert_eq!(durations(&host), [None, None]);
}

#[test]
fn group_scroll_saves_and_its_live_run_takes_live_calls_outside_undo() {
    let Some((mut host, _, group, items)) = grouped() else {
        return;
    };
    let scroll = |speed| ApiGroupScroll {
        speed,
        direction: ApiDirection::Left,
        gap: 0.0,
        repeat: Default::default(),
        start: Default::default(),
        fade: 0.0,
        ramp_ms: 300,
    };
    let set = |speed| GroupOperation::SetScroll {
        scroll: Some(scroll(speed)),
    };
    let get = |host: &Host| {
        host.group_get(&GetGroup {
            scene: host.scene.get(),
            group: group.get(),
            canvas: OutputCanvas::Main,
        })
        .unwrap()
    };
    let before = host.collection.clone();
    let pause = apply(&host, group, &items, GroupOperation::ScrollPause {});
    assert!(matches!(host.group_apply(&pause), Err(Refusal::Invalid(_))));

    assert!(
        host.group_apply(&apply(&host, group, &items, set(80.0)))
            .unwrap()
    );
    assert!(
        !host
            .group_apply(&apply(&host, group, &items, set(80.0)))
            .unwrap()
    );
    host.apply(&|_| {});
    let state = get(&host);
    assert!(state.scroll.is_some() && state.render_failures.is_empty());
    let run = state.run.unwrap();
    assert!(!run.paused && (run.speed - 80.0).abs() < 1e-3);
    let step = host.edits.history.next_undo().cloned();
    assert_eq!(step.as_ref().map(|s| s.kind), Some(EditKind::Scroll));

    // Live calls change only the run: no saved change, no undo step.
    let saved = host.collection.clone();
    assert!(host.group_apply(&pause).unwrap());
    let speed = apply(
        &host,
        group,
        &items,
        GroupOperation::ScrollSpeed { speed: 120.0 },
    );
    assert!(host.group_apply(&speed).unwrap());
    let paused = get(&host).run.unwrap();
    assert!(paused.paused && (paused.speed - 120.0).abs() < 1e-3 && paused.id == run.id);
    let resume = apply(&host, group, &items, GroupOperation::ScrollResume {});
    assert!(host.group_apply(&resume).unwrap());
    assert!(!get(&host).run.unwrap().paused);
    let restart = apply(&host, group, &items, GroupOperation::ScrollRestart {});
    assert!(host.group_apply(&restart).unwrap());
    let restarted = get(&host).run.unwrap();
    assert!(restarted.id > run.id && (restarted.speed - 80.0).abs() < 1e-3);
    assert!(host.collection == saved);
    assert_eq!(host.edits.history.next_undo().cloned(), step);

    // Each saved change, Undo included, is a new settings generation: a new run.
    assert!(
        host.group_apply(&apply(&host, group, &items, set(90.0)))
            .unwrap()
    );
    host.apply(&|_| {});
    let changed = get(&host).run.unwrap();
    assert!(changed.id > restarted.id);
    host.undo_or_redo_records(true).unwrap();
    host.apply(&|_| {});
    assert!(get(&host).run.unwrap().id > changed.id);
    host.undo_or_redo_records(true).unwrap();
    host.apply(&|_| {});
    assert!(get(&host).run.is_none());
    assert!(matches!(host.group_apply(&pause), Err(Refusal::Invalid(_))));
    assert!(host.collection == before);
}

#[test]
fn group_move_maps_every_member_in_one_step() {
    let Some((mut host, _, group, items)) = grouped() else {
        return;
    };
    let at = |host: &Host| {
        items
            .iter()
            .map(|&i| {
                host.crop_target_on(i, OutputCanvas::Main)
                    .ok()
                    .unwrap()
                    .1
                    .position
            })
            .collect::<Vec<_>>()
    };
    let start = at(&host);
    let request = apply(
        &host,
        group,
        &items,
        GroupOperation::Move { dx: 10.0, dy: -4.0 },
    );
    assert!(host.group_apply(&request).unwrap());
    for (a, b) in start.iter().zip(at(&host)) {
        assert!((b[0] - a[0] - 10.0).abs() < 1e-3 && (b[1] - a[1] + 4.0).abs() < 1e-3);
    }
    host.undo_or_redo(true).unwrap();
    assert_eq!(at(&host), start);
}
