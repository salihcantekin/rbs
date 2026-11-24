//! Groups in the scene collection document (format 30, ADR-0182).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings};
use rbs_user_data::DataError;
use rbs_user_data::document::{self, Document};

fn grouped() -> SceneCollection {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let (scene, _) = col.add_scene("A", SceneScope::Shared).unwrap();
    let mut items = Vec::new();
    for k in 0..3 {
        let (src, _) = col
            .add_source(
                &format!("S{k}"),
                NodeTypeId::new("org.rbs.source.colour"),
                Settings::new(),
            )
            .unwrap();
        let layout = Layout::new([0.0, 0.0], [100.0, 100.0]);
        let (item, _) = col
            .add_item(scene, ItemContent::Source(src), layout, None)
            .unwrap();
        items.push(item);
    }
    col.group_items(scene, &items[..2], Some("Pair")).unwrap();
    col
}

#[test]
fn groups_round_trip_as_format_30() {
    let col = grouped();
    assert_eq!(col.write_format(), 30);
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.starts_with("format = 30"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);
    assert_eq!(back.id_counters(), col.id_counters());
}

#[test]
fn a_collection_without_groups_is_still_written_as_before() {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    assert!(col.write_format() < 30);
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(!text.contains("group"), "{text}");
}

#[test]
fn the_counter_outlives_the_last_group() {
    let mut col = grouped();
    let scene = col.scenes().next().unwrap().id;
    let group = col.groups(scene).unwrap()[0].id;
    col.ungroup(scene, group).unwrap();
    // No group is left, but ids were issued: older builds must not reuse them.
    assert_eq!(col.write_format(), 30);
    let text = document::render(&col, Some("Show")).unwrap();
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back.id_counters().group, col.id_counters().group);
}

#[test]
fn groups_in_a_format_before_30_are_refused() {
    let text = "format = 29\nkind = \"scene-collection\"\n[[canvases]]\nid = 0\nname = \"Main\"\nwidth = 1920\nheight = 1080\nframe_rate = [30, 1]\n[[scenes]]\nid = 0\nname = \"A\"\n[[scenes.groups]]\nid = 0\nname = \"G\"\n";
    match document::parse::<SceneCollection>(text) {
        Err(DataError::Invalid(why)) => assert!(why.to_string().contains("groups"), "{why}"),
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
}
