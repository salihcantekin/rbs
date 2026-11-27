//! ADR-0188 scene-placement refusal.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    CollectionGlide, CollectionTransition, ItemContent, ItemId, Layout, Scene, SceneId, SceneItem,
    SceneScope, Settings,
};
use rbs_scene::{IdCounters, SceneCollection, SceneError};

fn nested_scene() -> Scene {
    let mut scene = Scene::new(SceneId::from_raw(1), "Scene", SceneScope::Shared);
    scene.items.push(SceneItem::new(
        ItemId::from_raw(1),
        ItemContent::Scene(SceneId::from_raw(1)),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
    ));
    scene
}

#[test]
fn all_loaders_refuse_before_other_invariants() {
    let scene = nested_scene();
    let invalid_glide = CollectionGlide {
        duration_ms: u32::MAX,
        ..CollectionGlide::default()
    };
    let mut transition = CollectionTransition::default();
    transition.fade.duration_ms = u32::MAX;
    let error = SceneError::NestingUnsupported;
    assert_eq!(
        SceneCollection::from_parts(vec![], vec![], vec![scene.clone()], IdCounters::default()),
        Err(error.clone())
    );
    assert_eq!(
        SceneCollection::from_parts_with_glide(
            vec![],
            vec![],
            vec![scene.clone()],
            IdCounters::default(),
            invalid_glide,
        ),
        Err(error.clone())
    );
    assert_eq!(
        SceneCollection::from_parts_with_transition(
            vec![],
            vec![],
            vec![scene],
            IdCounters::default(),
            invalid_glide,
            transition,
        ),
        Err(error.clone())
    );
    assert_eq!(
        error.to_string(),
        "Scenes cannot be placed inside other scenes."
    );
}

#[test]
fn edits_refuse_without_consuming_ids_and_valid_undo_redo_still_works() {
    let mut collection = SceneCollection::new();
    let (scene, _) = collection.add_scene("Scene", SceneScope::Shared).unwrap();
    let (source, _) = collection
        .add_source(
            "Source",
            NodeTypeId::new("org.rbs.source.test"),
            Settings::new(),
        )
        .unwrap();
    let before = collection.clone();
    let counters = collection.id_counters();
    let nested = SceneItem::new(
        ItemId::from_raw(0),
        ItemContent::Scene(scene),
        Layout::new([0.0; 2], [1.0; 2]),
    );
    assert_eq!(
        collection.add_item(scene, nested.content, nested.layout, None),
        Err(SceneError::NestingUnsupported)
    );
    assert_eq!(
        collection.set_scene_items(scene, vec![nested]),
        Err(SceneError::NestingUnsupported)
    );
    assert_eq!(collection, before);
    assert_eq!(collection.id_counters(), counters);
    let (_, undo) = collection
        .add_item(
            scene,
            ItemContent::Source(source),
            Layout::new([0.0; 2], [1.0; 2]),
            None,
        )
        .unwrap();
    let after = collection.clone();
    let redo = collection.undo(undo).unwrap();
    assert_eq!(collection, before);
    collection.undo(redo).unwrap();
    assert_eq!(collection, after);
}
