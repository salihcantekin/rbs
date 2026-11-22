//! Contract tests for buses and sends (scene model spec, ADR-0097 decision 9, ADR-0170).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{BusId, CanvasId, MAIN_BUS_NAME, MAX_BUSES, Settings, SourceId};
use rbs_scene::{SceneCollection, SceneError};

fn studio() -> (SceneCollection, CanvasId, SourceId, SourceId) {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let ty = || NodeTypeId::new("org.rbs.source.microphone");
    let (mic, _) = col.add_source("Mic", ty(), Settings::new()).unwrap();
    let (music, _) = col.add_source("Music", ty(), Settings::new()).unwrap();
    (col, canvas, mic, music)
}

#[test]
fn main_always_exists_and_its_name_is_reserved() {
    let (mut col, canvas, _, _) = studio();
    assert_eq!(col.bus_by_name(canvas, MAIN_BUS_NAME), Some(BusId::MAIN));
    assert_eq!(col.bus_name(canvas, BusId::MAIN), Some(MAIN_BUS_NAME));
    assert!(col.buses(canvas).unwrap().is_empty());
    assert_eq!(
        col.add_bus(canvas, MAIN_BUS_NAME).unwrap_err(),
        SceneError::DuplicateName(MAIN_BUS_NAME.into())
    );
    assert_eq!(
        col.remove_bus(canvas, BusId::MAIN).unwrap_err(),
        SceneError::UnknownBus(BusId::MAIN)
    );
}

#[test]
fn buses_are_capped_named_uniquely_and_ids_are_never_reused() {
    let (mut col, canvas, _, _) = studio();
    let (vod, undo) = col.add_bus(canvas, " VOD safe ").unwrap();
    assert_ne!(vod, BusId::MAIN);
    assert_eq!(col.bus_by_name(canvas, "VOD safe"), Some(vod));
    assert_eq!(
        col.add_bus(canvas, "VOD safe").unwrap_err(),
        SceneError::DuplicateName("VOD safe".into())
    );
    assert_eq!(
        col.add_bus(canvas, "  ").unwrap_err(),
        SceneError::EmptyName
    );
    col.undo(undo).unwrap();
    let (again, _) = col.add_bus(canvas, "VOD safe").unwrap();
    assert_ne!(again, vod, "a bus id is never reused");
    for i in 2..MAX_BUSES {
        col.add_bus(canvas, &format!("Bus {i}")).unwrap();
    }
    assert_eq!(col.buses(canvas).unwrap().len(), MAX_BUSES - 1);
    assert_eq!(
        col.add_bus(canvas, "One more").unwrap_err(),
        SceneError::TooManyBuses
    );
}

#[test]
fn sends_default_to_main_only_and_store_only_changes() {
    let (mut col, canvas, mic, music) = studio();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    assert_eq!(col.sends(canvas, mic).level(BusId::MAIN), 1.0);
    assert_eq!(col.sends(canvas, mic).level(vod), 0.0);
    col.set_send(canvas, mic, vod, 1.0).unwrap();
    let undo = col.set_send(canvas, music, BusId::MAIN, 0.5).unwrap();
    assert_eq!(col.sends(canvas, music).level(BusId::MAIN), 0.5);
    col.undo(undo).unwrap();
    assert!(col.sends(canvas, music).is_default());
    assert!(!col.canvas(canvas).unwrap().sends.contains_key(&music));
    assert_eq!(
        col.set_send(canvas, mic, vod, 1.5).unwrap_err(),
        SceneError::InvalidVolume
    );
    assert_eq!(
        col.set_send(canvas, mic, BusId::from_raw(99), 1.0)
            .unwrap_err(),
        SceneError::UnknownBus(BusId::from_raw(99))
    );
    assert_eq!(
        col.set_send(canvas, SourceId::from_raw(99), vod, 1.0)
            .unwrap_err(),
        SceneError::UnknownSource(SourceId::from_raw(99))
    );
}

#[test]
fn removing_a_bus_removes_its_sends_and_undo_restores_both() {
    let (mut col, canvas, mic, music) = studio();
    col.add_bus(canvas, "Stream").unwrap();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    col.set_send(canvas, mic, vod, 0.8).unwrap();
    col.set_bus_volume(canvas, vod, 0.5).unwrap();
    let before = col.clone();
    let undo = col.remove_bus(canvas, vod).unwrap();
    assert!(col.sends(canvas, mic).is_default());
    assert_eq!(col.bus_by_name(canvas, "VOD safe"), None);
    let redo = col.undo(undo).unwrap();
    assert_eq!(col, before, "same place, level and sends");
    col.undo(redo).unwrap();
    assert!(col.sends(canvas, music).is_default());
}

#[test]
fn deleting_a_source_removes_its_sends_and_undo_restores_them() {
    let (mut col, canvas, mic, _) = studio();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    col.set_send(canvas, mic, vod, 0.7).unwrap();
    let before = col.clone();
    let undo = col.delete_source(mic).unwrap();
    assert!(col.canvas(canvas).unwrap().sends.is_empty());
    col.undo(undo).unwrap();
    assert_eq!(col, before);
}

#[test]
fn bus_levels_are_live_controls_that_canvas_edits_keep() {
    let (mut col, canvas, _, _) = studio();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    let rename = col.rename_canvas(canvas, "Studio").unwrap();
    let generation = col.generation();
    col.set_bus_muted(canvas, vod, true).unwrap();
    col.set_bus_volume(canvas, vod, 0.25).unwrap();
    assert_eq!(col.generation(), generation + 2);
    assert_eq!(
        col.set_bus_volume(canvas, vod, 2.0).unwrap_err(),
        SceneError::InvalidVolume
    );
    // Undoing the earlier rename neither conflicts with nor reverts the level.
    col.undo(rename).unwrap();
    let bus = &col.buses(canvas).unwrap()[0];
    assert!(bus.level.muted);
    assert_eq!(bus.level.volume, 0.25);
    // A rename of the bus keeps its level too.
    let undo = col.rename_bus(canvas, vod, "No music").unwrap();
    assert_eq!(col.bus_by_name(canvas, "No music"), Some(vod));
    col.set_bus_volume(canvas, vod, 0.5).unwrap();
    col.undo(undo).unwrap();
    assert_eq!(col.bus_by_name(canvas, "VOD safe"), Some(vod));
    assert_eq!(col.buses(canvas).unwrap()[0].level.volume, 0.5);
}

#[test]
fn a_loaded_collection_issues_bus_ids_above_its_buses() {
    let (mut col, canvas, mic, _) = studio();
    let (vod, _) = col.add_bus(canvas, "VOD safe").unwrap();
    col.set_send(canvas, mic, vod, 0.5).unwrap();
    let mut counters = col.id_counters();
    counters.bus = 0;
    let loaded = SceneCollection::from_parts(
        col.canvases().cloned().collect(),
        col.sources().cloned().collect(),
        col.scenes().cloned().collect(),
        counters,
    )
    .unwrap();
    assert_eq!(loaded, col);
    assert!(loaded.id_counters().bus > vod.get());
    let mut loaded = loaded;
    let (next, _) = loaded.add_bus(canvas, "Other").unwrap();
    assert!(next.get() > vod.get());
}
