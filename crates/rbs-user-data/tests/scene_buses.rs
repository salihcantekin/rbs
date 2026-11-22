//! Buses and sends in the scene collection document (format 29, ADR-0170).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{BusId, Settings};
use rbs_user_data::DataError;
use rbs_user_data::document::{self, Document};

const CANVAS: &str =
    "[[canvases]]\nid = 0\nname = \"Main\"\nwidth = 1920\nheight = 1080\nframe_rate = [30, 1]\n";
const SOURCE: &str = "[[sources]]\nid = 0\nname = \"Mic\"\ntype = \"org.rbs.source.microphone\"\n";

fn with_buses() -> SceneCollection {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let ty = || NodeTypeId::new("org.rbs.source.microphone");
    let (mic, _) = col.add_source("Mic", ty(), Settings::new()).unwrap();
    let (music, _) = col.add_source("Music", ty(), Settings::new()).unwrap();
    let (gone, _) = col.add_bus(canvas, "Gone").unwrap();
    col.remove_bus(canvas, gone).unwrap();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    col.add_bus(canvas, "Mic only").unwrap();
    col.set_bus_volume(canvas, vod, 0.5).unwrap();
    col.set_bus_muted(canvas, vod, true).unwrap();
    col.set_send(canvas, mic, vod, 0.8).unwrap();
    col.set_send(canvas, music, BusId::MAIN, 0.0).unwrap();
    col
}

#[test]
fn buses_and_sends_round_trip_as_format_29() {
    let col = with_buses();
    assert_eq!(col.write_format(), 29);
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.starts_with("format = 29"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);
    assert_eq!(back.id_counters(), col.id_counters());
}

#[test]
fn a_collection_without_buses_is_still_written_as_before() {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    assert!(col.write_format() < 29);
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(!text.contains("bus"), "{text}");
}

#[test]
fn a_format_28_file_loads_with_main_only() {
    let text = format!("format = 28\nkind = \"scene-collection\"\n{CANVAS}{SOURCE}");
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    let canvas = col.canvases().next().unwrap();
    assert!(canvas.buses.is_empty() && canvas.sends.is_empty());
}

#[test]
fn buses_in_a_format_before_29_are_refused() {
    let bus = "[[canvases.buses]]\nid = 1\nname = \"VOD\"\n";
    let text = format!("format = 28\nkind = \"scene-collection\"\n{CANVAS}{bus}");
    match document::parse::<SceneCollection>(&text) {
        Err(DataError::Invalid(why)) => assert!(why.to_string().contains("buses"), "{why}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn a_send_to_a_missing_bus_is_refused() {
    let send = "[[canvases.sends]]\nsource = 0\nbus = 4\nlevel = 0.5\n";
    let text = format!("format = 29\nkind = \"scene-collection\"\n{CANVAS}{send}{SOURCE}");
    assert!(matches!(
        document::parse::<SceneCollection>(&text),
        Err(DataError::Invalid(_))
    ));
}
