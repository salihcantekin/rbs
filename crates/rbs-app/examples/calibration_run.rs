//! *Test this computer* on real encoders (ADR-0087 step D1, control API 1.55), through
//! the app's studio host and an in-process control API client, as `calibration.run`
//! runs in the app. The studio runs a 1920x1080 main canvas at 60 fps with no source;
//! the default recording profile writes into `folder`. Prints every `calibration.state`
//! and ends with the `done` (or `failed`) state as JSON.
//!
//! ```sh
//! cargo run --release -p rbs-app --example calibration_run -- /tmp/rbs-calibration
//! ```
//!
//! `calibration.toml` is written into `<folder>/config`, never the user's own config
//! folder. The run takes at most about 65 s and needs no device or permission.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rbs_app::record_profile::RecordProfile;
use rbs_app::studio_host::{Command, HostConfig, SourceKind, StudioHost, Update};
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;

/// Longer than the run's own limit and grace.
const LIMIT: Duration = Duration::from_secs(90);

struct JsonSender(mpsc::Sender<serde_json::Value>);

impl rbs_api::transport::ConnectionSender for JsonSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), rbs_api::transport::SendError> {
        let v = serde_json::from_str(text).map_err(|_| rbs_api::transport::SendError::Closed)?;
        self.0
            .send(v)
            .map_err(|_| rbs_api::transport::SendError::Closed)
    }

    fn close(&mut self, _reason: rbs_api::transport::CloseReason) {}
}

fn call(
    handle: &rbs_api::transport::ConnectionHandle,
    rx: &Receiver<serde_json::Value>,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let text = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    handle.push_message(text.to_string());
    loop {
        let v = rx.recv_timeout(LIMIT).expect("an answer");
        if v["id"] == id {
            return v;
        }
    }
}

fn main() -> ExitCode {
    let folder = std::env::args().nth(1).map_or_else(
        || std::env::temp_dir().join("rbs-calibration"),
        PathBuf::from,
    );
    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::canvas(Size::new(1920, 1080), FrameRate::FPS_60);
    config.preview = None;
    config.config_dir = Some(folder.join("config"));
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
    let wait = |rx: &Receiver<Update>| rx.recv_timeout(Duration::from_secs(60));
    loop {
        match wait(&rx) {
            Ok(Update::Started { encoders, .. }) => {
                let names: Vec<_> = encoders.iter().map(|e| e.name.as_str()).collect();
                rbs_crash::outln!("studio started; video encoders: {names:?}");
                break;
            }
            Ok(Update::Failed(e)) => {
                rbs_crash::errln!("the studio did not start: {e}");
                return ExitCode::FAILURE;
            }
            Ok(_) => {}
            Err(_) => {
                rbs_crash::errln!("the studio did not answer");
                return ExitCode::FAILURE;
            }
        }
    }
    let mut profile = RecordProfile::new();
    profile.folder = folder.join("rec");
    host.send(Command::SetProfile(profile));
    // The host takes commands in order: once this answer comes, the profile is set.
    host.send(Command::ListTargets(SourceKind::Screen));
    loop {
        match wait(&rx) {
            Ok(Update::Targets(SourceKind::Screen, _)) => break,
            Ok(_) => {}
            Err(_) => {
                rbs_crash::errln!("the studio did not answer");
                return ExitCode::FAILURE;
            }
        }
    }

    let server = rbs_api::session::ServerConfig::new(rbs_api::request::ClientInfo::new(
        "rbs",
        "calibration_run",
    ))
    .expect("server config");
    let (api, endpoint) = rbs_api::ApiServer::start(
        server,
        rbs_api::TokenStore::new(),
        rbs_api::thread::ApiOptions::new(host.api_waker()),
    )
    .expect("the API starts");
    host.attach_api(endpoint, None);
    let (jtx, jrx) = mpsc::channel();
    let handle = api
        .inbox()
        .connect(
            rbs_api::transport::PeerInfo::same_user_ipc(),
            Box::new(JsonSender(jtx)),
            Box::new(|| {}),
        )
        .expect("a connection");
    call(
        &handle,
        &jrx,
        1,
        "session.hello",
        serde_json::json!({"protocols": ["1.55"], "client": {"name": "calibration_run", "version": "1"}}),
    );
    call(
        &handle,
        &jrx,
        2,
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    let started = Instant::now();
    let run = call(
        &handle,
        &jrx,
        3,
        "calibration.run",
        serde_json::json!({"profile": "default"}),
    );
    if run.get("error").is_some() {
        rbs_crash::errln!("calibration.run refused: {run}");
        return ExitCode::FAILURE;
    }
    loop {
        let Ok(v) = jrx.recv_timeout(LIMIT) else {
            rbs_crash::errln!("no calibration.state within {LIMIT:?}");
            return ExitCode::FAILURE;
        };
        if v["params"]["topic"] != "calibration.state" {
            continue;
        }
        let data = &v["params"]["data"];
        let t = started.elapsed().as_secs_f64();
        match data["state"].as_str() {
            Some("done" | "failed") => {
                rbs_crash::outln!("after {t:.1} s:");
                rbs_crash::outln!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
                let estimate = call(
                    &handle,
                    &jrx,
                    4,
                    "record.estimate",
                    serde_json::json!({"profile": "default"}),
                );
                rbs_crash::outln!(
                    "record.estimate max_tracks: {}",
                    estimate["result"]["max_tracks"]
                );
                return if data["state"] == "done" {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
            }
            _ => rbs_crash::outln!("{t:6.1} s  {data}"),
        }
    }
}
