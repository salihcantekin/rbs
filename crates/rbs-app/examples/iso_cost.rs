//! What separate video files cost the master recording (ADR-0086 step 3, decision 12;
//! P15 measurements). The studio runs a 1920x1080 main canvas at 60 fps and records it
//! with the platform's encoders. The scene has a background, a picture over it and four
//! "camera" pictures, so the scene's own cost is the same in every run; `tracks` marks
//! that many cameras to record separately. It prints the render tick times (how much of
//! the 16.7 ms period each tick used), the process CPU, peak memory, and the master's
//! frames and drops, so runs with 0, 1, 2 and 4 tracks compare on one machine (see the
//! `measure-performance` skill).
//!
//! ```sh
//! cargo run --release -p rbs-app --example iso_cost -- 0 30 /tmp/iso-cost
//! ```
//!
//! Arguments: tracks (0 to 4), seconds (default 30), output folder (default the temp
//! folder). The sources are colour pictures, so no device or permission is needed. Run
//! each case a few times and compare the medians. `0` is the "before" of every later
//! step. The files take the master's encoder settings (decision 4).

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0055 rules the shell.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_core::media::{ContainerId, Size};
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_record::{IsoVideoStart, OutputSettings, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, RecordSeparately, SceneScope, SettingValue, Settings};
use rbs_sources::types::{self, keys};

const MAIN: (u32, u32) = (1920, 1080);
/// Pictures that could be marked to record separately.
const CAMERAS: usize = 4;

