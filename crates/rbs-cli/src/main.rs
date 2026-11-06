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
//! (`.mkv`) or fragmented MP4 (`.mp4`) file until the time is up or Enter is pressed.
//! `stream` sends the same program live: H.264 and AAC packets over RTMP, or MPEG-TS
//! over SRT, with the stream key from an environment variable.

#![allow(clippy::print_stdout, clippy::print_stderr)] // A command-line program prints.

mod app;
mod args;
mod control;
mod permissions;
mod platform;
mod remote;
mod scene;
mod token;

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rbs_core::capability::Requirement;
use rbs_core::registry::{AdapterFactory, AdapterId, Registry};
use rbs_engine::Engine;
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_graph::description::NodeId;
use rbs_graph::runtime::{FaultCause, RuntimeEvent};
use rbs_record::{AudioSettings, Progress, RecordAdapters, RecordSettings, Recording};
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};

use crate::args::{Command, RecordArgs, StreamTarget};

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(raw.iter().map(String::as_str)) {
        Ok(Command::Help) => {
            println!("{}", args::USAGE);
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
                    eprintln!("token: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::App(c)) => {
            let what = match c {
                args::AppCommand::Stream(args::StreamControl::Start) => "app stream start",
                args::AppCommand::Stream(args::StreamControl::Stop) => "app stream stop",
                args::AppCommand::Stream(args::StreamControl::Status { .. }) => "app stream status",
            };
            let result = rbs_api_ipc::Endpoint::for_current_user()
                .map_err(|e| format!("the local control endpoint is not known ({e})"))
                .and_then(|ipc| app::run(&ipc, &c, &mut std::io::stdout()));
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{what}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::RemoteFingerprint) => {
            match remote::fingerprint(&rbs_remote_settings::config_dir(), &mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("remote fingerprint: {e}");
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
                eprintln!("{what} failed: {e}");
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
        // An adapter that probes to nothing (no driver, no device) cannot be used.
        let state = if registry.capabilities(id).is_some_and(|c| !c.is_empty()) {
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
        for picker in adapters.sources.pickers(&t).unwrap_or_default() {
            println!("    chosen in the system dialog [{}]", picker.adapter());
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
    if adapters.video_encoders.ids().next().is_none() {
        return Err(format!(
            "recording is not available on {} yet (no video encoder adapters are wired)",
            std::env::consts::OS
        ));
    }
    if !a.no_audio && adapters.audio_encoders.ids().next().is_none() {
        return Err(format!(
            "no audio encoder is wired on {} yet; pass --no-audio to record the picture only",
            std::env::consts::OS
        ));
    }

    // The stream key is checked before any device or permission prompt is touched.
    let secret = match &a.stream {
        Some(target) => stream_secret(target)?,
        None => None,
    };
    let permissions = permissions::Permissions::new(&mut adapters.sources);
    let built = scene::build(&adapters.sources, a, &|t, id| permissions.check(t, id))?;
    for s in &built.skipped {
        println!("left out: {s}");
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
                return Err(format!(
                    "{} exists; pass --overwrite to replace it",
                    out.display()
                ));
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
    settings.video.rate_control = a.rate_control;
    settings.video.keyframe_interval = a.keyframe_interval;
    settings.video.max_b_frames = a.b_frames;
    settings.video.profile = a.profile.clone();
    settings.video.preset = a.preset;
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

    println!("video: {}", args::describe_video(a));

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
            if let Some(out) = file.as_ref().filter(|f| !existed && f.exists()) {
                let _ = std::fs::remove_file(out);
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
        started.muxer.as_ref().map_or("none", |c| c.name.as_str()),
        started.transport.name
    );
    for s in &started.sources {
        match &s.failed {
            None => println!("source {}: {} [{}]", s.name, s.target, s.adapter),
            Some(why) => println!("source {}: did not start: {why}", s.name),
        }
    }

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
        Some(d) => println!(
            "{verb} {} s to {destination} (press Enter to stop early)",
            d.as_secs(),
        ),
        None => println!("{verb} to {destination}; press Enter to stop"),
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
    let mut link = Link::default();
    while !stop.load(Ordering::Relaxed) && a.duration.is_none_or(|d| start.elapsed() < d) {
        let serviced = recording.service();
        for (_, event) in &serviced.events {
            if let Some(text) = describe_event(event, &recording.started().sources) {
                println!("{text}");
            }
        }
        for e in &serviced.errors {
            println!("engine error: {e}");
        }
        if let Some(why) = recording.failure() {
            failure = Some(why);
            break;
        }
        let progress = recording.progress();
        if let Some(target) = &a.stream {
            for change in link.update(&progress) {
                println!("{}", change.describe(&target.url));
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
            println!("{text}");
        }
        let wait = serviced.wait.unwrap_or(Duration::from_millis(50));
        std::thread::sleep(wait.min(Duration::from_millis(50)));
    }
    let elapsed = start.elapsed();
    println!("stopping...");
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
    let report = recording.finish();
    let p = &report.progress;
    println!(
        "--- {:.1} s {}",
        elapsed.as_secs_f64(),
        if a.stream.is_some() {
            "streamed"
        } else {
            "recorded"
        }
    );
    println!(
        "video: {} frames exported, {} encoded, {} dropped at export, {} skipped before the encoder, {} dropped at the encoder",
        p.frames_exported, p.frames_encoded, p.export_drops, p.encoder_skipped, p.encoder_drops
    );
    for s in &p.sources {
        println!(
            "{}: {} frames, {} resyncs, {} errors",
            s.name, s.frames, s.resyncs, s.errors
        );
    }
    println!(
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
    println!(
        "audio tail: {} encoder flushes, {} lost at the end; {} packets dropped before the first video packet",
        p.audio_flushes, p.audio_tail_lost, p.start_drops
    );
    if a.stream.is_some() {
        println!(
            "stream: connected {} times, {} reconnect attempts, {} items dropped by the transport",
            p.connects, p.reconnect_attempts, p.transport_drops
        );
    }
    if let Some(out) = &file {
        let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
        println!("file: {} ({size} bytes)", out.display());
    }
    for e in &report.shutdown_errors {
        println!("shutdown error: {e}");
    }
    let failure = failure.or(report.failure);
    let url = a.stream.as_ref().map_or("", |t| t.url.as_str());
    match failure {
        // The stream failed before it ever reached the server.
        Some(why) if a.stream.is_some() && p.connects == 0 => {
            Err(format!("never connected to {url}: {why}"))
        }
        Some(why) => Err(why),
        None if a.stream.is_some() && p.connects == 0 => Err(format!("never connected to {url}")),
        None if p.packets_muxed() == 0 => Err(format!(
            "no packet reached the {}",
            if a.stream.is_some() { "stream" } else { "file" }
        )),
        None if !report.shutdown_errors.is_empty() => Err("the engine did not stop cleanly".into()),
        None => Ok(()),
    }
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
            println!("no stream id ({} is not set)", target.key_env);
            Ok(None)
        }
        (Some(_), _) => {
            println!("stream key: from {}", target.key_env);
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
}
