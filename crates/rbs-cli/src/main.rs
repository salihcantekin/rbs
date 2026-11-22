//! `rbs-cli`: the headless RBS command line (ADR-0040, spec `docs/specs/recording.md`).
//!
//! ```text
//! rbs-cli record C:\Videos\rec.mkv --seconds 60
//! RBS_STREAM_KEY=... rbs-cli stream rtmp://a.rtmp.youtube.com/live2
//! rbs-cli devices
//! rbs-cli token create "Stream Deck"
//! rbs-cli remote fingerprint
//! ```
//!
//! `record` captures the main display with the first camera in the bottom-right corner,
//! the default microphone and the system audio, and writes H.264 and AAC to a Matroska
//! (`.mkv`) or fragmented MP4 (`.mp4`) file until the time is up, Enter is pressed,
//! or Ctrl+C asks the recording to finish.
//! `stream` sends the same program live: H.264 and AAC packets over RTMP, or MPEG-TS
//! over SRT, with the stream key from an environment variable.

mod app;
mod args;
mod control;
mod diag_log;
mod interrupt;
mod obs_import;
mod permissions;
mod remote;
mod scene;
mod token;
mod vcam;

use std::fmt;
use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rbs_codec::VideoEncoderFactory;
use rbs_core::capability::Requirement;
use rbs_core::registry::{AdapterFactory, AdapterId, Registry};
use rbs_engine::Engine;
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_graph::description::NodeId;
use rbs_graph::runtime::{FaultCause, RuntimeEvent};
use rbs_record::{AudioSettings, Progress, RecordAdapters, RecordError, RecordSettings, Recording};
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};

use crate::args::{Command, RecordArgs, StreamTarget};

