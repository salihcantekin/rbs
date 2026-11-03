//! `rbs-cli`: the headless RBS command line (ADR-0040, spec `docs/specs/recording.md`).
//!
//! ```text
//! rbs-cli record C:\Videos\rec.mkv --seconds 60
//! rbs-cli devices
//! ```
//!
//! `record` captures the main display with the first camera in the bottom-right corner,
//! the default microphone and the system audio, and writes H.264 and AAC to a Matroska
//! (`.mkv`) or fragmented MP4 (`.mp4`) file until the time is up or Enter is pressed.

#![allow(clippy::print_stdout, clippy::print_stderr)] // A command-line program prints.

mod args;
mod platform;
mod scene;

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rbs_codec::RateControl;
use rbs_core::capability::Requirement;
use rbs_core::registry::{AdapterFactory, AdapterId, Registry};
use rbs_engine::Engine;
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_record::{AudioSettings, Progress, RecordAdapters, RecordSettings, Recording};

use crate::args::{Command, RecordArgs};

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(raw.iter().map(String::as_str)) {
        Ok(Command::Help) => {
            println!("{}", args::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Devices) => devices(),
        Ok(Command::Record(a)) => match record(&a) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("record failed: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("{e}\n\n{}", args::USAGE);
            ExitCode::from(2)
        }
    }
}

fn probed() -> RecordAdapters {
    let (mut adapters, errors) = platform::adapters();
    for e in errors {
        eprintln!("warning: {e}");
    }
    adapters.probe();
    adapters
}

fn list_registry<F: ?Sized + AdapterFactory>(title: &str, registry: &Registry<F>) {
    println!("{title}:");
    let ids: Vec<&AdapterId> = registry.ids().collect();
    if ids.is_empty() {
        println!("  (none on this platform yet)");
    }
    for id in ids {
        let state = if registry.capabilities(id).is_some() {
            "available"
        } else {
            "unavailable"
        };
        println!("  {id} ({state})");
    }
}

fn devices() -> ExitCode {
    let adapters = probed();
    println!("sources:");
    for t in adapters.sources.installed() {
        println!("  {t}");
        let targets = adapters.sources.targets(&t).unwrap_or_default();
        for (i, target) in targets.iter().enumerate() {
            println!(
                "    {i}: {} [{}]{}",
                target.name,
                target.adapter,
                if target.is_default { " (default)" } else { "" }
            );
        }
    }
    list_registry("video encoders", &adapters.video_encoders);
    list_registry("audio encoders", &adapters.audio_encoders);
    list_registry("muxers", &adapters.muxers);
    list_registry("transports", &adapters.transports);
    ExitCode::SUCCESS
}

/// The registered id `wanted` names: the id itself or its last dotted part.
fn resolve<F: ?Sized + AdapterFactory>(
    registry: &Registry<F>,
    wanted: &str,
) -> Result<AdapterId, String> {
    let ids: Vec<&AdapterId> = registry.ids().collect();
    ids.iter()
        .find(|id| id.as_str() == wanted || id.as_str().rsplit('.').next() == Some(wanted))
        .map(|id| (*id).clone())
        .ok_or_else(|| {
            let known: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
            format!("no encoder {wanted:?}; known: {}", known.join(", "))
        })
}

fn db(peak: Option<f32>) -> String {
    match peak {
        Some(p) if p.is_finite() => format!("{p:.1} dB"),
        _ => "-inf dB".to_owned(),
    }
}

fn line(elapsed: Duration, p: &Progress) -> String {
    let mut s = format!(
        "{:>4}s  video {} frames ({} dropped)",
        elapsed.as_secs(),
        p.frames_encoded,
        p.export_drops + p.encoder_drops,
    );
    for src in &p.sources {
        match src.peak_db {
            Some(_) => s.push_str(&format!(" | {} {}", src.name, db(src.peak_db))),
            None => s.push_str(&format!(" | {} {} frames", src.name, src.frames)),
        }
    }
    s.push_str(&format!(
        " | {} packets | late {}/{}",
        p.packets_muxed(),
        p.late_render_ticks,
        p.late_audio_ticks
    ));
    s
}

