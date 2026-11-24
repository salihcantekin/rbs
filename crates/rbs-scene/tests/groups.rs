//! Groups of items (ADR-0182): grouping, ungrouping, invariants, dissolving and undo.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, ItemId, Layout, SceneId, SceneScope, Settings};

fn full() -> Layout {
    Layout::new([0.0, 0.0], [1920.0, 1080.0])
}

/// A scene with `n` colour items, bottom first.
fn fixture(n: usize) -> (SceneCollection, SceneId, Vec<ItemId>) {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (scene, _) = col.add_scene("A", SceneScope::Shared).unwrap();
    let mut items = Vec::new();
    for k in 0..n {
        let (src, _) = col
            .add_source(
                &format!("S{k}"),
                NodeTypeId::new("org.rbs.source.colour"),
                Settings::new(),
            )
            .unwrap();
        let (item, _) = col
            .add_item(scene, ItemContent::Source(src), full(), None)
            .unwrap();
        items.push(item);
    }
    (col, scene, items)
}

fn order(col: &SceneCollection, scene: SceneId) -> Vec<ItemId> {
    col.scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.id)
        .collect()
}

fn tags(col: &SceneCollection, scene: SceneId) -> Vec<Option<u32>> {
    col.scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.group.map(|g| g.get()))
        .collect()
}

#[test]
fn grouping_lifts_members_next_to_the_topmost_and_undo_restores_the_order() {
    let (mut col, scene, it) = fixture(4);
    let before = order(&col, scene);
    // Items 0 and 2 grouped; item 1 sat between them.
    let (g, undo) = col.group_items(scene, &[it[0], it[2]], None).unwrap();
    assert_eq!(order(&col, scene), vec![it[1], it[0], it[2], it[3]]);
    assert_eq!(
        tags(&col, scene),
        vec![None, Some(g.get()), Some(g.get()), None]
    );
    assert_eq!(col.groups(scene).unwrap()[0].name, "Group 1");
    let redo = col.undo(undo).unwrap();
    assert_eq!(order(&col, scene), before);
    assert!(col.groups(scene).unwrap().is_empty());
    col.undo(redo).unwrap();
    assert_eq!(col.groups(scene).unwrap().len(), 1);
}

#[test]
fn a_group_needs_two_distinct_items_of_one_scene() {
    let (mut col, scene, it) = fixture(2);
    assert!(col.group_items(scene, &[it[0]], None).is_err());
    assert!(col.group_items(scene, &[it[0], it[0]], None).is_err());
    let (other, _) = col.add_scene("B", SceneScope::Shared).unwrap();
    assert!(col.group_items(other, &[it[0], it[1]], None).is_err());
}

#[test]
fn grouping_groups_merges_them() {
    let (mut col, scene, it) = fixture(5);
    col.group_items(scene, &[it[0], it[1]], None).unwrap();
    let (g2, _) = col.group_items(scene, &[it[1], it[3]], None).unwrap();
    // Item 0 came along with item 1's group; one group is left.
    assert_eq!(col.groups(scene).unwrap().len(), 1);
    let members = col
        .scene(scene)
        .unwrap()
        .items
        .iter()
        .filter(|i| i.group == Some(g2))
        .count();
    assert_eq!(members, 3);
}

#[test]
fn a_member_cannot_leave_the_block_but_can_move_inside_it() {
    let (mut col, scene, it) = fixture(4);
    col.group_items(scene, &[it[1], it[2]], None).unwrap();
    // Leaving the block would split it.
    assert!(col.move_item(it[1], 3).is_err());
    assert!(col.move_item(it[3], 2).is_err());
    // Swapping inside the block is fine.
    let idx = order(&col, scene).iter().position(|i| *i == it[2]).unwrap();
    col.move_item(it[1], idx).unwrap();
}

#[test]
fn removing_a_member_dissolves_a_two_member_group_and_undo_brings_it_back() {
    let (mut col, scene, it) = fixture(3);
    let (g, _) = col
        .group_items(scene, &[it[0], it[1]], Some("Pair"))
        .unwrap();
    let undo = col.remove_item(it[0]).unwrap();
    assert!(col.groups(scene).unwrap().is_empty());
    assert!(tags(&col, scene).iter().all(Option::is_none));
    col.undo(undo).unwrap();
    assert_eq!(col.groups(scene).unwrap()[0].name, "Pair");
    assert_eq!(
        tags(&col, scene).iter().flatten().count(),
        2,
        "both members are tagged again with {g:?}"
    );
}

#[test]
fn removing_one_of_three_members_keeps_the_group() {
    let (mut col, scene, it) = fixture(3);
    col.group_items(scene, &it, None).unwrap();
    col.remove_item(it[2]).unwrap();
    assert_eq!(col.groups(scene).unwrap().len(), 1);
}

#[test]
fn ungroup_keeps_the_order_and_rename_checks_the_name() {
    let (mut col, scene, it) = fixture(3);
    let (a, _) = col.group_items(scene, &[it[0], it[1]], None).unwrap();
    let order_before = order(&col, scene);
    col.rename_group(scene, a, "Camera set").unwrap();
    assert!(col.rename_group(scene, a, "  ").is_err());
    col.ungroup(scene, a).unwrap();
    assert_eq!(order(&col, scene), order_before);
    assert!(col.groups(scene).unwrap().is_empty());
}

#[test]
fn a_copied_scene_gets_fresh_group_ids() {
    let (mut col, scene, it) = fixture(2);
    let (g, _) = col.group_items(scene, &[it[0], it[1]], None).unwrap();
    let (copy, _) = col.duplicate_scene(scene, "B").unwrap();
    let copied = col.groups(copy).unwrap();
    assert_eq!(copied.len(), 1);
    assert_ne!(copied[0].id, g);
    assert!(
        col.scene(copy)
            .unwrap()
            .items
            .iter()
            .all(|i| i.group == Some(copied[0].id))
    );
}

#[test]
fn group_counters_survive_a_reload_so_ids_are_not_reused() {
    let (mut col, scene, it) = fixture(2);
    let (g, _) = col.group_items(scene, &[it[0], it[1]], None).unwrap();
    col.ungroup(scene, g).unwrap();
    assert!(col.id_counters().group > g.get());
}

#[test]
fn an_item_added_inside_a_block_lands_above_it() {
    let (mut col, scene, it) = fixture(3);
    col.group_items(scene, &[it[0], it[1]], None).unwrap();
    let (src, _) = col
        .add_source(
            "New",
            NodeTypeId::new("org.rbs.source.colour"),
            Settings::new(),
        )
        .unwrap();
    let (new, _) = col
        .add_item(scene, ItemContent::Source(src), full(), Some(1))
        .unwrap();
    let at = order(&col, scene).iter().position(|i| *i == new).unwrap();
    assert_eq!(at, 2, "above the two members, not between them");
}