/// `rbs-cli project`: writes the editor project next to the manifest and names it;
/// separate files missing from the folder are listed (ADR-0089 decision 10).
fn project(p: &args::ProjectArgs) -> Result<(), String> {
    let manifest =
        std::path::absolute(&p.manifest).map_err(|e| format!("{}: {e}", p.manifest.display()))?;
    let (Some(folder), Some(name)) = (
        manifest.parent(),
        manifest.file_name().and_then(|n| n.to_str()),
    ) else {
        return Err(format!("{} is not a manifest file", p.manifest.display()));
    };
    let done = rbs_project::export(folder, name, p.format, p.strict).map_err(|e| e.to_string())?;
    rbs_crash::outln!("wrote {}", done.path.display());
    for file in &done.missing {
        rbs_crash::outln!("left out (not in the folder): {file}");
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    let _log = match args::take_log_level(&mut raw).and_then(|l| diag_log::start(l.as_deref())) {
        Ok(run) => run,
        Err(e) => {
            rbs_crash::errln!("{e}\n\n{}", args::USAGE);
            return ExitCode::from(2);
        }
    };
    match args::parse(raw.iter().map(String::as_str)) {
        Ok(Command::Help) => {
            rbs_crash::outln!("{}", args::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Devices) => devices(),
        Ok(Command::Token(t)) => {
            let config = rbs_remote_settings::config_dir();
            // A running RBS serves its own config directory: with another one chosen
            // here, only the file is changed.
            let ipc = std::env::var_os("RBS_CONFIG_DIR")
                .is_none()
                .then(|| rbs_api_ipc::Endpoint::for_current_user().ok())
                .flatten();
            let (mut out, mut notes) = (std::io::stdout(), std::io::stderr());
            match token::run(&config, ipc.as_ref(), &t, &mut out, &mut notes) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    rbs_crash::errln!("token: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::App(c)) => {
            let what = match c {
                args::AppCommand::Stream(args::StreamControl::Start) => "app stream start",
                args::AppCommand::Stream(args::StreamControl::Stop) => "app stream stop",
                args::AppCommand::Stream(args::StreamControl::Status { .. }) => "app stream status",
                args::AppCommand::Stream(args::StreamControl::Destinations { .. }) => {
                    "app stream destinations"
                }
                args::AppCommand::Stream(args::StreamControl::Destination { .. }) => {
                    "app stream destination"
                }
            };
            let result = rbs_api_ipc::Endpoint::for_current_user()
                .map_err(|e| format!("the local control endpoint is not known ({e})"))
                .and_then(|ipc| app::run(&ipc, &c, &mut std::io::stdout()));
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    rbs_crash::errln!("{what}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Vcam(c)) => {
            let factory = virtual_camera();
            match vcam::run(factory.as_deref(), c, &mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    rbs_crash::errln!("vcam: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Project(p)) => match project(&p) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                rbs_crash::errln!("project: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(Command::ObsImport(c)) => {
            let config = rbs_remote_settings::config_dir();
            match obs_import::run(&c, &config, &mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    rbs_crash::errln!("obs-import: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::RemoteFingerprint) => {
            match remote::fingerprint(&rbs_remote_settings::config_dir(), &mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    rbs_crash::errln!("remote fingerprint: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Record(a)) => match record(&a) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let what = if a.stream.is_some() {
                    "stream"
                } else {
                    "record"
                };
                rbs_crash::errln!("{what} failed: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            rbs_crash::errln!("{e}\n\n{}", args::USAGE);
            ExitCode::from(2)
        }
    }
}

fn probed() -> RecordAdapters {
    let mut registered = rbs_platform::adapters(&rbs_platform::Options::default());
    for e in registered.errors {
        rbs_crash::errln!("warning: {e}");
    }
    // Listing devices probes every adapter, including every video encoder.
    registered.adapters.probe();
    registered.adapters
}

fn list_registry<F: ?Sized + AdapterFactory>(title: &str, registry: &Registry<F>) {
    rbs_crash::outln!("{title}:");
    let ids: Vec<&AdapterId> = registry.ids().collect();
    if ids.is_empty() {
        rbs_crash::outln!("  (none on this platform yet)");
    }
    for id in ids {
        // An adapter that probes to nothing (no driver, no device) cannot be used.
        let state = if registry.capabilities(id).is_some_and(|c| !c.is_empty()) {
            "available"
        } else {
            "unavailable"
        };
        rbs_crash::outln!("  {id} ({state})");
    }
}

fn devices() -> ExitCode {
    let adapters = probed();
    rbs_crash::outln!("sources:");
    for t in adapters.sources.installed() {
        rbs_crash::outln!("  {t}");
        let targets = adapters.sources.targets(&t).unwrap_or_default();
        for (i, target) in targets.iter().enumerate() {
            rbs_crash::outln!(
                "    {i}: {} [{}]{}",
                target.name,
                target.adapter,
                if target.is_default { " (default)" } else { "" }
            );
        }
        for picker in adapters.sources.pickers(&t).unwrap_or_default() {
            rbs_crash::outln!("    chosen in the system dialog [{}]", picker.adapter());
        }
    }
    list_registry("video encoders", &adapters.video_encoders);
    list_registry("audio encoders", &adapters.audio_encoders);
    list_registry("muxers", &adapters.muxers);
    list_registry("transports", &adapters.transports);
    ExitCode::SUCCESS
}

/// Probes the adapters a recording uses, once each.
///
/// Sources, muxers and transports (`probe_io`) and audio encoders are always probed.
/// With no `encoder`, every video encoder is probed once. With `encoder`, the id is
/// resolved from the registered ids (that does not probe), only that factory is
/// installed, and that registry is probed once. The other video encoders are not
/// probed. A named encoder that is not usable fails rather than falling back.
fn probe_for_record(
    adapters: &mut RecordAdapters,
    video_factories: &[Arc<dyn VideoEncoderFactory>],
    encoder: Option<&str>,
) -> Result<(), String> {
    let named = match encoder {
        Some(name) => {
            let id = resolve(&adapters.video_encoders, name)?;
            let factory = video_factories
                .iter()
                .find(|factory| factory.id() == id)
                .cloned()
                .ok_or_else(|| format!("encoder {id} is not available on this system"))?;
            let mut only = Registry::new();
            only.register(factory).map_err(|e| e.to_string())?;
            adapters.video_encoders = only;
            Some(id)
        }
        None => None,
    };
    adapters.probe_io();
    adapters.audio_encoders.probe_all();
    adapters.video_encoders.probe_all();
    if let Some(id) = named {
        // Only the named encoder: if it cannot open, the recording fails rather than
        // quietly using another one.
        let available = adapters
            .video_encoders
            .select(&Requirement::new())
            .candidates
            .iter()
            .any(|factory| factory.id() == id);
        if !available {
            return Err(format!("encoder {id} is not available on this system"));
        }
    }
    Ok(())
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
        "{:>4}s  video {} exported, {} encoded ({} dropped, {} skipped)",
        elapsed.as_secs(),
        p.frames_exported,
        p.frames_encoded,
        p.export_drops + p.encoder_drops,
        p.encoder_skipped,
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

/// A `record` or `stream` failure. A studio start keeps [`RecordError`] until it is
/// printed; every other failure is already a sentence.
enum RecordCommandError {
    Record(RecordError),
    Other(String),
}

impl fmt::Display for RecordCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<String> for RecordCommandError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for RecordCommandError {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}

/// A new master clock and the platform's adapters with the input monitor of pointer
/// filters on it; the engine runs on the same clock, so pointer and frame times share
/// one time line (ADR-0165).
fn clock_and_adapters() -> (
    Arc<dyn rbs_core::time::Clock>,
    rbs_platform::PlatformAdapters,
) {
    let clock: Arc<dyn rbs_core::time::Clock> = Arc::new(rbs_core::time::MonotonicClock::new());
    let options = rbs_platform::Options::default().with_master_clock(Arc::clone(&clock));
    (clock, rbs_platform::adapters(&options))
}

fn record(a: &RecordArgs) -> Result<(), RecordCommandError> {
    let (clock, mut registered) = clock_and_adapters();
    for e in registered.errors {
        rbs_crash::errln!("warning: {e}");
    }
    probe_for_record(
        &mut registered.adapters,
        &registered.video_encoders,
        a.encoder.as_deref(),
    )?;
    let mut adapters = registered.adapters;
    if adapters.video_encoders.ids().next().is_none() {
        return Err(format!(
            "recording is not available on {} yet (no video encoder adapters are wired)",
            std::env::consts::OS
        )
        .into());
    }
    if !a.no_audio && adapters.audio_encoders.ids().next().is_none() {
        return Err(format!(
            "no audio encoder is wired on {} yet; pass --no-audio to record the picture only",
            std::env::consts::OS
        )
        .into());
    }

    // The stream key is checked before any device or permission prompt is touched.
    let secret = match &a.stream {
        Some(target) => stream_secret(target)?,
        None => None,
    };
    let permissions = permissions::Permissions::new(&mut adapters.sources);
    let built = scene::build(&adapters.sources, a, &|t, id| permissions.check(t, id))?;
    for s in &built.skipped {
        rbs_crash::outln!("left out: {s}");
    }
    if a.duration.is_none() && !std::io::stdin().is_terminal() {
        return Err("without a terminal nothing can press Enter: pass --seconds".into());
    }
    let mut settings = match &a.stream {
        Some(target) => stream_settings(built.canvas, target, secret),
        None => {
            let out =
                std::path::absolute(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
            if out.exists() && !a.overwrite {
                return Err(
                    format!("{} exists; pass --overwrite to replace it", out.display()).into(),
                );
            }
            RecordSettings::new(
                built.canvas,
                rbs_transport_file::file_url(&out),
                a.container,
            )
        }
    };
    // The file written (`record`), to remove after a failed start and report its size.
    let file = match &a.stream {
        Some(_) => None,
        None => Some(std::path::absolute(&a.out).map_err(|e| e.to_string())?),
    };
    let existed = file.as_ref().is_some_and(|f| f.exists());
    if let Some(video) = &mut settings.video {
        video.rate_control = a.rate_control;
        video.keyframe_interval = a.keyframe_interval;
        video.max_b_frames = a.b_frames;
        video.profile = a.profile.clone();
        video.preset = a.preset;
    }
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
            rbs_crash::outln!("{name} sync offset: {ms} ms");
        }
    }

    rbs_crash::outln!("video: {}", args::describe_video(a));

    // `RBS_GPU_POWER=low` prefers the integrated GPU of a hybrid machine: a diagnostic
    // for measuring the weak hardware tier on a desktop (performance audit, R-03).
    let low_power = std::env::var_os("RBS_GPU_POWER").is_some_and(|v| v == "low");
    let request = GpuRequest {
        power: if low_power {
            rbs_gpu::PowerPreference::LowPower
        } else {
            rbs_gpu::PowerPreference::HighPerformance
        },
        ..GpuRequest::default()
    };
    let gpu = Arc::new(GpuContext::new(&request).map_err(|e| format!("no usable GPU: {e}"))?);
    rbs_crash::outln!("gpu: {}", gpu.adapter_name());
    let mut engine = Engine::with_clock(clock, rbs_engine::EngineConfig::default());
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut recording = match Recording::start(engine, gpu, &built.collection, &adapters, &settings)
    {
        Ok(r) => r,
        Err(e) => {
            // A failed start may have created the file; leave no empty file behind.
            if let Some(out) = file.as_ref().filter(|f| !existed && f.exists()) {
                let _ = std::fs::remove_file(out);
            }
            return Err(RecordCommandError::Record(e));
        }
    };
    let started = recording.started().clone();
    rbs_crash::outln!(
        "video encoder: {} | audio encoder: {} | muxer: {} | transport: {}",
        started
            .video_encoder
            .as_ref()
            .map_or("none", |c| c.name.as_str()),
        started
            .audio_encoder
            .as_ref()
            .map_or("none", |c| c.name.as_str()),
        started.muxer.as_ref().map_or("none", |c| c.name.as_str()),
        started.transport.name
    );
    for s in &started.sources {
        match &s.failed {
            None => rbs_crash::outln!("source {}: {} [{}]", s.name, s.target, s.adapter),
            Some(why) => rbs_crash::outln!("source {}: did not start: {why}", s.name),
        }
    }

    // Kept for the whole run: the session's adapter lives as long as the camera sends.
    let camera = match a.vcam {
        Some(f) => Some(start_camera(&mut recording, f)?),
        None => None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    // The URL never holds the stream key, so it can be printed.
    let destination = match (&file, &a.stream) {
        (Some(out), _) => out.display().to_string(),
        (None, Some(target)) => target.url.clone(),
        (None, None) => String::new(),
    };
    let verb = if a.stream.is_some() {
        "streaming"
    } else {
        "recording"
    };
    match a.duration {
        Some(d) => rbs_crash::outln!(
            "{verb} {} s to {destination} (press Enter or Ctrl+C to stop early)",
            d.as_secs(),
        ),
        None => rbs_crash::outln!("{verb} to {destination}; press Enter or Ctrl+C to stop"),
    }
    interrupt::install();
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
    let mut link = Link::default();
    while !stop.load(Ordering::Relaxed)
        && !interrupt::stop_asked()
        && a.duration.is_none_or(|d| start.elapsed() < d)
    {
        let serviced = recording.service();
        for (_, event) in &serviced.events {
            if let Some(text) = describe_event(event, &recording.started().sources) {
                rbs_crash::outln!("{text}");
            }
        }
        for e in &serviced.errors {
            rbs_crash::outln!("engine error: {e}");
        }
        if let Some(why) = recording.failure() {
            failure = Some(why);
            break;
        }
        let progress = recording.progress();
        if let Some(target) = &a.stream {
            for change in link.update(&progress) {
                rbs_crash::outln!("{}", change.describe(&target.url));
            }
        }
        if start.elapsed() >= next {
            next += Duration::from_secs(1);
            let mut text = line(start.elapsed(), &progress);
            if a.stream.is_some() {
                text.push_str(if progress.connected {
                    " | live"
                } else {
                    " | not connected"
                });
            }
            if let Some(state) = recording.virtual_camera() {
                text.push_str(&format!(" | camera {}", camera_line(&state)));
            }
            rbs_crash::outln!("{text}");
        }
        let wait = serviced.wait.unwrap_or(Duration::from_millis(50));
        std::thread::sleep(wait.min(Duration::from_millis(50)));
    }
    let elapsed = start.elapsed();
    rbs_crash::outln!("stopping...");
    if failure.is_none() {
        // Ends the video now and lets the frames still read back reach the encoder
        // (ADR-0045); the recording gives up on them after `END_TAIL_LIMIT` itself.
        recording.end();
        let ending = Instant::now();
        while !recording.tail_done() && ending.elapsed() < Duration::from_secs(1) {
            recording.service();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let camera_end = recording.virtual_camera();
    let report = recording.finish();
    let p = &report.progress;
    rbs_crash::outln!(
        "--- {:.1} s {}",
        elapsed.as_secs_f64(),
        if a.stream.is_some() {
            "streamed"
        } else {
            "recorded"
        }
    );
    rbs_crash::outln!(
        "video: {} frames exported, {} encoded, {} dropped at export, {} skipped before the encoder, {} dropped at the encoder",
        p.frames_exported,
        p.frames_encoded,
        p.export_drops,
        p.encoder_skipped,
        p.encoder_drops
    );
    for s in &p.sources {
        rbs_crash::outln!(
            "{}: {} frames, {} resyncs, {} errors",
            s.name,
            s.frames,
            s.resyncs,
            s.errors
        );
    }
    rbs_crash::outln!(
        "output: {} packets muxed, {} muxer drops, backlog peak {}, {} lost at finish, {} lost in the video encoder; audio drops {} (queue) {} (encoder); late ticks render {} audio {}",
        p.packets_muxed(),
        p.mux_drops,
        p.backlog_peak,
        report.finish_lost,
        report.flush_lost,
        p.audio_queue_drops,
        p.audio_packet_drops,
        p.late_render_ticks,
        p.late_audio_ticks
    );
    rbs_crash::outln!(
        "audio tail: {} encoder flushes, {} lost at the end; {} packets dropped before the first video packet",
        p.audio_flushes,
        p.audio_tail_lost,
        p.start_drops
    );
    if a.stream.is_some() {
        rbs_crash::outln!(
            "stream: connected {} times, {} reconnect attempts, {} items dropped by the transport",
            p.connects,
            p.reconnect_attempts,
            p.transport_drops
        );
    }
    if let (Some(_), Some(state)) = (&camera, &camera_end) {
        rbs_crash::outln!("virtual camera: {}", camera_line(state));
        if let Some(why) = &state.failure {
            rbs_crash::outln!("virtual camera failed: {why}");
        }
    }
    if let Some(out) = &file {
        let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
        rbs_crash::outln!("file: {} ({size} bytes)", out.display());
    }
    for e in &report.shutdown_errors {
        rbs_crash::outln!("shutdown error: {e}");
    }
    let failure = failure.or(report.failure);
    let url = a.stream.as_ref().map_or("", |t| t.url.as_str());
    match failure {
        // The stream failed before it ever reached the server.
        Some(why) if a.stream.is_some() && p.connects == 0 => {
            Err(format!("never connected to {url}: {why}").into())
        }
        Some(why) => Err(why.into()),
        None if a.stream.is_some() && p.connects == 0 => {
            Err(format!("never connected to {url}").into())
        }
        None if p.packets_muxed() == 0 => Err(format!(
            "no packet reached the {}",
            if a.stream.is_some() { "stream" } else { "file" }
        )
        .into()),
        None if !report.shutdown_errors.is_empty() => Err("the engine did not stop cleanly".into()),
        None => Ok(()),
    }
}

/// Sets the camera's format and starts sending the program to it (`--vcam`). Returns
/// the factory, which must outlive the camera's session.
/// The platform's virtual camera adapter, if it has one (ADR-0066: Windows only). Its
/// symbolic link is kept in the RBS settings folder, in the file the app uses too.
fn virtual_camera() -> Option<Arc<dyn rbs_vcam::VirtualCameraFactory>> {
    rbs_platform::virtual_camera(&rbs_remote_settings::config_dir())
        .ok()
        .map(|camera| camera.factory)
}

fn start_camera(
    recording: &mut Recording,
    f: args::VcamFormat,
) -> Result<Arc<dyn rbs_vcam::VirtualCameraFactory>, String> {
    let factory =
        virtual_camera().ok_or("--vcam: the virtual camera is not available on this platform")?;
    let format = vcam::camera_format(f)?;
    if !factory
        .set_format(&format)
        .map_err(|e| format!("--vcam: {e}"))?
    {
        rbs_crash::outln!(
            "virtual camera: an app is using the camera; it shows the still picture until it opens the camera again"
        );
    }
    recording
        .start_virtual_camera(
            factory.as_ref(),
            rbs_record::VirtualCameraSettings::new(f.size.width, f.size.height, format.rate),
        )
        .map_err(|e| format!("--vcam: {e}"))?;
    rbs_crash::outln!(
        "virtual camera: sending {}x{} at {} fps",
        f.size.width,
        f.size.height,
        f.fps
    );
    Ok(factory)
}

/// The camera's counters in one line.
fn camera_line(state: &rbs_record::VirtualCameraState) -> String {
    let s = &state.stats;
    let n = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    let delivered = state.session.as_ref().map_or_else(String::new, |c| {
        let c = c.snapshot();
        format!(
            "{} delivered, {} with no app reading, {} waiting for a format change; ",
            c.sent(),
            c.consumer_absent(),
            c.format_pending()
        )
    });
    format!(
        "{delivered}{} handed over, {} skipped (no app reading), {} skipped (app behind), {} skipped (readback busy), {} lost, {} refused, {} failed",
        n(&s.sent),
        n(&s.no_reader),
        n(&s.consumer_busy),
        n(&s.busy),
        n(&s.lost),
        n(&s.refused),
        n(&s.failed)
    )
}

/// A node failure or restart the engine reported, in words; `None` for what is not
/// worth a line (a failure record dropped with its node). Sources go by name; other
/// nodes (encoders, the output) by their part, since the recording's own failure
/// report names the cause when it ends the recording.
fn describe_event(event: &RuntimeEvent, sources: &[rbs_record::SourceState]) -> Option<String> {
    let name = |node: NodeId| {
        sources.iter().find(|s| s.node == node).map_or_else(
            || format!("an engine node ({node})"),
            |s| format!("source {}", s.name),
        )
    };
    match event {
        RuntimeEvent::Failed {
            node,
            attempt,
            cause,
            retry_at,
        } => {
            let cause = match cause {
                FaultCause::Join(e) => e.to_string(),
                FaultCause::Bypassed(h) => h.last_error.unwrap_or("stopped").to_owned(),
                _ => "failed".to_owned(),
            };
            let next = if retry_at.is_some() {
                "restarting it shortly"
            } else {
                "not restarted"
            };
            let times = if *attempt > 1 {
                format!(" ({attempt} times in a row)")
            } else {
                String::new()
            };
            Some(format!("{} failed{times}: {cause}; {next}", name(*node)))
        }
        RuntimeEvent::Restarted { node, .. } => Some(format!("{} restarted", name(*node))),
        RuntimeEvent::Recovered { node } => Some(format!("{} runs normally again", name(*node))),
        _ => None,
    }
}

/// Reconnect policy of `stream`: up to 10 tries, 1 s doubling to 30 s apart (about
/// three minutes before a server that never answers fails the stream).
const STREAM_RECONNECT: ReconnectPolicy = ReconnectPolicy::Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(30),
    attempts: Some(10),
};

/// The stream key (RTMP, required) or stream id (SRT, optional) from the environment
/// variable the target names, trimmed.
fn stream_secret(target: &StreamTarget) -> Result<Option<Secret>, String> {
    let key = std::env::var(&target.key_env)
        .ok()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty());
    match (&key, target.input) {
        (None, TransportInput::Packets) => Err(format!(
            "set the environment variable {} to the stream key (an RTMP server needs one)",
            target.key_env
        )),
        (None, _) => {
            rbs_crash::outln!("no stream id ({} is not set)", target.key_env);
            Ok(None)
        }
        (Some(_), _) => {
            rbs_crash::outln!("stream key: from {}", target.key_env);
            Ok(key.map(Secret::new))
        }
    }
}

/// The output settings of `stream`: the URL, its transport input, the secret, reconnect
/// and the SRT latency.
fn stream_settings(
    canvas: rbs_scene::model::CanvasId,
    target: &StreamTarget,
    secret: Option<Secret>,
) -> RecordSettings {
    let mut settings = match target.input {
        TransportInput::Container(container) => {
            RecordSettings::new(canvas, target.url.clone(), container)
        }
        _ => RecordSettings::packets(canvas, target.url.clone()),
    };
    settings.secret = secret;
    if target.reconnect {
        settings.reconnect = STREAM_RECONNECT;
    }
    if let Some(ms) = target.srt_latency_ms {
        settings.transport_options = vec![("latency_ms".to_owned(), ms.to_string())];
    }
    settings
}

/// A change of a stream's connection, printed when it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkChange {
    /// Connected, for the `n`th time.
    Connected(u64),
    /// Connection attempts after a failure: total so far.
    Reconnecting(u64),
}

impl LinkChange {
    fn describe(self, url: &str) -> String {
        match self {
            LinkChange::Connected(1) => format!("connected to {url}"),
            LinkChange::Connected(n) => format!("connected to {url} again ({n} times)"),
            LinkChange::Reconnecting(n) => format!("not connected, trying again (attempt {n})"),
        }
    }
}

/// The connection counters seen last, to print each change once.
#[derive(Debug, Default)]
struct Link {
    connects: u64,
    attempts: u64,
}

impl Link {
    fn update(&mut self, p: &Progress) -> Vec<LinkChange> {
        let mut changes = Vec::new();
        if p.reconnect_attempts > self.attempts {
            self.attempts = p.reconnect_attempts;
            changes.push(LinkChange::Reconnecting(p.reconnect_attempts));
        }
        if p.connects > self.connects {
            self.connects = p.connects;
            changes.push(LinkChange::Connected(p.connects));
        }
        changes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn node_id() -> NodeId {
        use rbs_graph::description::GraphDescription;
        use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
        GraphDescription::new()
            .add_node(Arc::new(NodeDescriptor::new(
                NodeTypeId::new_static("test.node"),
                NodeKind::Source,
                Vec::new(),
                vec![Pin::named("video", LinkKind::Video, true)],
            )))
            .unwrap()
    }

    #[test]
    fn engine_events_are_described_in_words() {
        use rbs_graph::runtime::{JoinError, NodeError, NodeHealth};
        let node = node_id();
        let failed = RuntimeEvent::Failed {
            node,
            attempt: 1,
            cause: FaultCause::Join(JoinError::Prepare(NodeError::Permanent(
                "transport disconnected",
            ))),
            retry_at: None,
        };
        let text = describe_event(&failed, &[]).unwrap();
        assert_eq!(
            text,
            format!(
                "an engine node ({node}) failed: prepare failed: permanent failure: transport disconnected; not restarted"
            )
        );
        assert!(!text.contains('{'), "no debug output: {text}");
        let mut health = NodeHealth::default();
        health.last_error = Some("device lost");
        let bypassed = RuntimeEvent::Failed {
            node,
            attempt: 3,
            cause: FaultCause::Bypassed(health),
            retry_at: Some(rbs_core::time::Timestamp::from_nanos(1)),
        };
        assert_eq!(
            describe_event(&bypassed, &[]).unwrap(),
            format!(
                "an engine node ({node}) failed (3 times in a row): device lost; restarting it shortly"
            )
        );
        assert_eq!(describe_event(&RuntimeEvent::Cleared { node }, &[]), None);
    }

    #[test]
    fn a_busy_canvas_stays_a_record_error_until_it_is_printed() {
        let id = rbs_scene::model::CanvasId::from_raw(7);
        let error = RecordCommandError::Record(RecordError::CanvasBusy(id));
        assert!(matches!(
            error,
            RecordCommandError::Record(RecordError::CanvasBusy(got)) if got == id
        ));
        assert_eq!(error.to_string(), RecordError::CanvasBusy(id).to_string());
    }

    #[test]
    fn connection_changes_are_printed_once_each() {
        let mut link = Link::default();
        let mut p = Progress::default();
        assert!(link.update(&p).is_empty());
        p.connects = 1;
        assert_eq!(link.update(&p), [LinkChange::Connected(1)]);
        assert!(link.update(&p).is_empty(), "nothing new");
        p.reconnect_attempts = 2;
        assert_eq!(link.update(&p), [LinkChange::Reconnecting(2)]);
        p.connects = 2;
        assert_eq!(link.update(&p), [LinkChange::Connected(2)]);
        assert_eq!(
            LinkChange::Connected(1).describe("rtmp://h/app"),
            "connected to rtmp://h/app"
        );
        assert_eq!(
            LinkChange::Reconnecting(3).describe("x"),
            "not connected, trying again (attempt 3)"
        );
    }

    /// Counts `probe` calls. Creating the factory does not probe, and `create` is never
    /// called: the test does not open a device.
    struct CountingEncoder {
        id: &'static str,
        probes: Arc<AtomicUsize>,
        fail: bool,
    }

    impl AdapterFactory for CountingEncoder {
        fn id(&self) -> AdapterId {
            AdapterId::new(self.id)
        }

        fn display_name(&self) -> &str {
            self.id
        }

        fn probe(
            &self,
        ) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(rbs_core::registry::ProbeError::new("missing"))
            } else {
                Ok(rbs_core::capability::CapabilitySet::new())
            }
        }
    }

    impl VideoEncoderFactory for CountingEncoder {
        fn create(
            &self,
            _: &rbs_codec::VideoEncoderConfig,
        ) -> Result<Box<dyn rbs_codec::VideoEncoder>, rbs_codec::CreateError> {
            Err(rbs_codec::CreateError::Unsupported {
                what: "test".into(),
            })
        }
    }

    impl rbs_codec::AudioEncoderFactory for CountingEncoder {
        fn create(
            &self,
            _: &rbs_codec::AudioEncoderConfig,
        ) -> Result<Box<dyn rbs_codec::AudioEncoder>, rbs_codec::CreateError> {
            Err(rbs_codec::CreateError::Unsupported {
                what: "test".into(),
            })
        }
    }

    fn counting(id: &'static str, fail: bool) -> (Arc<CountingEncoder>, Arc<AtomicUsize>) {
        let probes = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(CountingEncoder {
            id,
            probes: Arc::clone(&probes),
            fail,
        });
        (factory, probes)
    }

    fn register_video(
        adapters: &mut RecordAdapters,
        factory: Arc<CountingEncoder>,
    ) -> Arc<dyn VideoEncoderFactory> {
        let factory: Arc<dyn VideoEncoderFactory> = factory;
        adapters
            .video_encoders
            .register(Arc::clone(&factory))
            .unwrap();
        factory
    }

    #[test]
    fn named_encoder_is_probed_once_and_the_others_are_not() {
        let (alpha, alpha_probes) = counting("org.rbs.test.alpha", false);
        let (beta, beta_probes) = counting("org.rbs.test.beta", false);
        let (audio, audio_probes) = counting("org.rbs.test.audio", false);
        let mut adapters = RecordAdapters::new();
        let factories = vec![
            register_video(&mut adapters, alpha),
            register_video(&mut adapters, beta),
        ];
        adapters
            .audio_encoders
            .register(audio as Arc<dyn rbs_codec::AudioEncoderFactory>)
            .unwrap();

        probe_for_record(&mut adapters, &factories, Some("beta")).unwrap();

        assert_eq!(alpha_probes.load(Ordering::SeqCst), 0);
        assert_eq!(beta_probes.load(Ordering::SeqCst), 1);
        assert_eq!(audio_probes.load(Ordering::SeqCst), 1);
        let ids: Vec<_> = adapters
            .video_encoders
            .ids()
            .map(|id| id.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["org.rbs.test.beta"]);
    }

    #[test]
    fn every_video_encoder_is_probed_once_when_none_is_named() {
        let (alpha, alpha_probes) = counting("org.rbs.test.alpha", false);
        let (beta, beta_probes) = counting("org.rbs.test.beta", false);
        let mut adapters = RecordAdapters::new();
        let factories = vec![
            register_video(&mut adapters, alpha),
            register_video(&mut adapters, beta),
        ];

        probe_for_record(&mut adapters, &factories, None).unwrap();

        assert_eq!(alpha_probes.load(Ordering::SeqCst), 1);
        assert_eq!(beta_probes.load(Ordering::SeqCst), 1);
        assert_eq!(adapters.video_encoders.ids().count(), 2);
    }

    #[test]
    fn an_unknown_encoder_fails_before_any_probe() {
        let (alpha, alpha_probes) = counting("org.rbs.test.alpha", false);
        let mut adapters = RecordAdapters::new();
        let factories = vec![register_video(&mut adapters, alpha)];

        let err = probe_for_record(&mut adapters, &factories, Some("missing")).unwrap_err();

        assert!(err.contains("no encoder"), "{err}");
        assert_eq!(alpha_probes.load(Ordering::SeqCst), 0);
        assert_eq!(adapters.video_encoders.ids().count(), 1);
    }

    #[test]
    fn a_named_encoder_that_cannot_probe_does_not_fall_back() {
        let (alpha, alpha_probes) = counting("org.rbs.test.alpha", false);
        let (beta, beta_probes) = counting("org.rbs.test.beta", true);
        let mut adapters = RecordAdapters::new();
        let factories = vec![
            register_video(&mut adapters, alpha),
            register_video(&mut adapters, beta),
        ];

        let err = probe_for_record(&mut adapters, &factories, Some("beta")).unwrap_err();

        assert!(err.contains("not available on this system"), "{err}");
        assert_eq!(alpha_probes.load(Ordering::SeqCst), 0);
        assert_eq!(beta_probes.load(Ordering::SeqCst), 1);
        let ids: Vec<_> = adapters
            .video_encoders
            .ids()
            .map(|id| id.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["org.rbs.test.beta"]);
    }
}
