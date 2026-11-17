//! Remove from scene with companion sounds (ADR-0065 decision 7, ADR-0142).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    ItemContent, ItemId, Layout, SceneId, SceneLevel, SceneScope, Settings, SourceId,
};
use rbs_scene::{COMPANION_OF_HINT, SceneAudio, SceneCollection};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

fn full() -> Layout {
    Layout::new([0.0, 0.0], [1920.0, 1080.0])
}

struct Fixture {
    col: SceneCollection,
    a: SceneId,
    b: SceneId,
    picture: SourceId,
    sound: SourceId,
}

/// A window and its companion sound, both placed in scenes A and B (as Duplicate scene
/// leaves them).
fn fixture() -> Fixture {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (a, _) = col.add_scene("A", SceneScope::Shared).unwrap();
    let (picture, _) = col
        .add_source("Chrome", ty("window-capture"), Settings::new())
        .unwrap();
    let (sound, _) = col
        .add_source("Chrome sound", ty("audio-input"), Settings::new())
        .unwrap();
    companion(&mut col, sound, picture);
    col.add_item(a, ItemContent::Source(picture), full(), None)
        .unwrap();
    col.add_item(a, ItemContent::Source(sound), full(), None)
        .unwrap();
    let (b, _) = col.duplicate_scene(a, "B").unwrap();
    Fixture {
        col,
        a,
        b,
        picture,
        sound,
    }
}

fn companion(col: &mut SceneCollection, sound: SourceId, picture: SourceId) {
    let mut hints = BTreeMap::new();
    hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
    col.set_source_settings_and_hints(sound, Settings::new(), hints)
        .unwrap();
}

fn item_of(col: &SceneCollection, scene: SceneId, source: SourceId) -> ItemId {
    col.scene(scene)
        .unwrap()
        .items
        .iter()
        .find(|i| i.content == ItemContent::Source(source))
        .unwrap()
        .id
}

fn contents(col: &SceneCollection, scene: SceneId) -> Vec<ItemContent> {
    col.scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.content)
        .collect()
}

/// Undo, redo and undo again give back exactly `before` and `after`.
fn round_trip(col: &mut SceneCollection, before: &SceneCollection, undo: rbs_scene::Undo) {
    let after = col.clone();
    let redo = col.undo(undo).unwrap();
    assert_eq!(col, before);
    let undo = col.undo(redo).unwrap();
    assert_eq!(*col, after);
    col.undo(undo).unwrap();
    assert_eq!(col, before);
}

#[test]
fn the_companion_leaves_the_scene_with_its_picture_and_other_scenes_stay() {
    let mut f = fixture();
    let before = f.col.clone();
    let b_before = contents(&f.col, f.b);
    let pic = item_of(&f.col, f.a, f.picture);
    let snd = item_of(&f.col, f.a, f.sound);
    let (removed, undo) = f.col.remove_from_scene(pic).unwrap();
    assert!(!removed.source_deleted);
    assert!(removed.deleted_sources.is_empty());
    assert_eq!(removed.removed_items, vec![pic, snd]);
    assert!(contents(&f.col, f.a).is_empty());
    assert_eq!(contents(&f.col, f.b), b_before, "B is untouched");
    round_trip(&mut f.col, &before, undo);
}

#[test]
fn the_last_placement_deletes_the_picture_and_its_companion() {
    let mut f = fixture();
    let pic_a = item_of(&f.col, f.a, f.picture);
    f.col.remove_from_scene(pic_a).unwrap();
    let before = f.col.clone();
    let pic_b = item_of(&f.col, f.b, f.picture);
    let snd_b = item_of(&f.col, f.b, f.sound);
    let (removed, undo) = f.col.remove_from_scene(pic_b).unwrap();
    assert!(removed.source_deleted);
    assert_eq!(removed.deleted_sources, vec![f.sound, f.picture]);
    assert_eq!(removed.removed_items, vec![pic_b, snd_b]);
    assert!(f.col.source(f.picture).is_none() && f.col.source(f.sound).is_none());
    round_trip(&mut f.col, &before, undo);
}

#[test]
fn a_second_item_of_the_picture_in_the_scene_keeps_the_companion() {
    let mut f = fixture();
    let (second, _) = f
        .col
        .add_item(f.a, ItemContent::Source(f.picture), full(), None)
        .unwrap();
    f.col.set_item_visible(second, false).unwrap();
    let pic = item_of(&f.col, f.a, f.picture);
    let (removed, _) = f.col.remove_from_scene(pic).unwrap();
    assert_eq!(
        removed.removed_items,
        vec![pic],
        "a hidden item still draws it"
    );
    assert!(contents(&f.col, f.a).contains(&ItemContent::Source(f.sound)));
}

#[test]
fn a_picture_still_drawn_through_a_nested_scene_keeps_the_companion() {
    let mut f = fixture();
    let (n, _) = f.col.add_scene("Nested", SceneScope::Shared).unwrap();
    let (nested_pic, _) = f
        .col
        .add_item(n, ItemContent::Source(f.picture), full(), None)
        .unwrap();
    let (nest, _) = f
        .col
        .add_item(f.a, ItemContent::Scene(n), full(), None)
        .unwrap();
    f.col.set_item_visible(nest, false).unwrap();
    let pic = item_of(&f.col, f.a, f.picture);
    let (removed, _) = f.col.remove_from_scene(pic).unwrap();
    assert_eq!(removed.removed_items, vec![pic]);
    // Known limit (decision 2): removing it from the nested scene leaves A's
    // companion item alone.
    let (removed, _) = f.col.remove_from_scene(nested_pic).unwrap();
    assert_eq!(removed.removed_items, vec![nested_pic]);
    assert!(contents(&f.col, f.a).contains(&ItemContent::Source(f.sound)));
}

