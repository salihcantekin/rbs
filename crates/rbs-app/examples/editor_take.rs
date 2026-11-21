//! Makes real recordings for a manual editor import (ADR-0089 decision 9): the main
//! display and the default microphone, through the app's studio host, with the
//! recording profile's editor project on. Each take adds three markers while it runs
//! and, when it stops, writes `<take>.rbs.toml` and the FCP7 XML `<take>.xml` (for
//! DaVinci Resolve and Premiere Pro) next to the media; `project.export` then adds
//! `<take>.fcpxml` (for Final Cut Pro).
//!
//! ```sh
//! cargo run --release -p rbs-app --example editor_take -- <folder> [seconds] [mp4,mov,mkv]
//! ```
//!
//! One take per container named (default `mp4,mov`), each `seconds` long (default
//! 15), named `rbs-take-<container>`. A take whose file already exists is refused, so
//! nothing is replaced. Exits with an error when a take does not stop cleanly or its
//! project is missing.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rbs_app::record_profile::{API_PROFILE_ID, EditorProject, Format, RecordProfile};
use rbs_app::studio_host::{Command, HostConfig, RecordingUpdate, SourceKind, StudioHost, Update};

const WAIT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(folder) = args.next().map(PathBuf::from) else {
        rbs_crash::errln!("usage: editor_take <folder> [seconds] [mp4,mov,mkv]");
        return ExitCode::FAILURE;
    };
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    let containers = args.next().unwrap_or_else(|| "mp4,mov".into());
    let folder = std::path::absolute(&folder).expect("an absolute folder");
    std::fs::create_dir_all(&folder).expect("the folder");

    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        rbs_app::platform::adapters,
        || {
            // `RBS_ALLOW_SOFTWARE_GPU=1` lets a machine without a GPU try it.
            let allow_software =
                std::env::var_os("RBS_ALLOW_SOFTWARE_GPU").is_some_and(|v| v == "1");
            rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest {
                allow_software,
                ..rbs_gpu::GpuRequest::default()
            })
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

    loop {
        match rx.recv_timeout(WAIT) {
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
    for kind in [SourceKind::Screen, SourceKind::AudioInput] {
        host.send(Command::ListTargets(kind));
        let target = loop {
            match rx.recv_timeout(WAIT) {
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
                rbs_crash::outln!("adding {kind:?}: {}", target.name);
                host.send(Command::AddSource { kind, target });
            }
            None => rbs_crash::outln!("no {kind:?} to add"),
        }
    }
    let mut profile = RecordProfile::new();
    profile.folder = folder.clone();
    profile.editor_project = Some(EditorProject::Xmeml);
    host.send(Command::SetProfile(profile));
    let mut api = Api::attach(&host);

    let mut ok = true;
    for name in containers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (format, ext) = match name {
            "mp4" => (Format::Mp4, "mp4"),
            "mov" => (Format::Mov, "mov"),
            "mkv" => (Format::Mkv, "mkv"),
            other => {
                rbs_crash::errln!("unknown container {other}: mp4, mov or mkv");
                ok = false;
                continue;
            }
        };
        let path = folder.join(format!("rbs-take-{ext}.{ext}"));
        if path.exists() {
            rbs_crash::errln!("{} exists; nothing replaced", path.display());
            ok = false;
            continue;
        }
        ok &= take(&host, &rx, &mut api, &path, format, seconds);
    }
    drop(api);
    drop(host);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// One take: start, three markers spread over the take, stop, then check the files.
fn take(
    host: &StudioHost,
    rx: &mpsc::Receiver<Update>,
    api: &mut Api,
    path: &Path,
    format: Format,
    seconds: u64,
) -> bool {
    rbs_crash::outln!("recording {}", path.display());
    // As the window records (ADR-0164 step 6): the saved settings, then the control
    // API's `record.start_to_file`.
    let mut profile = RecordProfile::new();
    if let Some(folder) = path.parent() {
        profile.folder = folder.to_owned();
    }
    profile.use_format(format);
    profile.audio_kbps = 192;
    profile.editor_project = Some(EditorProject::Xmeml);
    host.send(Command::SetProfile(profile));
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::RecordStartToFile(
            rbs_api::request::RecordStartToFile {
                profile: API_PROFILE_ID.into(),
                path: path.display().to_string(),
                canvas: None,
            },
        )),
        reply: rbs_app::studio_host::ApiReply::Quiet,
    });
    let start = Instant::now();
    let length = Duration::from_secs(seconds.max(4));
    let marks = [
        ("Intro", "blue", 1),
        ("Good take", "green", 2),
        ("Cut here", "red", 3),
    ];
    let mut next_mark = 0;
    let mut stop_sent = false;
    loop {
        if !stop_sent && next_mark < marks.len() {
            let (label, color, n) = marks[next_mark];
            if start.elapsed() >= length * n / 4 {
                // A new session for each marker keeps under the two-a-second budget.
                api.reconnect();
                let answer = api.call(
                    "marker.add",
                    serde_json::json!({"label": label, "color": color}),
                );
                rbs_crash::outln!(
                    "{:.1} s: marker {label}: {answer}",
                    start.elapsed().as_secs_f64()
                );
                next_mark += 1;
            }
        }
        if !stop_sent && start.elapsed() >= length {
            rbs_crash::outln!("{:.1} s: stopping", start.elapsed().as_secs_f64());
            host.send(Command::Api {
                call: Box::new(rbs_api::Command::RecordStop(rbs_api::request::OnCanvases {
                    canvas: Some(rbs_api::types::OutputTarget::Main),
                })),
                reply: rbs_app::studio_host::ApiReply::Quiet,
            });
            stop_sent = true;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Update::Recording(u)) => {
                rbs_crash::outln!("{:.1} s: {u:?}", start.elapsed().as_secs_f64());
                match u {
                    RecordingUpdate::NotStarted(_) => return false,
                    RecordingUpdate::Stopped {
                        failure, frames, ..
                    } => {
                        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        let dir = path.parent().unwrap_or(Path::new("."));
                        let manifest = dir.join(format!("{stem}.rbs.toml"));
                        let project = dir.join(format!("{stem}.xml"));
                        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        let fcpxml = api.call(
                            "project.export",
                            serde_json::json!({"recording": file_name, "format": "fcpxml"}),
                        );
                        rbs_crash::outln!("project.export fcpxml: {fcpxml}");
                        rbs_crash::outln!("manifest: {}", manifest.display());
                        rbs_crash::outln!("project:  {}", project.display());
                        let fine = failure.is_none()
                            && frames > 0
                            && manifest.exists()
                            && project.exists();
                        if !fine {
                            rbs_crash::errln!("the take did not end cleanly");
                        }
                        return fine;
                    }
                    _ => {}
                }
            }
            Ok(Update::Error(e)) => rbs_crash::outln!("error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop_sent && start.elapsed() > length + WAIT {
                    rbs_crash::errln!("the recording did not stop in time");
                    return false;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// An in-process control API connection to the host (same-user IPC: every scope).
struct Api {
    server: rbs_api::ApiServer,
    handle: rbs_api::transport::ConnectionHandle,
    rx: mpsc::Receiver<serde_json::Value>,
    next: i64,
}

struct JsonSender(mpsc::Sender<serde_json::Value>);

impl rbs_api::transport::ConnectionSender for JsonSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), rbs_api::transport::SendError> {
        let v = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
        self.0
            .send(v)
            .map_err(|_| rbs_api::transport::SendError::Closed)
    }

    fn close(&mut self, _reason: rbs_api::transport::CloseReason) {}
}