fn main() {
    let mut args = std::env::args().skip(1);
    let tracks: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let dir = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).expect("the output folder");
    assert!(tracks <= CAMERAS, "tracks: 0 to {CAMERAS}");

    let mut col = SceneCollection::new();
    let (main, _) = col
        .add_canvas("Main", Size::new(MAIN.0, MAIN.1), FrameRate::FPS_60)
        .expect("canvas");
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).expect("scene");
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
    let mut add = |name: &str, settings: Settings, layout: Layout| {
        let (id, _) = col
            .add_source(name, NodeTypeId::new_static(types::COLOR_SOURCE), settings)
            .expect("source");
        col.add_item(scene, ItemContent::Source(id), layout, None)
            .expect("item");
        id
    };
    add(
        "Back",
        colour("#203060", "#a04020"),
        Layout::new([0.0, 0.0], [MAIN.0 as f32, MAIN.1 as f32]),
    );
    add(
        "Front",
        colour("#ffffff80", "#00000080"),
        Layout::new([480.0, 270.0], [960.0, 540.0]),
    );
    // Four 1080p pictures in the corners, at a quarter of the canvas each.
    let (qw, qh) = (MAIN.0 as f32 / 4.0, MAIN.1 as f32 / 4.0);
    let cameras: Vec<_> = (0..CAMERAS)
        .map(|i| {
            let x = if i % 2 == 0 { 0.0 } else { MAIN.0 as f32 - qw };
            let y = if i < 2 { 0.0 } else { MAIN.1 as f32 - qh };
            add(
                &format!("Camera {}", i + 1),
                colour("#30a050", "#5030a0"),
                Layout::new([x, y], [qw, qh]),
            )
        })
        .collect();
    col.set_program(main, Some(scene)).expect("program");
    let marked = &cameras[..tracks];
    for id in marked {
        col.set_source_record_separately(*id, RecordSeparately::new(true, false))
            .expect("mark");
    }

    let (mut adapters, notes) = rbs_app::platform::adapters();
    adapters.probe();
    for n in notes {
        eprintln!("note: {n}");
    }
    // `RBS_BENCH_SOFTWARE=1` accepts a CPU adapter (lavapipe), only to check the demo
    // runs on a machine without a GPU; its numbers mean nothing.
    let request = rbs_gpu::GpuRequest {
        allow_software: std::env::var_os("RBS_BENCH_SOFTWARE").is_some(),
        ..rbs_gpu::GpuRequest::default()
    };
    let gpu = Arc::new(rbs_gpu::GpuContext::new(&request).expect("a GPU"));
    println!("gpu: {}", gpu.adapter_name());
    let mut engine = rbs_engine::Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio =
        Studio::start(engine, gpu, &col, &adapters.sources, &settings).expect("the studio starts");

    let path = dir.join("master.mkv");
    let mut out = OutputSettings::new(format!("file://{}", path.display()), ContainerId::Matroska);
    out.audio = None;
    out.canvas = Some(main);
    let master = studio
        .start_output(&adapters, &out)
        .expect("the master starts");
    // The sources report their picture once the master draws them.
    let warm = Instant::now();
    while marked.iter().any(|id| studio.picture_size(*id).is_none()) {
        assert!(warm.elapsed() < Duration::from_secs(10), "the cameras draw");
        studio.service();
        std::thread::sleep(Duration::from_millis(10));
    }
    let iso = if tracks == 0 {
        Vec::new()
    } else {
        let folder = format!("file://{}", dir.display());
        let video = out.video.clone().unwrap_or_default();
        studio
            .start_iso_video(
                &adapters,
                &IsoVideoStart::new(folder, ContainerId::Matroska, video),
            )
            .expect("the separate files start")
            .tracks
    };
    println!("tracks: {} ({marked:?})", iso.len());

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
        "wall {:.1} s, process CPU {:.1} % of one core, peak memory {:.0} MiB",
        wall.as_secs_f64(),
        100.0 * cpu.as_secs_f64() / wall.as_secs_f64(),
        peak_rss_mib()
    );
    if let Some(t) = studio.render_tick_times() {
        let mean = if t.ticks == 0 {
            Duration::ZERO
        } else {
            t.sum / u32::try_from(t.ticks).unwrap_or(u32::MAX)
        };
        println!(
            "render ticks {}: mean {:.2} ms, max {:.2} ms, 50 % under {} % and 99 % under {} % of the period, by quarter {:?}",
            t.ticks,
            mean.as_secs_f64() * 1e3,
            t.max.as_secs_f64() * 1e3,
            t.share_under(0.5),
            t.share_under(0.99),
            t.buckets
        );
    }
    let p = studio.output_progress(master).expect("the master runs");
    println!(
        "master: exported {} (drops {}), encoded {} (skipped {}, drops {}), late render ticks {}",
        p.frames_exported,
        p.export_drops,
        p.frames_encoded,
        p.encoder_skipped,
        p.encoder_drops,
        p.late_render_ticks
    );
    for t in &iso {
        use std::sync::atomic::Ordering::Relaxed;
        println!(
            "file {} {}x{}: frames {}, gave way {}, lost {}",
            t.file_name,
            t.size.0,
            t.size.1,
            t.stats.frames.load(Relaxed),
            t.stats.gave_way.load(Relaxed),
            t.stats.lost.load(Relaxed)
        );
    }
    studio.stop_iso_video();
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

/// Process CPU time (user + system) so far.
#[cfg(windows)]
fn cpu_time() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut t = [FILETIME::default(); 4];
    // SAFETY: GetProcessTimes fills the four FILETIMEs it is given for our own process.
    let ok = unsafe {
        let [create, exit, kernel, user] = &mut t;
        GetProcessTimes(GetCurrentProcess(), create, exit, kernel, user).is_ok()
    };
    if !ok {
        return Duration::ZERO;
    }
    // 100 ns units.
    let ft = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Duration::from_nanos((ft(t[2]) + ft(t[3])) * 100)
}

/// Peak working set in MiB.
#[cfg(windows)]
fn peak_rss_mib() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: fills the counters struct of the given size for our own process.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).is_ok() };
    if ok {
        c.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        0.0
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

/// Process CPU time (user + system) so far.
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

/// Windows and macOS only (the owner's targets); elsewhere the demo prints zero.
#[cfg(not(any(windows, target_vendor = "apple")))]
fn cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(not(any(windows, target_vendor = "apple")))]
fn peak_rss_mib() -> f64 {
    0.0
}
