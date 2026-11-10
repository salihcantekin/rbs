//! "Capture audio too" companions (ADR-0096): sound is audible only while its picture
//! is visible, and deleting the picture deletes its companions in one undo record.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings, SourceId};
use rbs_scene::{COMPANION_OF_HINT, SceneCollection};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
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
