//! Atomic group Remove from scene, including companion dependencies.

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use std::collections::{BTreeMap, BTreeSet};

use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{ItemContent, ItemId, Layout, SceneId, SceneScope, Settings, SourceId};
use rbs_scene::{COMPANION_OF_HINT, SceneCollection, SceneError};

fn source(col: &mut SceneCollection, name: &str) -> SourceId {
    col.add_source(
        name,
        NodeTypeId::new("org.rbs.source.colour"),
        Settings::new(),
    )
    .unwrap()
    .0
}

fn place(col: &mut SceneCollection, scene: SceneId, content: ItemContent) -> ItemId {
    col.add_item(scene, content, Layout::new([0.0, 0.0], [10.0, 10.0]), None)
        .unwrap()
        .0
}

fn companion(col: &mut SceneCollection, picture: SourceId) -> SourceId {
    let sound = source(col, "Sound");
    col.set_source_settings_and_hints(
        sound,
        Settings::new(),
        BTreeMap::from([(COMPANION_OF_HINT.to_owned(), picture.get().to_string())]),
    )
    .unwrap();
    sound
}

#[test]
fn last_placements_and_shared_companions_are_deduplicated_and_undo_together() {
    let mut col = SceneCollection::new();
    let scene = col.add_scene("Main", SceneScope::Shared).unwrap().0;
    let other = col.add_scene("Other", SceneScope::Shared).unwrap().0;
    let picture = source(&mut col, "Picture");
    let sound = companion(&mut col, picture);
    let first = place(&mut col, scene, ItemContent::Source(picture));
    let second = place(&mut col, scene, ItemContent::Source(picture));
    let (group, _) = col.group_items(scene, &[first, second], None).unwrap();
    let sound_here = place(&mut col, scene, ItemContent::Source(sound));
    let sound_elsewhere = place(&mut col, other, ItemContent::Source(sound));
    let other_source = source(&mut col, "Other source");
    let other_item = place(&mut col, other, ItemContent::Source(other_source));
    col.group_items(other, &[sound_elsewhere, other_item], None)
        .unwrap();
    let mut items = col.scene(scene).unwrap().items.clone();
    items.iter_mut().find(|i| i.id == sound_here).unwrap().group = Some(group);
    col.set_scene_items(scene, items).unwrap();
    let before = col.clone();
    let (removed, undo) = col
        .remove_group_members(scene, group, &[sound_here, second, first])
        .unwrap();
    assert_eq!(
        removed
            .removed_items
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first, second, sound_here, sound_elsewhere])
    );
    assert_eq!(removed.removed_items.len(), 4);
    assert_eq!(
        removed
            .deleted_sources
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([picture, sound])
    );
    assert!(removed.source_deleted);
    assert!(col.source(picture).is_none() && col.source(sound).is_none());
    let redo = col.undo(undo).unwrap();
    assert_eq!(col, before);
    col.undo(redo).unwrap();
    assert!(col.item(sound_elsewhere).is_none());
}

#[test]
fn removal_considers_the_whole_set_when_a_picture_disappears() {
    let mut col = SceneCollection::new();
    let scene = col.add_scene("Main", SceneScope::Shared).unwrap().0;
    let nested = col.add_scene("Nested", SceneScope::Shared).unwrap().0;
    let picture = source(&mut col, "Picture");
    let sound = companion(&mut col, picture);
    let direct = place(&mut col, scene, ItemContent::Source(picture));
    place(&mut col, nested, ItemContent::Source(picture));
    let other = source(&mut col, "Other");
    let nested_item = place(&mut col, scene, ItemContent::Source(other));
    let (group, _) = col
        .group_items(scene, &[direct, nested_item], None)
        .unwrap();
    let sound_here = place(&mut col, scene, ItemContent::Source(sound));
    let sound_nested = place(&mut col, nested, ItemContent::Source(sound));
    let before = col.clone();
    let (removed, undo) = col
        .remove_group_members(scene, group, &[direct, nested_item])
        .unwrap();
    assert_eq!(
        removed
            .removed_items
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([direct, nested_item, sound_here])
    );
    assert_eq!(removed.deleted_sources, vec![other]);
    assert!(col.source(picture).is_some() && col.source(sound).is_some());
    assert!(col.item(sound_nested).is_some());
    col.undo(undo).unwrap();
    assert_eq!(col, before);
}

#[test]
fn companion_stays_when_the_scene_still_places_the_picture() {
    let mut col = SceneCollection::new();
    let scene = col.add_scene("Main", SceneScope::Shared).unwrap().0;
    let nested = col.add_scene("Nested", SceneScope::Shared).unwrap().0;
    let picture = source(&mut col, "Picture");
    let sound = companion(&mut col, picture);
    let unrelated = source(&mut col, "Other");
    let direct = place(&mut col, scene, ItemContent::Source(picture));
    let other = place(&mut col, scene, ItemContent::Source(unrelated));
    place(&mut col, nested, ItemContent::Source(picture));
    place(&mut col, scene, ItemContent::Source(picture));
    let group = col.group_items(scene, &[direct, other], None).unwrap().0;
    let sound_here = place(&mut col, scene, ItemContent::Source(sound));
    let (removed, _) = col
        .remove_group_members(scene, group, &[direct, other])
        .unwrap();
    assert!(!removed.removed_items.contains(&sound_here));
    assert!(col.item(sound_here).is_some());
    assert!(col.source(sound).is_some());
}

#[test]
fn stale_duplicate_or_empty_membership_never_mutates() {
    let mut col = SceneCollection::new();
    let scene = col.add_scene("Main", SceneScope::Shared).unwrap().0;
    let a = source(&mut col, "A");
    let b = source(&mut col, "B");
    let x = place(&mut col, scene, ItemContent::Source(a));
    let y = place(&mut col, scene, ItemContent::Source(b));
    let group = col.group_items(scene, &[x, y], None).unwrap().0;
    let before = col.clone();
    for members in [vec![], vec![x], vec![x, x]] {
        assert_eq!(
            col.remove_group_members(scene, group, &members),
            Err(SceneError::UndoConflict)
        );
        assert_eq!(col, before);
    }
}
