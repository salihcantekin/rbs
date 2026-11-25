//! "Capture audio too" companions (ADR-0096): sound is audible only while its picture
//! is visible, and deleting the picture deletes its companions in one undo record.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings, SourceId};
use rbs_scene::{COMPANION_OF_HINT, SceneCollection, SceneError};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

fn full() -> Layout {
    Layout::new([0.0, 0.0], [1920.0, 1080.0])
}

struct Fixture {
    col: SceneCollection,
    canvas: rbs_scene::model::CanvasId,
    scene: rbs_scene::model::SceneId,
    picture: SourceId,
    sound: SourceId,
    picture_item: rbs_scene::model::ItemId,
}

fn fixture() -> Fixture {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (scene, _) = col.add_scene("Scene", SceneScope::Shared).unwrap();
    let (picture, _) = col
        .add_source("Chrome", ty("window-capture"), Settings::new())
        .unwrap();
    let (sound, _) = col
        .add_source("Chrome sound", ty("audio-input"), Settings::new())
        .unwrap();
    let mut hints = BTreeMap::new();
    hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
    col.set_source_settings_and_hints(sound, Settings::new(), hints)
        .unwrap();
    let full = Layout::new([0.0, 0.0], [1920.0, 1080.0]);
    let (picture_item, _) = col
        .add_item(scene, ItemContent::Source(picture), full, None)
        .unwrap();
    col.add_item(scene, ItemContent::Source(sound), full, None)
        .unwrap();
    Fixture {
        col,
        canvas,
        scene,
        picture,
        sound,
        picture_item,
    }
}

#[test]
fn the_hint_names_the_picture() {
    let f = fixture();
    assert_eq!(
        f.col.source(f.sound).unwrap().companion_of(),
        Some(f.picture)
    );
    assert_eq!(f.col.source(f.picture).unwrap().companion_of(), None);
}

#[test]
fn a_companion_is_audible_only_while_its_picture_is_visible() {
    let mut f = fixture();
    let shown = f.col.visible_sources(f.canvas, f.scene).unwrap();
    assert!(shown.contains(&f.picture) && shown.contains(&f.sound));
    f.col.set_item_visible(f.picture_item, false).unwrap();
    let shown = f.col.visible_sources(f.canvas, f.scene).unwrap();
    assert!(!shown.contains(&f.picture));
    assert!(!shown.contains(&f.sound), "no sound without its picture");
}

#[test]
fn deleting_the_picture_deletes_the_companion_in_one_undo() {
    let mut f = fixture();
    let before = f.col.clone();
    let undo = f.col.delete_source(f.picture).unwrap();
    assert!(f.col.source(f.picture).is_none());
    assert!(f.col.source(f.sound).is_none());
    let redo = f.col.undo(undo).unwrap();
    assert_eq!(
        f.col, before,
        "one undo restores both sources and placements"
    );
    f.col.undo(redo).unwrap();
    assert!(f.col.source(f.picture).is_none() && f.col.source(f.sound).is_none());
}

#[test]
fn deleting_only_the_companion_keeps_the_picture() {
    let mut f = fixture();
    f.col.delete_source(f.sound).unwrap();
    assert!(f.col.source(f.picture).is_some());
}

#[test]
fn new_companion_joins_a_group_directly_above_the_picture_and_undoes_as_one() {
    let mut f = fixture();
    f.col.delete_source(f.sound).unwrap();
    let (other, _) = f
        .col
        .add_source("Other", ty("colour"), Settings::new())
        .unwrap();
    let (other_item, _) = f
        .col
        .add_item(f.scene, ItemContent::Source(other), full(), None)
        .unwrap();
    let (group, _) = f
        .col
        .group_items(f.scene, &[f.picture_item, other_item], None)
        .unwrap();
    let before = f.col.clone();
    let (sound, item, undo) = f
        .col
        .add_companion(
            f.picture,
            "Sound",
            ty("audio-input"),
            Settings::new(),
            BTreeMap::new(),
            full(),
        )
        .unwrap();
    let scene = f.col.scene(f.scene).unwrap();
    let position = scene
        .items
        .iter()
        .position(|i| i.id == f.picture_item)
        .unwrap();
    assert_eq!(scene.items[position + 1].id, item);
    assert_eq!(scene.items[position + 1].group, Some(group));
    assert_eq!(f.col.source(sound).unwrap().companion_of(), Some(f.picture));
    let redo = f.col.undo(undo).unwrap();
    assert_eq!(f.col, before);
    f.col.undo(redo).unwrap();
    assert_eq!(f.col.item(item).unwrap().1.group, Some(group));
    assert_eq!(f.col.source(sound).unwrap().companion_of(), Some(f.picture));
}

#[test]
fn companion_creation_refuses_ambiguous_placements_without_changing_counters() {
    for other_scene in [false, true] {
        let mut f = fixture();
        f.col.delete_source(f.sound).unwrap();
        let scene = if other_scene {
            f.col
                .add_scene("Other scene", SceneScope::Shared)
                .unwrap()
                .0
        } else {
            f.scene
        };
        f.col
            .add_item(scene, ItemContent::Source(f.picture), full(), None)
            .unwrap();
        let before = f.col.clone();
        let counters = f.col.id_counters();
        assert_eq!(
            f.col.add_companion(
                f.picture,
                "Sound",
                ty("audio-input"),
                Settings::new(),
                BTreeMap::new(),
                full()
            ),
            Err(SceneError::AmbiguousCompanion)
        );
        assert_eq!(f.col, before);
        assert_eq!(f.col.id_counters(), counters);
    }
}

#[test]
fn companion_creation_failure_after_staging_leaves_no_source_item_or_id() {
    let mut f = fixture();
    f.col.delete_source(f.sound).unwrap();
    let before = f.col.clone();
    let counters = f.col.id_counters();
    let bad_layout = Layout::new([f32::NAN, 0.0], [100.0, 100.0]);
    assert_eq!(
        f.col.add_companion(
            f.picture,
            "Sound",
            ty("audio-input"),
            Settings::new(),
            BTreeMap::new(),
            bad_layout
        ),
        Err(SceneError::InvalidLayout)
    );
    assert_eq!(f.col, before);
    assert_eq!(f.col.id_counters(), counters);
}

#[test]
fn companion_creation_refuses_missing_placement_and_existing_companion() {
    let mut f = fixture();
    assert_eq!(
        f.col.add_companion(
            f.picture,
            "Sound",
            ty("audio-input"),
            Settings::new(),
            BTreeMap::new(),
            full()
        ),
        Err(SceneError::CompanionExists(f.picture))
    );
    f.col.delete_source(f.sound).unwrap();
    f.col.remove_item(f.picture_item).unwrap();
    let before = f.col.clone();
    let counters = f.col.id_counters();
    assert_eq!(
        f.col.companion_picture_placement(f.picture),
        Err(SceneError::PictureNotPlaced(f.picture))
    );
    assert_eq!(
        f.col.add_companion(
            f.picture,
            "Sound",
            ty("audio-input"),
            Settings::new(),
            BTreeMap::new(),
            full()
        ),
        Err(SceneError::PictureNotPlaced(f.picture))
    );
    assert_eq!(f.col, before);
    assert_eq!(f.col.id_counters(), counters);
}
