//! A slide show source in a running studio (ADR-0171): bound, fed by the hub, moved by
//! commands, edited live without a new node, and replaced when `transition` changes.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::path::{Path, PathBuf};

use rbs_core::media::ContainerId;
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::model::{ItemContent, Layout, SettingValue, Settings, SourceId};
use rbs_sources::slideshow::{FOLDER, SlideCommand};
use rbs_sources::types::{self, keys};
use support::*;

fn folder() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-record-slideshow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbs-sources/tests/data/plain-4x2.jpg");
    for i in 1..=3 {
        std::fs::copy(&src, dir.join(format!("{i}.jpg"))).unwrap();
    }
    dir
}

fn show_settings(dir: &Path, transition: &str) -> Settings {
    let mut s = Settings::new();
    s.insert(FOLDER.into(), SettingValue::Text(dir.display().to_string()));
    s.insert(keys::WIDTH.into(), SettingValue::Int(64));
    s.insert(keys::HEIGHT.into(), SettingValue::Int(32));
    s.insert("advance".into(), SettingValue::Text("manual".into()));
    s.insert("transition".into(), SettingValue::Text(transition.into()));
    s
}

fn node_of(studio: &Studio, source: SourceId) -> Option<rbs_graph::description::NodeId> {
    studio
        .sources()
        .into_iter()
        .find(|s| s.source == source)
        .map(|s| s.node)
}

#[test]
fn a_slide_show_runs_moves_on_command_and_takes_live_edits() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let dir = folder();
    let (mut col, canvas) = scene();
    let (show, _) = col
        .add_source(
            "Photos",
            NodeTypeId::new_static(types::SLIDESHOW_SOURCE),
            show_settings(&dir, "cut"),
        )
        .unwrap();
    let live = col.canvases().next().unwrap().program.unwrap();
    col.add_item(
        live,
        ItemContent::Source(show),
        Layout::new([0.0, 0.0], [64.0, 32.0]),
        None,
    )
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://slides", ContainerId::Matroska),
        )
        .unwrap();
    assert_eq!(studio.picture_size(show), Some((64, 32)));
    assert!(run_until(&mut studio, &clock, |s| {
        s.slideshow_state(show)
            .is_some_and(|st| st.index == Some(0) && st.count == 3)
    }));
    assert!(studio.slideshow_command(show, SlideCommand::Next));
    assert!(run_until(&mut studio, &clock, |s| {
        s.slideshow_state(show)
            .is_some_and(|st| st.index == Some(1))
    }));
    let node = node_of(&studio, show).unwrap();

    // A live key keeps the node.
    let mut looped = show_settings(&dir, "cut");
    looped.insert("loop".into(), SettingValue::Bool(false));
    col.set_source_settings(show, looped).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(node_of(&studio, show), Some(node));

    // `transition` is not live: a new node, which starts at the first picture.
    col.set_source_settings(show, show_settings(&dir, "fade"))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_ne!(node_of(&studio, show), Some(node));
    assert!(run_until(&mut studio, &clock, |s| {
        s.slideshow_state(show)
            .is_some_and(|st| st.index == Some(0))
    }));

    // Removed: the hub lets it go.
    col.delete_source(show).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.slideshow_state(show).is_none());
    assert!(!studio.slideshow_command(show, SlideCommand::Next));
    let report = studio.stop_output(out).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
