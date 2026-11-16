//! What projectors cost the studio (ADR-0125 decision 12; P15 measurements). The studio
//! runs a main canvas (1920x1080 or 3840x2160) at 60 fps with a background and a picture
//! over it, and adds `pictures` projector pictures at the canvas's size, as the host does
//! for open projectors. Each picture has a subscriber that takes every frame and lets it
//! go at once, as a projector window does. It prints the render tick times (how much of
//! the 16.7 ms period each tick used), the process CPU and peak memory, so runs with 0,
//! 1, 2 and 4 pictures compare on one machine (see the `measure-performance` skill).
//! No window opens.
//!
//! ```sh
//! cargo run --release -p rbs-app --example projector_cost -- 1 10 1080
//! ```
//!
//! Arguments: pictures (0 to 4), seconds (default 10), canvas height 1080 or 2160
//! (default 1080). `0` is the "before". Run each case a few times and compare medians.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0055 rules the shell.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_gpu::share::{MAX_PREVIEW_SURFACES, PreviewDesc, ring_needed};
use rbs_graph::node::NodeTypeId;
use rbs_record::{Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
use rbs_sources::types::{self, keys};

fn main() {
    let mut args = std::env::args().skip(1);
    let pictures: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let tall: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1080);
    assert!(pictures <= 4, "pictures: 0 to 4");
    let main_size = match tall {
        2160 => (3840, 2160),
        _ => (1920, 1080),
    };
    let fps = FrameRate::FPS_60;

    let mut col = SceneCollection::new();
    let (main, _) = col
        .add_canvas("Main", Size::new(main_size.0, main_size.1), fps)
        .expect("canvas");
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).expect("scene");
    let colour = |c: &str, c2: &str| -> Settings {
        [
            (keys::COLOR.to_owned(), SettingValue::Text(c.to_owned())),
            (keys::COLOR2.to_owned(), SettingValue::Text(c2.to_owned())),
            (
                keys::WIDTH.to_owned(),
                SettingValue::Int(i64::from(main_size.0)),
            ),
            (
                keys::HEIGHT.to_owned(),
                SettingValue::Int(i64::from(main_size.1)),
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
    };
    let (w, h) = (main_size.0 as f32, main_size.1 as f32);
    add(
        "Back",
        colour("#203060", "#a04020"),
        Layout::new([0.0, 0.0], [w, h]),
    );
    add(
        "Front",
        colour("#ffffff80", "#00000080"),
        Layout::new([w / 4.0, h / 4.0], [w / 2.0, h / 2.0]),
    );
    col.set_program(main, Some(scene)).expect("program");

    let (mut adapters, notes) = rbs_app::platform::adapters();
    adapters.probe();
    for n in notes {
        eprintln!("note: {n}");
    }
    // `RBS_BENCH_SOFTWARE=1` accepts a CPU adapter, only to check the demo runs on a
    // machine without a GPU; its numbers mean nothing.
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

    // The host's ring for a projector picture (studio_host `preview_surfaces`).
    let interval = fps.frame_duration();
    let desc = PreviewDesc {
        width: main_size.0,
        height: main_size.1,
        surfaces: ring_needed(interval, hold(interval)).min(MAX_PREVIEW_SURFACES),
    };
    let ids: Vec<_> = (0..pictures)
        .map(|_| {
            studio
                .add_picture(&col, &adapters.sources, main, desc)
                .expect("the picture joins")
                .0
        })
        .collect();
    let warm = Instant::now();
    while ids.iter().any(|id| studio.picture(*id).is_none()) {
        assert!(
            warm.elapsed() < Duration::from_secs(10),
            "the pictures start"
        );
        studio.service();
        std::thread::sleep(Duration::from_millis(10));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = ids
        .iter()
        .map(|id| {
            let endpoint = studio.picture(*id).expect("an endpoint");
            let (stop, frames) = (Arc::clone(&stop), Arc::clone(&frames));
            std::thread::spawn(move || read(endpoint, &stop, &frames))
        })
        .collect();
    println!(
        "canvas {}x{}, pictures {pictures} ({} surfaces each)",
        main_size.0, main_size.1, desc.surfaces
    );

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
    stop.store(true, Ordering::Relaxed);
    println!(
        "wall {:.1} s, process CPU {:.1} % of one core, peak memory {:.0} MiB, frames received {}",
        wall.as_secs_f64(),
        100.0 * cpu.as_secs_f64() / wall.as_secs_f64(),
        peak_rss_mib(),
        frames.load(Ordering::Relaxed)
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
    for r in readers {
        let _ = r.join();
    }
    for id in ids {
        let _ = studio.remove_picture(&col, &adapters.sources, id);
    }
    let (_, errors) = studio.shutdown();
    if !errors.is_empty() {
        eprintln!("shutdown errors: {errors:?}");
    }
}

/// How long a projector window's layer holds a frame (studio_host `preview_hold`).
#[cfg(windows)]
fn hold(interval: Duration) -> Duration {
    interval.saturating_add(rbs_gpu::share::DEFAULT_RETIRE_POLL)
}

#[cfg(target_os = "macos")]
fn hold(_: Duration) -> Duration {
    rbs_gpu::share::DEFAULT_RETIRE_AFTER
}

#[cfg(not(any(windows, target_os = "macos")))]
fn hold(_: Duration) -> Duration {
    Duration::from_millis(34)
}

/// Takes every frame of a picture and lets it go at once, as a projector window does.
#[cfg(any(windows, target_vendor = "apple"))]
fn read(endpoint: rbs_gpu::share::PreviewEndpoint, stop: &AtomicBool, frames: &AtomicU64) {
    let mut sub = rbs_gpu::share::PreviewSubscriber::connect(&endpoint, Duration::from_secs(5))
        .expect("the picture accepts a subscriber");
    while !stop.load(Ordering::Relaxed) {
        match sub.recv(Duration::from_millis(100)) {
            Ok(Some(frame)) => {
                drop(frame);
                frames.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("picture ended: {e}");
                return;
            }
        }
    }
}

/// No subscriber elsewhere: the pictures are published with nobody reading.
#[cfg(not(any(windows, target_vendor = "apple")))]
fn read(_: rbs_gpu::share::PreviewEndpoint, _: &AtomicBool, _: &AtomicU64) {}

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
