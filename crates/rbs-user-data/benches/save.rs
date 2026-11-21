//! What one autosave of a studio-sized scene collection costs on the writer thread
//! (audit F-09): rendering the document, and the checks `write` runs on the previous
//! file before replacing it (one TOML parse for the envelope, one full `parse`).
//!
//! 300 sources with a few settings each, 10 scenes of 30 items (the `rbs-scene` `edit`
//! bench's document). Each round changes one volume, so every save replaces a file
//! whose bytes differ. Prints p50 and max over `RBS_BENCH_ROUNDS` rounds (default 50)
//! for: render alone, `parse` of the previous file alone, a whole `Library::save`
//! (render, checks, atomic write to `RBS_BENCH_DIR`, default the temporary directory),
//! the same through `Library::save_known` as the app's writer thread saves (the file
//! it wrote last is not parsed again), and a `save_known` of an unchanged document
//! (nothing is written).
//!
//! `cargo bench -p rbs-user-data --bench save`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings, SourceId};
use rbs_user_data::{Entry, UserData, document};

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

fn main() {
    let rounds: usize = std::env::var("RBS_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .max(1);
    let base = std::env::var_os("RBS_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("rbs-save-bench-{}", std::process::id()));
    let (mut col, source) = studio();
    let data = UserData::new(dir.clone());
    let lock = data.try_lock().expect("writer lock").expect("not held");
    let lib = data.scene_collections();
    let id = lib.create(&lock, "Studio", &col).expect("create");
    // `save_known` saves another entry: a plain `save` of the same file in between
    // would make every file one the known writer did not write.
    let other = lib.create(&lock, "Other", &col).expect("create");
    let text = document::render(&col, Some("Studio")).expect("render");

    let mut render = Vec::with_capacity(rounds);
    let mut parse = Vec::with_capacity(rounds);
    let mut save = Vec::with_capacity(rounds);
    let mut known = Vec::with_capacity(rounds);
    let mut unchanged = Vec::with_capacity(rounds);
    let mut written = document::Written::default();
    for round in 0..rounds {
        col.set_source_volume(source, (round % 10) as f32 * 0.1)
            .expect("volume");
        let start = Instant::now();
        std::hint::black_box(document::render(&col, Some("Studio")).expect("render"));
        render.push(start.elapsed());
        let start = Instant::now();
        std::hint::black_box(document::parse::<SceneCollection>(&text).expect("parse"));
        parse.push(start.elapsed());
        let entry = Entry::new("Studio".to_owned(), col.clone());
        let start = Instant::now();
        lib.save(&lock, &id, &entry).expect("save");
        save.push(start.elapsed());
        col.set_source_volume(source, 0.05 + (round % 10) as f32 * 0.1)
            .expect("volume");
        let entry = Entry::new("Other".to_owned(), col.clone());
        let start = Instant::now();
        lib.save_known(&lock, &other, &entry, &mut written)
            .expect("save");
        known.push(start.elapsed());
        let start = Instant::now();
        lib.save_known(&lock, &other, &entry, &mut written)
            .expect("save");
        unchanged.push(start.elapsed());
    }
    drop(lock);
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "rbs bench: collection of {SOURCES} sources, {} KiB on disk",
        text.len() / 1024
    );
    for (what, mut times) in [
        ("render", render),
        ("parse previous", parse),
        ("save", save),
        ("save_known", known),
        ("save_known unchanged", unchanged),
    ] {
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        println!(
            "rbs bench: {what}: rounds={} ms p50={:.3} max={:.3}",
            times.len(),
            ms(times[times.len() / 2]),
            ms(times[times.len() - 1])
        );
    }
}
