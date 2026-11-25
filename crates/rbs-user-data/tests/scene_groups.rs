//! Groups in the scene collection document (format 30, ADR-0182).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{GroupScroll, ItemContent, Layout, SceneScope, ScrollDirection, Settings};
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

#[test]
fn group_scroll_round_trips_only_in_format_31_and_is_strict() {
    let mut col = grouped();
    let scene = col.scenes().next().unwrap().id;
    let canvas = col.canvases().next().unwrap().id;
    let group = col.groups(scene).unwrap()[0].id;
    let members: Vec<_> = col
        .scene(scene)
        .unwrap()
        .items
        .iter()
        .filter(|i| i.group == Some(group))
        .map(|i| i.id)
        .collect();
    col.set_group_scroll(
        scene,
        group,
        &members,
        canvas,
        Some(GroupScroll::new(ScrollDirection::Up, 125.0)),
    )
    .unwrap();
    let text = document::render(&col, None).unwrap();
    assert!(text.starts_with("format = 31"), "{text}");
    assert_eq!(
        document::parse::<SceneCollection>(&text).unwrap().document,
        col
    );
    assert!(
        document::parse::<SceneCollection>(&text.replacen("format = 31", "format = 30", 1))
            .is_err()
    );
    let mut value: toml::Value = toml::from_str(&text).unwrap();
    let groups = value["scenes"][0]["groups"].as_array_mut().unwrap();
    let scrolls = groups[0]["scrolls"].as_array_mut().unwrap();
    let entry = scrolls[0].clone();
    scrolls.push(entry);
    assert!(document::parse::<SceneCollection>(&toml::to_string(&value).unwrap()).is_err());
    let scrolls = value["scenes"][0]["groups"][0]["scrolls"]
        .as_array_mut()
        .unwrap();
    scrolls.pop();
    scrolls[0]["scroll"]
        .as_table_mut()
        .unwrap()
        .insert("on_return".into(), toml::Value::String("restart".into()));
    assert!(document::parse::<SceneCollection>(&toml::to_string(&value).unwrap()).is_err());
    let scrolls = value["scenes"][0]["groups"][0]["scrolls"]
        .as_array_mut()
        .unwrap();
    scrolls[0]["scroll"]
        .as_table_mut()
        .unwrap()
        .remove("on_return");
    scrolls[0]["canvas"] = toml::Value::Integer(999);
    assert!(document::parse::<SceneCollection>(&toml::to_string(&value).unwrap()).is_err());
    let scrolls = value["scenes"][0]["groups"][0]["scrolls"]
        .as_array_mut()
        .unwrap();
    scrolls[0]["canvas"] = toml::Value::Integer(i64::from(canvas.get()));
    scrolls[0]["scroll"]["speed"] = toml::Value::Float(0.0);
    assert!(document::parse::<SceneCollection>(&toml::to_string(&value).unwrap()).is_err());
    col.set_group_scroll(scene, group, &members, canvas, None)
        .unwrap();
    assert_eq!(col.write_format(), 30);
}