#[test]
fn a_companion_in_a_nested_scene_stays() {
    let mut f = fixture();
    let (n, _) = f.col.add_scene("Nested", SceneScope::Shared).unwrap();
    let (nested_snd, _) = f
        .col
        .add_item(n, ItemContent::Source(f.sound), full(), None)
        .unwrap();
    f.col
        .add_item(f.a, ItemContent::Scene(n), full(), None)
        .unwrap();
    let pic = item_of(&f.col, f.a, f.picture);
    let (removed, _) = f.col.remove_from_scene(pic).unwrap();
    assert!(!removed.removed_items.contains(&nested_snd));
    assert!(f.col.item(nested_snd).is_some());
}

#[test]
fn a_companion_placed_only_beside_the_removed_picture_is_deleted() {
    let mut f = fixture();
    // B keeps the picture but not the sound.
    let snd_b = item_of(&f.col, f.b, f.sound);
    f.col.remove_item(snd_b).unwrap();
    let before = f.col.clone();
    let pic = item_of(&f.col, f.a, f.picture);
    let snd = item_of(&f.col, f.a, f.sound);
    let (removed, undo) = f.col.remove_from_scene(pic).unwrap();
    assert!(!removed.source_deleted);
    assert_eq!(removed.deleted_sources, vec![f.sound]);
    assert_eq!(removed.removed_items, vec![pic, snd]);
    assert!(f.col.source(f.sound).is_none());
    assert!(f.col.source(f.picture).is_some());
    round_trip(&mut f.col, &before, undo);
}

#[test]
fn two_companions_and_two_items_of_one_go_together() {
    let mut f = fixture();
    let (cam_mic, _) = f
        .col
        .add_source("Cam mic", ty("audio-input"), Settings::new())
        .unwrap();
    companion(&mut f.col, cam_mic, f.picture);
    f.col
        .add_item(f.a, ItemContent::Source(cam_mic), full(), None)
        .unwrap();
    f.col
        .add_item(f.a, ItemContent::Source(f.sound), full(), None)
        .unwrap();
    let before = f.col.clone();
    let pic = item_of(&f.col, f.a, f.picture);
    let (removed, undo) = f.col.remove_from_scene(pic).unwrap();
    assert_eq!(removed.removed_items.len(), 4);
    assert_eq!(removed.deleted_sources, vec![cam_mic], "placed only in A");
    assert!(contents(&f.col, f.a).is_empty());
    round_trip(&mut f.col, &before, undo);
}

#[test]
fn own_levels_are_kept_for_a_removed_companion_and_restored_for_a_deleted_one() {
    let mut f = fixture();
    let mut levels = BTreeMap::new();
    let level = SceneLevel::new(Some(0.5), Some(false));
    levels.insert(f.sound, level);
    levels.insert(f.picture, level);
    f.col
        .set_scene_own_levels(f.a, SceneAudio::new(levels))
        .unwrap();
    let before = f.col.clone();
    let pic = item_of(&f.col, f.a, f.picture);
    let (_, undo) = f.col.remove_from_scene(pic).unwrap();
    let audio = f.col.scene(f.a).unwrap().audio.clone().unwrap();
    assert_eq!(audio.levels.get(&f.sound), Some(&level));
    round_trip(&mut f.col, &before, undo);
}

#[test]
fn a_companion_removed_on_its_own_and_a_nested_scene_item_go_alone() {
    let mut f = fixture();
    let snd = item_of(&f.col, f.a, f.sound);
    let (removed, _) = f.col.remove_from_scene(snd).unwrap();
    assert_eq!(removed.removed_items, vec![snd]);
    assert!(contents(&f.col, f.a).contains(&ItemContent::Source(f.picture)));
    let (n, _) = f.col.add_scene("Nested", SceneScope::Shared).unwrap();
    let (nest, _) = f
        .col
        .add_item(f.a, ItemContent::Scene(n), full(), None)
        .unwrap();
    let (removed, _) = f.col.remove_from_scene(nest).unwrap();
    assert_eq!(removed.removed_items, vec![nest]);
    assert!(f.col.scene(n).is_some());
}

#[test]
fn remove_item_still_removes_one_item() {
    let mut f = fixture();
    let pic = item_of(&f.col, f.a, f.picture);
    f.col.remove_item(pic).unwrap();
    assert_eq!(contents(&f.col, f.a), vec![ItemContent::Source(f.sound)]);
}

#[test]
fn a_global_picture_takes_its_companion_items_in_the_global_scene() {
    let mut f = fixture();
    let canvas = f.col.canvases().next().unwrap().id;
    let (g, _) = f
        .col
        .add_scene("Globals", SceneScope::Canvas(canvas))
        .unwrap();
    f.col.set_globals(canvas, Some(g)).unwrap();
    let (gpic, _) = f
        .col
        .add_item(g, ItemContent::Source(f.picture), full(), None)
        .unwrap();
    let (gsnd, _) = f
        .col
        .add_item(g, ItemContent::Source(f.sound), full(), None)
        .unwrap();
    let before = f.col.clone();
    // Still placed in A and B: only the global scene's items go.
    let (removed, undo) = f.col.remove_from_scene(gpic).unwrap();
    assert_eq!(removed.removed_items, vec![gpic, gsnd]);
    assert!(removed.deleted_sources.is_empty());
    assert!(contents(&f.col, g).is_empty());
    assert_eq!(contents(&f.col, f.a).len(), 2);
    round_trip(&mut f.col, &before, undo);
}
