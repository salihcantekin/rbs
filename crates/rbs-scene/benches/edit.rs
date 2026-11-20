//! Cost of one collection edit on a studio-sized document (audit F-09).
//!
//! 300 sources with a few settings each, 10 scenes of 30 items. A field edit is a
//! volume change (a fader sample); a structural edit is a rename, which still
//! copies and validates the whole collection.
//!
//! Run: `cargo bench -p rbs-scene --bench edit`.

#![allow(clippy::expect_used)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings, SourceId};

const SOURCES: usize = 300;
const SCENES: usize = 10;
const ITEMS_PER_SCENE: usize = 30;

fn studio() -> (SceneCollection, SourceId) {
    let mut col = SceneCollection::new();
    col.add_canvas("Horizontal", Size::new(1920, 1080), FrameRate::FPS_60)
        .expect("canvas");
    let mut sources = Vec::with_capacity(SOURCES);
    for i in 0..SOURCES {
        let mut settings = Settings::new();
        settings.insert(
            "path".into(),
            SettingValue::Text(format!("/media/clip-{i}.mp4")),
        );
        settings.insert("loop".into(), SettingValue::Bool(i % 2 == 0));
        settings.insert("speed".into(), SettingValue::Float(1.0));
        settings.insert("start_ms".into(), SettingValue::Int(0));
        let (id, _) = col
            .add_source(
                &format!("Source {i}"),
                NodeTypeId::new("org.rbs.source.media"),
                settings,
            )
            .expect("source");
        sources.push(id);
    }
    for s in 0..SCENES {
        let (scene, _) = col
            .add_scene(&format!("Scene {s}"), SceneScope::Shared)
            .expect("scene");
        for i in 0..ITEMS_PER_SCENE {
            let source = sources[(s * ITEMS_PER_SCENE + i) % SOURCES];
            col.add_item(
                scene,
                ItemContent::Source(source),
                Layout::new([0.0, 0.0], [640.0, 360.0]),
                None,
            )
            .expect("item");
        }
    }
    (col, sources[SOURCES / 2])
}

fn edits(c: &mut Criterion) {
    let (mut col, source) = studio();
    let mut step = 0_u32;
    c.bench_function("field_edit_volume_300_sources", |b| {
        b.iter(|| {
            step = step.wrapping_add(1);
            let volume = (step % 10) as f32 * 0.1;
            black_box(col.set_source_volume(source, volume).expect("volume"));
        });
    });
    let mut step = 0_u32;
    c.bench_function("structural_edit_rename_300_sources", |b| {
        b.iter(|| {
            step = step.wrapping_add(1);
            let name = if step.is_multiple_of(2) {
                "Renamed A"
            } else {
                "Renamed B"
            };
            black_box(col.rename_source(source, name).expect("rename"));
        });
    });
}

criterion_group!(benches, edits);
criterion_main!(benches);
