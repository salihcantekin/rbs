//! `rbs-cli app stream start|stop|status|destinations|destination`: control the stream of a running RBS desktop
//! app through its control API over local IPC (control API spec, "Streaming").
//!
//! The app goes live with the stream settings saved in its Settings, Stream and the key
//! in the system's credential store, as its own Go live button does; nothing about the
//! server or the key is passed or printed here. RBS serves local IPC while remote
//! control is on (Settings, Remote control).

use std::io::Write;

use rbs_api_ipc::Endpoint;
use serde_json::{Value, json};

use crate::args::{AppCommand, DestinationAction, StreamControl};
use crate::control::{Reach, RunningApp};

/// The protocol minor version that added `stream.*`.
const STREAM_MINOR: u32 = 1;

/// The protocol minor version that added `stream.destinations` and
/// `stream.destination.*` (ADR-0127).
const DESTINATIONS_MINOR: u32 = 74;

/// Runs `command` on the RBS serving `ipc`; what it says goes to `out`.
pub fn run(ipc: &Endpoint, command: &AppCommand, out: &mut dyn Write) -> Result<(), String> {
    let (scope, what) = match command {
        AppCommand::Stream(StreamControl::Status { .. }) => ("read", "reading the stream state"),
        AppCommand::Stream(StreamControl::Destinations { .. }) => {
            ("read", "reading the stream destinations")
        }
        AppCommand::Stream(
            StreamControl::Start | StreamControl::Stop | StreamControl::Destination { .. },
        ) => ("control", "stream control"),
    };
    let mut app = match RunningApp::connect(ipc, scope, what) {
        Reach::Connected(app) => app,
        Reach::NotRunning => {
            return Err(
                "RBS is not running, or remote control is off: turn on Allow remote control in its Settings, Remote control.".into(),
            );
        }
        Reach::Unusable(why) => return Err(format!("RBS could not be reached ({why}).")),
    };
    match app.speaks(STREAM_MINOR) {
        Some(true) => {}
        Some(false) => {
            return Err("the running RBS is too old to control its stream; update it.".into());
        }
        None => {
            return Err(
                "the running RBS speaks a control protocol this rbs-cli does not know; use the rbs-cli of the same release.".into(),
            );
        }
    }
    if matches!(
        command,
        AppCommand::Stream(StreamControl::Destinations { .. } | StreamControl::Destination { .. })
    ) && app.speaks(DESTINATIONS_MINOR) == Some(false)
    {
        return Err("the running RBS is too old to control stream destinations; update it.".into());
    }
    let written = |e: std::io::Error| e.to_string();
    match command {
        AppCommand::Stream(StreamControl::Destinations { json }) => {
            let r = app
                .call("stream.destinations", json!({}))
                .map_err(|e| e.message)?;
            if *json {
                writeln!(out, "{r}").map_err(written)
            } else {
                write!(out, "{}", list_destinations(&r)).map_err(written)
            }
        }
        AppCommand::Stream(StreamControl::Destination { action, id }) => {
            let (method, params, done) = match action {
                DestinationAction::Start => (
                    "stream.destination.start",
                    json!({"id": id}),
                    format!("Destination {id} is starting."),
                ),
                DestinationAction::Stop => (
                    "stream.destination.stop",
                    json!({"id": id}),
                    format!("Destination {id} was stopped."),
                ),
                DestinationAction::Enable => (
                    "stream.destination.set_enabled",
                    json!({"id": id, "enabled": true}),
                    format!("Go live now sends to destination {id}."),
                ),
                DestinationAction::Disable => (
                    "stream.destination.set_enabled",
                    json!({"id": id, "enabled": false}),
                    format!("Go live no longer sends to destination {id}."),
                ),
            };
            app.call(method, params).map_err(|e| e.message)?;
            writeln!(out, "{done}").map_err(written)
        }
        AppCommand::Stream(StreamControl::Start) => {
            let r = app.call("stream.start", json!({})).map_err(|e| e.message)?;
            writeln!(
                out,
                "Going live with the saved stream settings (video: {}, audio: {}). `rbs-cli app stream status` shows the connection.",
                r["video_encoder"].as_str().unwrap_or("?"),
                r["audio_encoder"].as_str().unwrap_or("none"),
            )
            .map_err(written)
        }
        AppCommand::Stream(StreamControl::Stop) => {
            app.call("stream.stop", json!({})).map_err(|e| e.message)?;
            writeln!(out, "The stream was ended.").map_err(written)
        }
        AppCommand::Stream(StreamControl::Status { json }) => {
            let r = app
                .call("stream.status", json!({}))
                .map_err(|e| e.message)?;
            let state = &r["data"];
            if *json {
                writeln!(out, "{state}").map_err(written)
            } else {
                writeln!(out, "{}", describe(state)).map_err(written)
            }
        }
    }
}