fn record(a: &RecordArgs) -> Result<(), String> {
    let (mut adapters, errors) = platform::adapters();
    for e in errors {
        eprintln!("warning: {e}");
    }
    let wanted = match &a.encoder {
        Some(name) => Some(resolve(&adapters.video_encoders, name)?),
        None => None,
    };
    adapters.probe();
    if let Some(id) = wanted {
        // Only the named encoder: if it cannot open, the recording fails rather than
        // quietly using another one.
        let candidates = adapters
            .video_encoders
            .select(&Requirement::new())
            .candidates;
        let factory = candidates
            .into_iter()
            .find(|f| f.id() == id)
            .ok_or_else(|| format!("encoder {id} is not available on this system"))?;
        let mut only = Registry::new();
        only.register(factory).map_err(|e| e.to_string())?;
        only.probe_all();
        adapters.video_encoders = only;
    }
    if adapters.video_encoders.ids().next().is_none()
        || (!a.no_audio && adapters.audio_encoders.ids().next().is_none())
    {
        return Err(format!(
            "recording is not available on {} yet (no encoder adapters are wired)",
            std::env::consts::OS
        ));
    }

    let built = scene::build(&adapters.sources, a)?;
    for s in &built.skipped {
        println!("left out: {s}");
    }
    if a.duration.is_none() && !std::io::stdin().is_terminal() {
        return Err("without a terminal nothing can press Enter: pass --seconds".into());
    }
    let out = std::path::absolute(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
    let existed = out.exists();
    if existed && !a.overwrite {
        return Err(format!(
            "{} exists; pass --overwrite to replace it",
            out.display()
        ));
    }
    let mut settings = RecordSettings::new(
        built.canvas,
        rbs_transport_file::file_url(&out),
        a.container,
    );
    settings.video.rate_control = RateControl::Cbr {
        bitrate_bps: a.video_bitrate,
    };
    settings.audio = (!a.no_audio).then(|| {
        let mut audio = AudioSettings::default();
        audio.bitrate_bps = a.audio_bitrate;
        audio
    });
    settings.audio_offsets = built.audio_offsets.clone();
    for (name, ms) in [
        ("microphone", a.mic_offset_ms),
        ("system audio", a.system_audio_offset_ms),
    ] {
        if ms != 0 {
            println!("{name} sync offset: {ms} ms");
        }
    }

    let gpu = Arc::new(
        GpuContext::new(&GpuRequest::default()).map_err(|e| format!("no usable GPU: {e}"))?,
    );
    println!("gpu: {}", gpu.adapter_name());
    let mut engine = Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut recording = match Recording::start(engine, gpu, &built.collection, &adapters, &settings)
    {
        Ok(r) => r,
        Err(e) => {
            // A failed start may have created the file; leave no empty file behind.
            if !existed && out.exists() {
                let _ = std::fs::remove_file(&out);
            }
            return Err(e.to_string());
        }
    };
    let started = recording.started().clone();
    println!(
        "video encoder: {} | audio encoder: {} | muxer: {} | transport: {}",
        started.video_encoder.name,
        started
            .audio_encoder
            .as_ref()
            .map_or("none", |c| c.name.as_str()),
        started.muxer.name,
        started.transport.name
    );
    for s in &started.sources {
        match &s.failed {
            None => println!("source {}: {} [{}]", s.name, s.target, s.adapter),
            Some(why) => println!("source {}: did not start: {why}", s.name),
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    match a.duration {
        Some(d) => println!(
            "recording {} s to {} (press Enter to stop early)",
            d.as_secs(),
            out.display()
        ),
        None => println!("recording to {}; press Enter to stop", out.display()),
    }
    {
        let stop = Arc::clone(&stop);
        // Detached: it ends with the process if stdin never delivers a line.
        std::thread::spawn(move || {
            let mut buf = String::new();
            if std::io::stdin()
                .lock()
                .read_line(&mut buf)
                .is_ok_and(|n| n > 0)
            {
                stop.store(true, Ordering::Relaxed);
            }
        });
    }

    let start = Instant::now();
    let mut next = Duration::from_secs(1);
    let mut failure = None;
    while !stop.load(Ordering::Relaxed) && a.duration.is_none_or(|d| start.elapsed() < d) {
        let serviced = recording.service();
        for (_, event) in &serviced.events {
            println!("engine: {event:?}");
        }
        for e in &serviced.errors {
            println!("engine error: {e}");
        }
        if let Some(why) = recording.failure() {
            failure = Some(why);
            break;
        }
        if start.elapsed() >= next {
            next += Duration::from_secs(1);
            println!("{}", line(start.elapsed(), &recording.progress()));
        }
        let wait = serviced.wait.unwrap_or(Duration::from_millis(50));
        std::thread::sleep(wait.min(Duration::from_millis(50)));
    }
    let elapsed = start.elapsed();
    println!("stopping...");
    let report = recording.finish();
    let p = &report.progress;
    println!("--- {:.1} s recorded", elapsed.as_secs_f64());
    println!(
        "video: {} frames exported, {} encoded, {} dropped at export, {} at the encoder",
        p.frames_exported, p.frames_encoded, p.export_drops, p.encoder_drops
    );
    for s in &p.sources {
        println!(
            "{}: {} frames, {} resyncs, {} errors",
            s.name, s.frames, s.resyncs, s.errors
        );
    }
    println!(
        "output: {} packets muxed, {} muxer drops, backlog peak {}, {} lost at finish; audio drops {} (queue) {} (encoder); late ticks render {} audio {}",
        p.packets_muxed(),
        p.mux_drops,
        p.backlog_peak,
        report.finish_lost,
        p.audio_queue_drops,
        p.audio_packet_drops,
        p.late_render_ticks,
        p.late_audio_ticks
    );
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("file: {} ({size} bytes)", out.display());
    for e in &report.shutdown_errors {
        println!("shutdown error: {e}");
    }
    let failure = failure.or(report.failure);
    match failure {
        Some(why) => Err(why),
        None if p.packets_muxed() == 0 => Err("no packet reached the file".into()),
        None if !report.shutdown_errors.is_empty() => Err("the engine did not stop cleanly".into()),
        None => Ok(()),
    }
}
