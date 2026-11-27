#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use std::sync::Mutex;

use rbs_api::request::{ApplyGroup, GroupChanged, GroupOperation, GroupScroll as ApiGroupScroll};
use rbs_api::types::{OutputCanvas, ScrollDirection as ApiDirection, ScrollRepeat};
use rbs_api::{Event, TopicSet};
use rbs_core::time::TestClock;
use rbs_scene::model::GroupId;

use super::super::*;

type Log = Arc<Mutex<Vec<Event>>>;
type Fixture = (Host, Arc<TestClock>, GroupId, Vec<ItemId>, Log);

/// Four items with the two colour items grouped, and a local subscriber to the group
/// topics.
fn grouped() -> Option<Fixture> {
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
    let log: Log = Arc::default();
    let sink = Arc::clone(&log);
    host.events.set_local(
        TopicSet::of([Topic::GroupChanged, Topic::GroupScrollFinished]),
        move |e| sink.lock().unwrap().push(e.clone()),
    );
    Some((host, clock, group, items, log))
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

fn take(log: &Log) -> Vec<Event> {
    std::mem::take(&mut *log.lock().unwrap())
}

#[test]
fn group_changed_follows_edits_and_live_calls_and_reports_removal() {
    let Some((mut host, _, group, items, log)) = grouped() else {
        return;
    };
    host.publish(&|_| {});
    let first = take(&log);
    assert_eq!(first.len(), 1);
    let key = first[0].key();
    // Nothing changed: nothing is sent again.
    host.publish(&|_| {});
    assert!(take(&log).is_empty());

    assert!(host.rename_group(host.scene, group, "Lower third").unwrap());
    host.publish(&|_| {});
    let renamed = take(&log);
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].key(), key);
    assert!(renamed[0].seq > first[0].seq);
    let EventPayload::GroupChanged(GroupChanged::State(state)) = &renamed[0].payload else {
        panic!("{renamed:?}");
    };
    assert_eq!(state.name, "Lower third");

    let scroll = GroupOperation::SetScroll {
        scroll: Some(ApiGroupScroll {
            speed: 80.0,
            direction: ApiDirection::Left,
            gap: 0.0,
            repeat: Default::default(),
            start: Default::default(),
            fade: 0.0,
            ramp_ms: 0,
        }),
    };
    assert!(
        host.group_apply(&apply(&host, group, &items, scroll))
            .unwrap()
    );
    host.apply(&|_| {});
    let events = take(&log);
    let EventPayload::GroupChanged(GroupChanged::State(state)) = &events.last().unwrap().payload
    else {
        panic!("{events:?}");
    };
    assert!(state.scroll.is_some() && state.run.is_some());
    let pause = apply(&host, group, &items, GroupOperation::ScrollPause {});
    assert!(host.group_apply(&pause).unwrap());
    host.publish(&|_| {});
    let events = take(&log);
    let EventPayload::GroupChanged(GroupChanged::State(state)) = &events.last().unwrap().payload
    else {
        panic!("{events:?}");
    };
    assert!(state.run.unwrap().paused);

    host.ungroup(host.scene, group).unwrap();
    host.publish(&|_| {});
    let removed = take(&log);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].key(), key);
    assert!(matches!(
        removed[0].payload,
        EventPayload::GroupChanged(GroupChanged::Removed(r)) if r.removed && r.group == group.get()
    ));
}

#[test]
fn a_once_group_run_reports_its_end_once() {
    let Some((mut host, clock, group, items, log)) = grouped() else {
        return;
    };
    let scroll = GroupOperation::SetScroll {
        scroll: Some(ApiGroupScroll {
            speed: 4000.0,
            direction: ApiDirection::Left,
            gap: 0.0,
            repeat: ScrollRepeat::Once,
            start: Default::default(),
            fade: 0.0,
            ramp_ms: 0,
        }),
    };
    assert!(
        host.group_apply(&apply(&host, group, &items, scroll))
            .unwrap()
    );
    host.apply(&|_| {});
    let run = host
        .studio
        .group_run(host.scene, group, host.canvas)
        .unwrap()
        .id;
    host.studio.service();
    host.publish(&|_| {});
    let finished = |log: &Log| {
        take(log)
            .into_iter()
            .filter_map(|e| match e.payload {
                EventPayload::GroupScrollFinished(f) => Some(f),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(finished(&log).is_empty());
    // Far past any window at 4000 units per second.
    clock.advance(Duration::from_secs(60));
    host.studio.service();
    host.publish(&|_| {});
    let ended = finished(&log);
    assert_eq!(ended.len(), 1);
    assert_eq!((ended[0].group, ended[0].run_id), (group.get(), run));
    host.studio.service();
    host.publish(&|_| {});
    assert!(finished(&log).is_empty());
}

#[test]
fn remove_members_removes_the_group_in_one_undo_step() {
    let Some((mut host, _, group, items, _)) = grouped() else {
        return;
    };
    let before = host.collection.clone();
    let stale = apply(&host, group, &items[..1], GroupOperation::RemoveMembers {});
    assert!(matches!(
        host.group_apply(&stale),
        Err(Refusal::Conflict(_))
    ));
    let remove = apply(&host, group, &items, GroupOperation::RemoveMembers {});
    assert!(host.group_apply(&remove).unwrap());
    let scene = host.collection.scene(host.scene).unwrap();
    assert!(scene.groups.is_empty() && scene.items.iter().all(|i| !items.contains(&i.id)));
    host.undo_or_redo(true).unwrap();
    assert!(host.collection == before);
}