/// The saved destinations (`stream.destinations`) as lines: id, name, service, canvas,
/// whether Go live sends to it, whether a server is saved and the encoding.
fn list_destinations(r: &Value) -> String {
    let list = r["destinations"].as_array().map_or(&[][..], Vec::as_slice);
    if list.is_empty() {
        return "No destinations.\n".into();
    }
    let mut text = String::new();
    for d in list {
        let name = d["name"].as_str().filter(|n| !n.is_empty());
        text.push_str(&format!(
            "{}  {}  {}  {}  {}  {}  {}\n",
            d["id"].as_u64().unwrap_or(0),
            name.unwrap_or("(unnamed)"),
            d["service"].as_str().unwrap_or("?"),
            d["canvas"].as_str().unwrap_or("?"),
            if d["enabled"] == true { "on" } else { "off" },
            if d["configured"] == true {
                "configured"
            } else {
                "no server"
            },
            d["encoding"].as_str().unwrap_or("?"),
        ));
    }
    text
}

/// `n` times, in words.
fn times(n: u64) -> String {
    if n == 1 {
        "once".into()
    } else {
        format!("{n} times")
    }
}

/// A stream state (`stream.status`, `stream.state`) in one line.
pub fn describe(state: &Value) -> String {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    match state["state"].as_str() {
        Some("live") => {
            let protocol = state["protocol"]
                .as_str()
                .unwrap_or("?")
                .to_ascii_uppercase();
            let link = if state["connected"] == true {
                "connected"
            } else if n(&state["connects"]) == 0 {
                "connecting"
            } else {
                "reconnecting"
            };
            format!(
                "Live over {protocol}, {link} (connected {}, {} reconnect attempts). Video: {}, audio: {}.",
                times(n(&state["connects"])),
                n(&state["reconnect_attempts"]),
                state["video_encoder"].as_str().unwrap_or("?"),
                state["audio_encoder"].as_str().unwrap_or("none"),
            )
        }
        Some("idle") => {
            let last = &state["last"];
            if last.is_null() {
                return "Not live.".into();
            }
            let how = match last["failure"].as_str() {
                Some(why) => format!("stopped on its own: {why}"),
                None => "was ended".into(),
            };
            let connects = n(&last["connects"]);
            let reached = if connects == 0 {
                "never reached the server".to_owned()
            } else {
                format!("connected {}", times(connects))
            };
            format!(
                "Not live. The last stream {how} ({} frames, {} dropped, {reached}).",
                n(&last["frames"]),
                n(&last["dropped"]),
            )
        }
        _ => format!("RBS answered a state this rbs-cli does not know: {state}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn destinations_list_one_line_each_without_a_server_or_key() {
        let r = json!({"destinations": [
            {"id": 0, "name": "Twitch", "service": "twitch", "canvas": "main",
             "enabled": true, "configured": true, "encoding": "shared"},
            {"id": 2, "name": "", "service": "custom", "canvas": "main",
             "enabled": false, "configured": false, "encoding": "own"},
        ]});
        assert_eq!(
            list_destinations(&r),
            "0  Twitch  twitch  main  on  configured  shared\n\
             2  (unnamed)  custom  main  off  no server  own\n"
        );
        assert_eq!(
            list_destinations(&json!({"destinations": []})),
            "No destinations.\n"
        );
    }
    use rbs_api::request::{ClientInfo, Empty, StreamStarted};
    use rbs_api::types::{Snapshot, StreamProtocol, StreamState, StreamSummary};
    use rbs_api::{
        ApiError, ApiOptions, ApiServer, Command, ControlTarget, ControlTransport, ErrorKind,
        EventPublisher, Response, ServerConfig, TokenStore,
    };

    #[test]
    fn states_are_described_in_one_line() {
        let live = serde_json::to_value(StreamState::Live {
            available: true,
            protocol: StreamProtocol::Rtmp,
            connected: true,
            connects: 1,
            reconnect_attempts: 0,
            video_encoder: "VideoToolbox".into(),
            audio_encoder: Some("AudioToolbox AAC".into()),
            held: false,
            held_ms: 0,
            hold_picture: None,
            hold_scene: None,
            hold_sound: None,
            hold_fallback: false,
            hold_sound_fallback: false,
            hold_fallback_reason: None,
            destinations: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            describe(&live),
            "Live over RTMP, connected (connected once, 0 reconnect attempts). Video: VideoToolbox, audio: AudioToolbox AAC."
        );
        let reconnecting = serde_json::to_value(StreamState::Live {
            available: true,
            protocol: StreamProtocol::Srt,
            connected: false,
            connects: 2,
            reconnect_attempts: 3,
            video_encoder: "NVENC".into(),
            audio_encoder: None,
            held: false,
            held_ms: 0,
            hold_picture: None,
            hold_scene: None,
            hold_sound: None,
            hold_fallback: false,
            hold_sound_fallback: false,
            hold_fallback_reason: None,
            destinations: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            describe(&reconnecting),
            "Live over SRT, reconnecting (connected 2 times, 3 reconnect attempts). Video: NVENC, audio: none."
        );
        let idle = serde_json::to_value(StreamState::idle()).unwrap();
        assert_eq!(describe(&idle), "Not live.");
        let failed = serde_json::to_value(StreamState::Idle {
            available: true,
            last: Some(StreamSummary {
                failure: Some("unknown stream key".into()),
                frames: 10,
                dropped: 1,
                connects: 0,
            }),
        })
        .unwrap();
        assert_eq!(
            describe(&failed),
            "Not live. The last stream stopped on its own: unknown stream key (10 frames, 1 dropped, never reached the server)."
        );
    }

    /// A fresh IPC endpoint per test; Unix socket paths are short.
    fn endpoint(name: &str) -> Endpoint {
        let id = format!("{name}-{}", std::process::id());
        if cfg!(windows) {
            Endpoint(format!(r"\\.\pipe\rbs-cli-app-test-{id}").into())
        } else {
            let dir = std::env::temp_dir().join(format!("rbs-cli-app-{id}"));
            let _ = std::fs::remove_dir_all(&dir);
            Endpoint(dir.join("c.sock"))
        }
    }

    /// A running "RBS" whose control thread streams as the app does, in memory.
    struct App {
        ipc: rbs_api_ipc::IpcTransport,
        api: Option<ApiServer>,
        wake: std::sync::mpsc::Sender<bool>,
        control: Option<std::thread::JoinHandle<()>>,
    }

    struct Streamer(StreamState);

    impl ControlTarget for Streamer {
        fn handle(
            &mut self,
            command: Command,
            _: &mut EventPublisher,
        ) -> Result<Response, ApiError> {
            let live = matches!(self.0, StreamState::Live { .. });
            match command {
                Command::StreamStart(_) if live => Err(ApiError::new(
                    ErrorKind::Conflict,
                    "a stream is already running",
                )),
                Command::StreamStart(_) => {
                    self.0 = StreamState::Live {
                        available: true,
                        protocol: StreamProtocol::Rtmp,
                        connected: false,
                        connects: 0,
                        reconnect_attempts: 0,
                        video_encoder: "Fake".into(),
                        audio_encoder: Some("Fake AAC".into()),
                        held: false,
                        held_ms: 0,
                        hold_picture: None,
                        hold_scene: None,
                        hold_sound: None,
                        hold_fallback: false,
                        hold_sound_fallback: false,
                        hold_fallback_reason: None,
                        destinations: Vec::new(),
                    };
                    Ok(Response::StreamStarted(StreamStarted {
                        video_encoder: "Fake".into(),
                        audio_encoder: Some("Fake AAC".into()),
                    }))
                }
                Command::StreamStop(_) if !live => {
                    Err(ApiError::new(ErrorKind::Conflict, "nothing streams"))
                }
                Command::StreamStop(_) => {
                    self.0 = StreamState::Idle {
                        available: true,
                        last: Some(StreamSummary {
                            failure: None,
                            frames: 60,
                            dropped: 0,
                            connects: 0,
                        }),
                    };
                    Ok(Response::Done(Empty {}))
                }
                Command::StreamStatus(_) => Ok(Response::StreamStatus(Snapshot {
                    seq: Default::default(),
                    data: self.0.clone(),
                })),
                other => Err(rbs_api::control::unsupported(&other)),
            }
        }
    }

    impl App {
        fn start(endpoint: &Endpoint) -> App {
            let (wake, woken) = std::sync::mpsc::channel();
            let waker = std::sync::Mutex::new(wake.clone());
            let (api, control_end) = ApiServer::start(
                ServerConfig::new(ClientInfo::new("RBS", "test")).unwrap(),
                TokenStore::new(),
                ApiOptions::new(move || {
                    let _ = waker.lock().unwrap().send(true);
                }),
            )
            .unwrap();
            let mut target = Streamer(StreamState::idle());
            let control = std::thread::spawn(move || {
                let mut events = EventPublisher::new();
                while let Ok(true) = woken.recv() {
                    while control_end.serve_one(&mut target, &mut events) {}
                }
            });
            let mut ipc =
                rbs_api_ipc::IpcTransport::new(rbs_api_ipc::IpcConfig::new(endpoint.clone()));
            ipc.start(api.inbox()).unwrap();
            App {
                ipc,
                api: Some(api),
                wake,
                control: Some(control),
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            self.ipc.stop();
            self.api.take();
            let _ = self.wake.send(false);
            if let Some(c) = self.control.take() {
                let _ = c.join();
            }
            if !cfg!(windows) {
                let _ = std::fs::remove_dir_all(self.ipc.endpoint().0.parent().unwrap());
            }
        }
    }

    fn run_text(e: &Endpoint, command: AppCommand) -> Result<String, String> {
        let mut out = Vec::new();
        run(e, &command, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_running_app_goes_live_reports_and_ends_its_stream() {
        let e = endpoint("live");
        let _app = App::start(&e);
        let status = AppCommand::Stream(StreamControl::Status { json: false });
        assert_eq!(run_text(&e, status.clone()).unwrap(), "Not live.\n");
        let started = run_text(&e, AppCommand::Stream(StreamControl::Start)).unwrap();
        assert!(
            started.contains("video: Fake, audio: Fake AAC"),
            "{started}"
        );
        let again = run_text(&e, AppCommand::Stream(StreamControl::Start)).unwrap_err();
        assert!(again.contains("already running"), "{again}");
        let live = run_text(&e, status.clone()).unwrap();
        assert!(live.starts_with("Live over RTMP, connecting"), "{live}");
        let json = run_text(&e, AppCommand::Stream(StreamControl::Status { json: true })).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["state"], "live", "{json}");
        assert_eq!(
            run_text(&e, AppCommand::Stream(StreamControl::Stop)).unwrap(),
            "The stream was ended.\n"
        );
        let idle = run_text(&e, status).unwrap();
        assert!(idle.contains("was ended (60 frames"), "{idle}");
        let none = run_text(&e, AppCommand::Stream(StreamControl::Stop)).unwrap_err();
        assert!(none.contains("nothing streams"), "{none}");
    }

    #[test]
    fn without_a_running_app_it_says_how_to_reach_one() {
        let e = endpoint("none");
        let why = run_text(&e, AppCommand::Stream(StreamControl::Start)).unwrap_err();
        assert!(why.contains("Allow remote control"), "{why}");
    }
}
