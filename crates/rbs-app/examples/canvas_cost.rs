//! What a second (vertical) canvas costs, ADR-0093 decision 10: the studio runs a
//! 1920x1080 main canvas at 60 fps and, with `2`, a 1080x1920 canvas showing the same
//! scene; each canvas records to its own file with the platform's encoders. It prints the
//! process CPU, peak memory, frames and drops, so the run with one canvas and the run with
//! two can be compared on the same machine (see the `measure-performance` skill).
//!
//! ```sh
//! cargo run --release -p rbs-app --example canvas_cost -- 1 30 /tmp/canvas-cost
//! cargo run --release -p rbs-app --example canvas_cost -- 2 30 /tmp/canvas-cost
//! ```
//!
//! Arguments: canvases (1 or 2), seconds (default 30), output folder (default the temp
//! folder). The sources are two colour pictures, one blended over the other, so no
//! device or permission is needed; set `RBS_BENCH_DISPLAY=1` to add the main display
//! (macOS asks for screen recording once). Run each case a few times and compare the
//! medians.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0055 rules the shell.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_record::{ExtraCanvas, OutputSettings, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
use rbs_sources::types::{self, keys};

const MAIN: (u32, u32) = (1920, 1080);
const TALL: (u32, u32) = (1080, 1920);

fn main() {
    let mut args = std::env::args().skip(1);
    let canvases: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let dir = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).expect("the output folder");
    assert!((1..=2).contains(&canvases), "canvases: 1 or 2");

    let mut col = SceneCollection::new();
    let (main, _) = col
        .add_canvas("Main", Size::new(MAIN.0, MAIN.1), FrameRate::FPS_60)
        .expect("canvas");
    let tall = (canvases == 2).then(|| {
        col.add_canvas("Vertical", Size::new(TALL.0, TALL.1), FrameRate::FPS_60)
            .expect("canvas")
            .0
    });
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).expect("scene");
    let mut add = |name: &str, kind: &'static str, settings: Settings, layout: Layout| {
        let (id, _) = col
            .add_source(name, NodeTypeId::new_static(kind), settings)
            .expect("source");
        col.add_item(scene, ItemContent::Source(id), layout, None)
            .expect("item");
    };
    let colour = |c: &str, c2: &str| -> Settings {
        [
            (keys::COLOR.to_owned(), SettingValue::Text(c.to_owned())),
            (keys::COLOR2.to_owned(), SettingValue::Text(c2.to_owned())),
            (keys::WIDTH.to_owned(), SettingValue::Int(i64::from(MAIN.0))),
            (
                keys::HEIGHT.to_owned(),
                SettingValue::Int(i64::from(MAIN.1)),
            ),
        ]
        .into_iter()
        .collect()
    };
    add(
        "Back",
        types::COLOR_SOURCE,
        colour("#203060", "#a04020"),
        Layout::new([0.0, 0.0], [MAIN.0 as f32, MAIN.1 as f32]),
    );
    add(
        "Front",
        types::COLOR_SOURCE,
        colour("#ffffff80", "#00000080"),
        Layout::new([480.0, 270.0], [960.0, 540.0]),
    );
    if std::env::var_os("RBS_BENCH_DISPLAY").is_some() {
        add(
            "Display",
            types::DISPLAY_SOURCE,
            Settings::new(),
            Layout::new([0.0, 0.0], [MAIN.0 as f32, MAIN.1 as f32]),
        );
    }
    col.set_program(main, Some(scene)).expect("program");
    if let Some(tall) = tall {
        col.set_program(tall, Some(scene)).expect("program");
    }

    let (mut adapters, notes) = rbs_app::platform::adapters();
    adapters.probe();
    for n in notes {
        eprintln!("note: {n}");
    }
    let gpu = Arc::new(rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default()).expect("a GPU"));
    println!("gpu: {}", gpu.adapter_name());
    let mut engine = rbs_engine::Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    if let Some(tall) = tall {
        settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    }
    let mut studio =
        Studio::start(engine, gpu, &col, &adapters.sources, &settings).expect("the studio starts");

    let mut outputs = Vec::new();
    for (name, canvas) in [("main", Some(main)), ("vertical", tall)] {
        let Some(canvas) = canvas else { continue };
        let path = dir.join(format!("{name}.mkv"));
        let mut out =
            OutputSettings::new(format!("file://{}", path.display()), ContainerId::Matroska);
        out.audio = None;
        out.canvas = Some(canvas);
        let id = studio
            .start_output(&adapters, &out)
            .expect("an output starts");
        outputs.push((name, id));
    }

    let cpu_start = cpu_time();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let serviced = studio.service();
        for (partition, event) in &serviced.events {
            eprintln!("event on {partition:?}: {event:?}");
        }
        std::thread::sleep(
            serviced
                .wait
                .unwrap_or(Duration::from_millis(50))
                .min(Duration::from_millis(50)),
        );
    }
    let wall = start.elapsed();
    let cpu = cpu_time().saturating_sub(cpu_start);
    println!(
        "canvases: {canvases}, wall {:.1} s, process CPU {:.1} % of one core, peak memory {:.0} MiB",
        wall.as_secs_f64(),
        100.0 * cpu.as_secs_f64() / wall.as_secs_f64(),
        peak_rss_mib()
    );
    for (name, id) in &outputs {
        let p = studio.output_progress(*id).expect("the output runs");
        println!(
            "{name}: exported {} (drops {}), encoded {} (skipped {}, drops {}), late render ticks {}",
            p.frames_exported,
            p.export_drops,
            p.frames_encoded,
            p.encoder_skipped,
            p.encoder_drops,
            p.late_render_ticks
        );
    }
    let (reports, errors) = studio.shutdown();
    for (_, r) in reports {
        if let Some(f) = r.failure {
            eprintln!("failure: {f}");
        }
    }
    if !errors.is_empty() {
        eprintln!("shutdown errors: {errors:?}");
    }
}

#[cfg(target_vendor = "apple")]
fn usage() -> libc::rusage {
    // SAFETY: getrusage fills the zeroed struct it is given; RUSAGE_SELF is valid.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    }
}

/// Process CPU time (user + system); zero where the demo has no reader.
#[cfg(target_vendor = "apple")]
fn cpu_time() -> Duration {
    let u = usage();
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    tv(u.ru_utime) + tv(u.ru_stime)
}

/// Peak resident memory in MiB (macOS reports bytes).
#[cfg(target_vendor = "apple")]
fn peak_rss_mib() -> f64 {
    usage().ru_maxrss as f64 / (1024.0 * 1024.0)
}

#[cfg(not(target_vendor = "apple"))]
fn cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(not(target_vendor = "apple"))]
fn peak_rss_mib() -> f64 {
    0.0
}
