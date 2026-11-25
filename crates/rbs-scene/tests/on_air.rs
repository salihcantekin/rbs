//! What is on air while an output runs (ADR-0052 decision 4): the pure reachability
//! the control API's on-air check uses.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

fn full() -> Layout {
    Layout::new([0.0, 0.0], [1920.0, 1080.0])
}

#[test]
fn program_scene_and_its_sources_are_on_air_others_are_not() {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Horizontal", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (cam, _) = col
        .add_source("Camera", ty("camera"), Settings::new())
        .unwrap();
    let (screen, _) = col
        .add_source("Screen", ty("screen-capture"), Settings::new())
        .unwrap();
    let (logo, _) = col
        .add_source("Logo", ty("image"), Settings::new())
        .unwrap();
    let (program, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (spare, _) = col.add_scene("Spare", SceneScope::Shared).unwrap();
    let (cam_item, _) = col
        .add_item(program, ItemContent::Source(cam), full(), None)
        .unwrap();
    let (logo_item, _) = col
        .add_item(program, ItemContent::Source(logo), full(), None)
        .unwrap();
    let (screen_item, _) = col
        .add_item(spare, ItemContent::Source(screen), full(), None)
        .unwrap();
    // The camera is shared with the spare scene: editing it changes the program.
    let (shared_item, _) = col
        .add_item(spare, ItemContent::Source(cam), full(), None)
        .unwrap();
    col.set_program(canvas, Some(program)).unwrap();

    let on_air = col.on_air();
    assert!(on_air.scenes.contains(&program));
    assert!(!on_air.scenes.contains(&spare));
    for item in [cam_item, logo_item] {
        assert!(on_air.items.contains(&item));
    }
    assert!(!on_air.items.contains(&screen_item));
    assert!(!on_air.items.contains(&shared_item));
    assert!(on_air.sources.contains(&cam));
    assert!(on_air.sources.contains(&logo));
    assert!(!on_air.sources.contains(&screen));
}

#[test]
fn nothing_is_on_air_without_a_program_scene() {
    let mut col = SceneCollection::new();
    col.add_canvas("Horizontal", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let on_air = col.on_air();
    assert!(on_air.scenes.is_empty() && on_air.items.is_empty() && on_air.sources.is_empty());
}
