//! How long a capture source that runs only when shown takes to show its first picture
//! again (ADR-0152 phase 4, audit F-07): a display or camera with `run: shown` goes on
//! and off air a few times, and each wake prints its warm-up (`Studio::warm_up`, the
//! capture node's time from `prepare` to the first frame) and the wall time from the
//! cut to the source's open session.
//!
//! ```sh
//! cargo run --release -p rbs-app --example warm_up -- display 5
//! cargo run --release -p rbs-app --example warm_up -- camera 5
//! ```
//!
//! Arguments: `display` (default) or `camera`, then the number of wakes (default 5).
//! macOS asks for screen recording or camera access once; without it the source never
//! opens and the run says so.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0055 rules the shell.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::activity::{Run, SourceActivity};
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings};
use rbs_sources::types;

const SIZE: (u32, u32) = (1920, 1080);
/// The longest a wake may take before the run gives up on it.
const OPEN_LIMIT: Duration = Duration::from_secs(20);

fn main() {
    let mut args = std::env::args().skip(1);
    let kind = args.next().unwrap_or_else(|| "display".to_owned());
    let wakes: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let type_id = match kind.as_str() {
        "camera" => types::CAMERA_SOURCE,
        _ => types::DISPLAY_SOURCE,
    };

    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(SIZE.0, SIZE.1), FrameRate::FPS_30)
        .expect("canvas");
    let (live, _) = col.add_scene("Live", SceneScope::Shared).expect("scene");
    let (empty, _) = col.add_scene("Empty", SceneScope::Shared).expect("scene");
    let (source, _) = col
        .add_source("Capture", NodeTypeId::new_static(type_id), Settings::new())
        .expect("source");
    col.add_item(
        live,
        ItemContent::Source(source),
        Layout::new([0.0, 0.0], [SIZE.0 as f32, SIZE.1 as f32]),
        None,
    )
    .expect("item");
    col.set_source_activity(source, SourceActivity::new(Some(Run::Shown), None, None))
        .expect("activity");
    col.set_program(canvas, Some(empty)).expect("program");

    let (mut adapters, notes) = rbs_app::platform::adapters();
    adapters.probe();
    for n in notes {
        rbs_crash::errln!("note: {n}");
    }
    let gpu = Arc::new(rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default()).expect("a GPU"));
    rbs_crash::outln!("gpu: {}, source: {kind}", gpu.adapter_name());
    let mut engine = rbs_engine::Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio =
        Studio::start(engine, gpu, &col, &adapters.sources, &settings).expect("the studio starts");
    let path = std::env::temp_dir().join("rbs-warm-up.mkv");
    let mut out = OutputSettings::new(format!("file://{}", path.display()), ContainerId::Matroska);
    out.audio = None;
    let output = studio.start_output(&adapters, &out).expect("an output starts");

    let mut warm_ups = Vec::new();
    for wake in 1..=wakes {
        // Off air: the session closes.
        col.set_program(canvas, Some(empty)).expect("program");
        studio.update(&col, &adapters.sources).expect("update");
        studio.end_transitions();
        studio.update(&col, &adapters.sources).expect("update");
        run_for(&mut studio, Duration::from_secs(2), |s| s.picture_size(source).is_none());
        let closed = studio.picture_size(source).is_none();

        // The cut: the session opens on its own thread.
        let cut = Instant::now();
        col.set_program(canvas, Some(live)).expect("program");
        studio.update(&col, &adapters.sources).expect("update");
        let opened = run_for(&mut studio, OPEN_LIMIT, |s| s.picture_size(source).is_some());
        let open_wall = cut.elapsed();
        // The first frame comes after the open; give it a moment, then read the warm-up.
        run_for(&mut studio, Duration::from_millis(1500), |_| false);
        let warm_up = studio.warm_up(source);
        rbs_crash::outln!(
            "wake {wake}: closed before {closed}, opened {opened} after {:.0} ms (wall), warm-up {}",
            open_wall.as_secs_f64() * 1000.0,
            warm_up.map_or("none".to_owned(), |w| format!(
                "{:.0} ms",
                w.as_secs_f64() * 1000.0
            ))
        );
        if let Some(w) = warm_up.filter(|_| opened) {
            warm_ups.push(w);
        }
    }
    warm_ups.sort();
    if let (Some(lo), Some(hi)) = (warm_ups.first(), warm_ups.last()) {
        let median = warm_ups[warm_ups.len() / 2];
        rbs_crash::outln!(
            "{kind}: {} wakes, warm-up min {:.0} ms, median {:.0} ms, max {:.0} ms",
            warm_ups.len(),
            lo.as_secs_f64() * 1000.0,
            median.as_secs_f64() * 1000.0,
            hi.as_secs_f64() * 1000.0
        );
    } else {
        rbs_crash::errln!("{kind}: no wake opened (permission, or no such device?)");
    }
    let _ = studio.stop_output(output);
    let (_, errors) = studio.shutdown();
    if !errors.is_empty() {
        rbs_crash::errln!("shutdown errors: {errors:?}");
    }
    let _ = std::fs::remove_file(&path);
}

/// Services the studio until `done` or `limit`; true when `done`.
fn run_for(studio: &mut Studio, limit: Duration, done: impl Fn(&Studio) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done(studio) {
            return true;
        }
        let serviced = studio.service();
        std::thread::sleep(
            serviced
                .wait
                .unwrap_or(Duration::from_millis(5))
                .min(Duration::from_millis(5)),
        );
    }
    done(studio)
}
