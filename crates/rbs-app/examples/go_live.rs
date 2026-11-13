//! End-to-end check of Go live on real devices, through the app's studio host as the
//! Live controls use it: the main display (and the default microphone) -> the platform's
//! encoders -> RTMP or SRT, with the stream key saved in and read from the system's
//! credential store (macOS Keychain, Windows Credential Manager).
//!
//! ```sh
//! # A local server, for example: ffmpeg -listen 1 -i rtmp://127.0.0.1/live/abc -c copy out.flv
//! RBS_STREAM_KEY=abc cargo run --release -p rbs-app --example go_live -- rtmp://127.0.0.1/live 20
//! ```
//!
//! With `RBS_STREAM_KEY` set, the key is saved in the store first (replacing a saved
//! one) and removed at the end; without it, the saved key is used. Prints every update
//! and exits with an error when the stream did not start or never connected.
//!
//! Record and stream at once (ADR-0047): with `RBS_ALSO_RECORD=/abs/path.mkv` a
//! recording starts once the stream runs and stops a quarter of the time before the
//! stream ends, while the stream goes on. It uses the recording defaults (12 Mbit/s,
//! its own encoder) or, with `RBS_RECORD_SAME=1`, the stream's video settings (the
//! encoders are shared).

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rbs_app::stream_key;
use rbs_app::stream_profile::{self, StreamProfile};
use rbs_app::studio_host::{
    Command, HostConfig, RecordRequest, RecordingUpdate, SourceKind, StreamRequest, StreamUpdate,
    StudioHost, Update,
};
use rbs_core::media::ContainerId;
use rbs_record::VideoSettings;
use rbs_transport::Secret;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "rtmp://127.0.0.1/live".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let server = match stream_profile::parse_server(&url) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let keys = stream_key::system();
    println!("key store: {}", keys.place());
    let new_key = std::env::var("RBS_STREAM_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());

    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.keys = Arc::clone(&keys);
    let host = StudioHost::spawn(
        config,
        rbs_app::platform::adapters,
        || {
            rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default())
                .map(Arc::new)
                .map_err(|e| e.to_string())
        },
        || {
            let mut engine = rbs_engine::Engine::new();
            engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
            engine
        },
        move |u| {
            let _ = tx.send(u);
        },
    )
    .expect("the studio host starts");
    let wait = |rx: &mpsc::Receiver<Update>| rx.recv_timeout(Duration::from_secs(60));

    // Started, then the main display and the microphone.
    loop {
        match wait(&rx) {
            Ok(Update::Started { encoders, .. }) => {
                let names: Vec<_> = encoders.iter().map(|e| e.name.as_str()).collect();
                println!("studio started; video encoders: {names:?}");
                break;
            }
            Ok(Update::Failed(e)) => {
                eprintln!("the studio did not start: {e}");
                return ExitCode::FAILURE;
            }
            Ok(_) => {}
            Err(_) => {
                eprintln!("the studio did not answer");
                return ExitCode::FAILURE;
            }
        }
    }
    for kind in [SourceKind::Screen, SourceKind::AudioInput] {
        host.send(Command::ListTargets(kind));
        let target = loop {
            match wait(&rx) {
                Ok(Update::Targets(k, t)) if k == kind => {
                    break t
                        .iter()
                        .find(|t| t.is_default && !t.picker)
                        .cloned()
                        .or_else(|| t.into_iter().find(|t| !t.picker));
                }
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        match target {
            Some(target) => {
                println!("adding {:?}: {}", kind, target.name);
                host.send(Command::AddSource { kind, target });
            }
            None => println!("no {kind:?} to add"),
        }
    }

    if let Some(k) = &new_key {
        host.send(Command::SaveStreamKey(Some(Secret::new(k.trim()))));
        loop {
            match wait(&rx) {
                Ok(Update::StreamKey(r)) => {
                    println!("key saved: {r:?}");
                    if r.is_err() {
                        return ExitCode::FAILURE;
                    }
                    break;
                }
                Ok(_) => {}
                Err(_) => return ExitCode::FAILURE,
            }
        }
    }

    let mut profile = StreamProfile::new();
    profile.server = url.clone();
    let e = &profile.encoding;
    host.send(Command::StartStream(StreamRequest {
        url: url.clone(),
        input: server.protocol.input(),
        needs_key: server.protocol.needs_key(),
        reconnect: profile.reconnect_policy(),
        transport_options: profile.transport_options(),
        encoder: None,
        video: e.video_settings(),
        audio_bitrate_bps: e.audio_bps(),
        size: None,
    }));
    let also_record = std::env::var_os("RBS_ALSO_RECORD").map(std::path::PathBuf::from);
    let same = std::env::var_os("RBS_RECORD_SAME").is_some_and(|v| v == "1");
    let record_video = if same {
        e.video_settings()
    } else {
        VideoSettings::default()
    };
    let mut record_state = also_record.as_ref().map(|_| 0u8); // 0 not yet, 1 sent, 2 stop sent
    let mut record_ok = also_record.is_none();
    let start = Instant::now();
    let mut ok = false;
    let mut ended = false;
    let mut sent_end = false;
    while !ended {
        let left = Duration::from_secs(seconds).saturating_sub(start.elapsed());
        if record_state == Some(1) && left < Duration::from_secs(seconds) / 4 {
            println!(
                "{:.1} s: stopping the recording",
                start.elapsed().as_secs_f64()
            );
            host.send(Command::StopRecording);
            record_state = Some(2);
        }
        if left.is_zero() && !sent_end {
            println!("{:.1} s: ending the stream", start.elapsed().as_secs_f64());
            host.send(Command::EndStream);
            sent_end = true;
        }
        let timeout = if sent_end {
            Duration::from_secs(30)
        } else {
            left.min(Duration::from_millis(500))
        };
        match rx.recv_timeout(timeout) {
            Ok(Update::Recording(u)) => {
                println!("{:.1} s: recording {u:?}", start.elapsed().as_secs_f64());
                match u {
                    RecordingUpdate::Stopped {
                        failure, frames, ..
                    } => record_ok = failure.is_none() && frames > 0,
                    RecordingUpdate::NotStarted(_) => record_state = None,
                    _ => {}
                }
            }
            Ok(Update::Stream(u)) => {
                println!("{:.1} s: {u:?}", start.elapsed().as_secs_f64());
                if let (StreamUpdate::Started { .. }, Some(0), Some(path)) =
                    (&u, record_state, &also_record)
                {
                    println!("recording to {} as well", path.display());
                    host.send(Command::StartRecording(RecordRequest {
                        path: path.clone(),
                        container: ContainerId::Matroska,
                        encoder: None,
                        video: Some(record_video.clone()),
                        audio_bitrate_bps: e.audio_bps(),
                        bit_depth: None,
                        size: None,
                    }));
                    record_state = Some(1);
                }
                match u {
                    StreamUpdate::NotStarted(_) | StreamUpdate::NoKey => {
                        ended = true;
                    }
                    StreamUpdate::Ended {
                        connects, failure, ..
                    } => {
                        ok = connects > 0 && failure.is_none();
                        ended = true;
                    }
                    _ => {}
                }
            }
            Ok(Update::Sources(rows)) => {
                for r in rows {
                    println!("source {}: {:?}", r.name, r.status);
                }
            }
            Ok(Update::Error(e)) => println!("error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) if sent_end => {
                eprintln!("the stream did not end in time");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if new_key.is_some() {
        host.send(Command::SaveStreamKey(None));
        while let Ok(u) = wait(&rx) {
            if let Update::StreamKey(r) = u {
                println!("key removed: {r:?}");
                break;
            }
        }
    }
    drop(host);
    if ok && record_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