impl Api {
    fn attach(host: &StudioHost) -> Self {
        let config = rbs_api::session::ServerConfig::new(rbs_api::request::ClientInfo::new(
            "rbs",
            "editor_take",
        ))
        .expect("a server config");
        let (server, endpoint) = rbs_api::ApiServer::start(
            config,
            rbs_api::TokenStore::new(),
            rbs_api::thread::ApiOptions::new(host.api_waker()),
        )
        .expect("the control API starts");
        host.attach_api(endpoint, None);
        let (handle, rx) = Self::connect(&server);
        let mut api = Self {
            server,
            handle,
            rx,
            next: 1,
        };
        api.hello();
        api
    }

    fn connect(
        server: &rbs_api::ApiServer,
    ) -> (
        rbs_api::transport::ConnectionHandle,
        mpsc::Receiver<serde_json::Value>,
    ) {
        let (tx, rx) = mpsc::channel();
        let handle = server
            .inbox()
            .connect(
                rbs_api::transport::PeerInfo::same_user_ipc(),
                Box::new(JsonSender(tx)),
                Box::new(|| {}),
            )
            .expect("a connection");
        (handle, rx)
    }

    fn reconnect(&mut self) {
        let (handle, rx) = Self::connect(&self.server);
        self.handle = handle;
        self.rx = rx;
        self.hello();
    }

    fn hello(&mut self) {
        self.call(
            "session.hello",
            serde_json::json!({"protocols": ["1.58"], "client": {"name": "editor_take", "version": "1"}}),
        );
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next;
        self.next += 1;
        let text =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            match self.rx.recv_timeout(WAIT) {
                Ok(v) if v["id"] == id => return v,
                Ok(_) => {}
                Err(_) => return serde_json::json!({"error": "no answer"}),
            }
        }
    }
}
