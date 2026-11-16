//! A text source in a running studio (ADR-0129 phase (b)): drawn by the worker,
//! uploaded in place, edited without a new node, and its file problem shown.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;

use rbs_core::media::ContainerId;
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings, TextDue};
use rbs_scene::model::{
    ItemContent, Layout, RecordSeparately, SceneScope, SettingValue, Settings, SourceId, VideoAs,
};
use rbs_sources::types::{self, keys};
use rbs_text_test::FakeTextRenderer;
use support::*;

fn text_settings(text: &str) -> Settings {
    let mut s = Settings::new();
    s.insert(keys::WIDTH.into(), SettingValue::Int(64));
    s.insert(keys::HEIGHT.into(), SettingValue::Int(24));
    s.insert(keys::SIZE.into(), SettingValue::Float(12.0));
    s.insert(keys::TEXT.into(), SettingValue::Text(text.into()));
    s
}

fn node_of(studio: &Studio, source: SourceId) -> Option<rbs_graph::description::NodeId> {
    studio
        .sources()
        .into_iter()
        .find(|s| s.source == source)
        .map(|s| s.node)
}

fn frames_of(studio: &Studio, name: &str) -> u64 {
    studio
        .progress()
        .sources
        .iter()
        .find(|s| s.name == name)
        .map_or(0, |s| s.frames)
}

#[test]
fn a_text_source_is_drawn_and_edited_in_place() {
    let Some(gpu) = gpu() else { return };
    let mut r = rig();
    r.adapters
        .sources
        .register_text(Arc::new(FakeTextRenderer::new()));
    let (mut col, canvas) = scene();
    let (title, _) = col
        .add_source(
            "Title",
            NodeTypeId::new_static(types::TEXT_SOURCE),
            text_settings("Hello"),
        )
        .unwrap();
    let live = col.canvases().next().unwrap().program.unwrap();
    col.add_item(
        live,
        ItemContent::Source(title),
        Layout::new([0.0, 0.0], [64.0, 24.0]),
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
            &OutputSettings::new("mem://text", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| s.picture_size(title)
        == Some((64, 24))));
    assert!(run_until(&mut studio, &clock, |s| frames_of(s, "Title") >= 1));
    let node = node_of(&studio, title).unwrap();
    let before = frames_of(&studio, "Title");

    // An edit keeps the node and puts a new picture on it.
    col.set_source_settings(title, text_settings("Bye"))
        .unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert_eq!(node_of(&studio, title), Some(node));
    assert!(run_until(&mut studio, &clock, |s| frames_of(s, "Title") > before));

    // A box edit too: the picture takes the new size on the same node.
    let mut bigger = text_settings("Bye");
    bigger.insert(keys::WIDTH.into(), SettingValue::Int(96));
    col.set_source_settings(title, bigger).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(node_of(&studio, title), Some(node));
    assert!(run_until(&mut studio, &clock, |s| s.picture_size(title)
        == Some((96, 24))));

    // A file that is not there is the source's problem, worded as for an image.
    let mut file = text_settings("");
    file.insert(
        keys::FILE.into(),
        SettingValue::Text("/nonexistent/rbs-text-source-test.txt".into()),
    );
    col.set_source_settings(title, file).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        s.sources()
            .iter()
            .any(|st| st.source == title && st.failed.as_deref() == Some("File not found"))
    }));

    // Removed: the worker lets the source go and the studio runs on.
    col.delete_source(title).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.picture_size(title).is_none());
    let report = studio.stop_output(out).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_switch_to_a_scene_with_a_text_waits_for_its_new_picture() {
    let Some(gpu) = gpu() else { return };
    let mut r = rig();
    r.adapters
        .sources
        .register_text(Arc::new(FakeTextRenderer::new()));
    let (mut col, canvas) = scene();
    let (title, _) = col
        .add_source(
            "Title",
            NodeTypeId::new_static(types::TEXT_SOURCE),
            text_settings("Next"),
        )
        .unwrap();
    // Only the scene that is not on air places the text, hidden.
    let (next, _) = col.add_scene("Next", SceneScope::Shared).unwrap();
    let (item, _) = col
        .add_item(
            next,
            ItemContent::Source(title),
            Layout::new([0.0, 0.0], [64.0, 24.0]),
            None,
        )
        .unwrap();
    col.set_item_visible(item, false).unwrap();
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
            &OutputSettings::new("mem://text-switch", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| s.picture_size(title)
        == Some((64, 24))));
    // Nothing enters for the scene on air.
    let live = col.canvases().next().unwrap().program.unwrap();
    assert!(!studio.text_prepare(canvas, live));
    assert_eq!(studio.text_due(), TextDue::Ready);

    // The text enters: drawn again now, and the switch waits until it is uploaded.
    assert!(studio.text_prepare(canvas, next));
    let mut due = TextDue::Wait;
    while due == TextDue::Wait {
        let mut stepped = false;
        assert!(run_until(&mut studio, &clock, |_| std::mem::replace(
            &mut stepped,
            true
        )));
        due = studio.text_due();
    }
    // Late only if the render thread missed two ticks; then it is counted.
    assert_eq!(studio.text_late(), u64::from(due == TextDue::Late));
    // A second prepare of the same scene has nothing new to wait for.
    assert!(!studio.text_prepare(canvas, next));

    // The switch itself keeps the picture: no second draw is asked for.
    col.set_program(canvas, Some(next)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.text_due(), TextDue::Ready);
    let report = studio.stop_output(out).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_text_read_from_a_document_is_adjusted_and_reported() {
    let Some(gpu) = gpu() else { return };
    let mut r = rig();
    r.adapters
        .sources
        .register_text(Arc::new(FakeTextRenderer::new()));
    let (mut col, canvas) = scene();
    // A size out of range and a clock marked to be recorded as images: values a
    // document can hold that no edit would accept.
    let mut settings = text_settings("12:00");
    settings.insert(keys::SIZE.into(), SettingValue::Float(1.0e6));
    settings.insert(keys::KIND.into(), SettingValue::Text("clock".into()));
    let (clock_text, _) = col
        .add_source(
            "Clock",
            NodeTypeId::new_static(types::TEXT_SOURCE),
            settings,
        )
        .unwrap();
    col.set_source_record_separately(
        clock_text,
        RecordSeparately::new(true, false).with_video_as(VideoAs::Image),
    )
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let keys: Vec<&str> = studio
        .source_adjusted()
        .iter()
        .filter(|a| a.source == clock_text)
        .map(|a| a.key.as_str())
        .collect();
    assert_eq!(keys, ["size", "video_as"]);

    // An edit that stores valid settings and the mark `auto` clears both.
    col.set_source_settings(clock_text, text_settings("Plain"))
        .unwrap();
    col.set_source_record_separately(clock_text, RecordSeparately::new(true, false))
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.source_adjusted().is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
