//! Sending to the virtual camera through the studio host (ADR-0066 decisions 12 and
//! 13) with the reference camera adapter: start and stop, a refused start, sending on
//! after a canvas change restarts the studio, and `vcam.*` from a control API client. The engine runs on a `TestClock` that never
//! moves; the test waits for the host's updates, with a bound only as a hang guard
//! (ADR-0032). Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::GpuContext;
use rbs_record::RecordAdapters;
use rbs_studio_host::studio_host::{
    CameraFactory, CameraSetup, CameraStatus, Command, HostConfig, StudioHost, Update,
};
use rbs_vcam::{InstallState, VirtualCameraFactory};
use rbs_vcam_test::TestVcamFactory;

/// Hang guard only: every wait ends as soon as its update arrives.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    crate::gpu()
}

fn studio(
    gpu: Arc<GpuContext>,
    camera: Option<Arc<TestVcamFactory>>,
) -> (StudioHost, Receiver<Update>) {
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.virtual_camera = camera.map(|c| CameraFactory(c as Arc<dyn VirtualCameraFactory>));
    let (tx, rx) = mpsc::channel();
    let host = StudioHost::spawn(
        config,
        || (RecordAdapters::new(), Vec::new()),
        move || Ok(gpu),
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
        crate::watch::forward(tx),
    )
    .unwrap();
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    (host, rx)
}

/// The next update `pick` accepts; others are skipped.
fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let u = rx.recv_timeout(left).expect("the host answered");
        if let Some(t) = pick(u) {
            return t;
        }
    }
}

fn camera(rx: &Receiver<Update>) -> Option<CameraStatus> {
    next(rx, |u| match u {
        Update::VirtualCamera(s) => Some(s),
        _ => None,
    })
}

/// The camera settings the window sends before its first start, with the
/// install state not read yet (starting says whether it is added).
fn ready(host: &StudioHost) {
    host.send(Command::SetVirtualCamera(CameraSetup {
        state: None,
        width: 640,
        height: 360,
        fps: FrameRate::new(15, 1).unwrap(),
        busy: false,
    }));
}

/// The window's start (ADR-0164 decision 4): `vcam.start` with the settings
/// [`set_up`] gave.
fn start(host: &StudioHost) {
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::VcamStart),
        reply: rbs_studio_host::studio_host::ApiReply::QuietIf(rbs_api::ErrorKind::Conflict),
    });
}

#[test]
fn the_camera_sends_until_stopped_and_across_a_canvas_change() {
    let Some(gpu) = gpu() else { return };
    let factory = Arc::new(TestVcamFactory::new());
    factory.install().unwrap();
    let (host, rx) = studio(gpu, Some(Arc::clone(&factory)));
    ready(&host);
    start(&host);
    let s = camera(&rx).expect("sending");
    assert_eq!((s.width, s.height, s.rate), (640, 360, 15));
    assert!(
        s.counters.is_some(),
        "the reference adapter shares its counters"
    );
    assert_eq!(s.failure, None);

    host.send(crate::set_canvas(Size::new(1280, 720), FrameRate::FPS_30));
    next(&rx, |u| match u {
        Update::Canvas { .. } => Some(()),
        Update::Error(e) | Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    // Still sending, at the camera's own size, with no error on the way.
    host.send(crate::stop_camera());
    next(&rx, |u| match u {
        Update::Error(e) => panic!("{e}"),
        Update::VirtualCamera(None) => Some(()),
        _ => None,
    });
    // Stopping again is harmless.
    host.send(crate::stop_camera());
    assert_eq!(camera(&rx), None);
}

#[test]
fn a_start_is_refused_without_the_camera() {
    let Some(gpu) = gpu() else { return };
    // Not added.
    let (host, rx) = studio(Arc::clone(&gpu), Some(Arc::new(TestVcamFactory::new())));
    ready(&host);
    start(&host);
    let error = refused(&rx);
    assert!(
        error.starts_with("The virtual camera did not start"),
        "{error}"
    );
    drop(host);
    // No adapter on this platform.
    let (host, rx) = studio(gpu, None);
    ready(&host);
    start(&host);
    let error = refused(&rx);
    assert_eq!(
        error,
        "the virtual camera is not available on this computer"
    );
}

/// A refused start: the camera's state (still off) for the Sending card and the
/// error, in either order; returns the error.
fn refused(rx: &Receiver<Update>) -> String {
    let (mut error, mut off) = (None, false);
    next(rx, |u| {
        match u {
            Update::Error(e) => error = Some(e),
            Update::VirtualCamera(None) => off = true,
            Update::VirtualCamera(Some(s)) => panic!("sending: {s:?}"),
            _ => {}
        }
        (off && error.is_some()).then_some(())
    });
    error.unwrap()
}

struct ChannelSender(Sender<serde_json::Value>);

impl ConnectionSender for ChannelSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        self.0
            .send(serde_json::from_str(text).unwrap())
            .map_err(|_| SendError::Closed)
    }

    fn close(&mut self, _reason: CloseReason) {}
}

/// A same-user IPC client of the control API, in memory.
struct Client {
    handle: ConnectionHandle,
    rx: Receiver<serde_json::Value>,
    next: i64,
    events: Vec<serde_json::Value>,
}

impl Client {
    /// Serves the control API from `host` and connects to it, after `session.hello`.
    fn connect(host: &StudioHost) -> (ApiServer, Self) {
        let config = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
        let (api, endpoint) =
            ApiServer::start(config, TokenStore::new(), ApiOptions::new(host.api_waker())).unwrap();
        host.attach_api(endpoint, None);
        let (out_tx, out_rx) = mpsc::channel();
        let handle = api
            .inbox()
            .connect(
                PeerInfo::same_user_ipc(),
                Box::new(ChannelSender(out_tx)),
                Box::new(|| {}),
            )
            .unwrap();
        let mut c = Self {
            handle,
            rx: out_rx,
            next: 1,
            events: Vec::new(),
        };
        let hello = c.call(
            "session.hello",
            serde_json::json!({"protocols": ["1.28"], "client": {"name": "test", "version": "1"}}),
        );
        assert_eq!(
            hello["result"]["protocol"],
            rbs_api::session::PROTOCOL_VERSION,
            "{hello}"
        );
        (api, c)
    }

    /// Calls a method without parameters; events that arrive meanwhile are kept.
    fn call0(&mut self, method: &str) -> serde_json::Value {
        self.call(method, serde_json::json!({}))
    }

    /// Calls a method and returns its answer; events that arrive meanwhile are kept.
    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next;
        self.next += 1;
        let text =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an answer");
            if v["id"] == id {
                return v;
            }
            self.events.push(v);
        }
    }

    /// The next `vcam.changed` event whose data `ok` accepts; earlier ones are dropped.
    fn changed(&mut self, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let mine = |e: &serde_json::Value| {
            e["params"]["topic"] == "vcam.changed" && ok(&e["params"]["data"])
        };
        while !self.events.is_empty() {
            let e = self.events.remove(0);
            if mine(&e) {
                return e;
            }
        }
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an event");
            if mine(&v) {
                return v;
            }
        }
    }
}

/// Tells the host what Settings › Virtual camera knows, and waits until it has it (a
/// stop answers after the setup was taken).
fn set_up(host: &StudioHost, rx: &Receiver<Update>, state: InstallState) {
    host.send(Command::SetVirtualCamera(CameraSetup {
        state: Some(state),
        width: 640,
        height: 360,
        fps: FrameRate::new(15, 1).unwrap(),
        busy: false,
    }));
    host.send(crate::stop_camera());
    stopped(rx);
}

/// Waits until the app is told the camera stopped; counters on the way are skipped.
fn stopped(rx: &Receiver<Update>) {
    next(rx, |u| {
        matches!(u, Update::VirtualCamera(None)).then_some(())
    });
}

fn kind(answer: &serde_json::Value) -> &str {
    answer["error"]["data"]["kind"].as_str().unwrap_or("none")
}

fn message(answer: &serde_json::Value) -> &str {
    answer["error"]["message"].as_str().unwrap_or("")
}

#[test]
fn a_control_api_client_starts_and_stops_the_camera() {
    let Some(gpu) = gpu() else { return };
    let factory = Arc::new(TestVcamFactory::new());
    let (host, rx) = studio(gpu, Some(Arc::clone(&factory)));
    let (api, mut c) = Client::connect(&host);
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["vcam.changed"]}),
    );
    assert!(sub["result"]["seq"].is_object(), "{sub}");

    // The app has not said what it knows yet: not guessed.
    let status = c.call0("vcam.status");
    let data = &status["result"]["data"];
    assert_eq!(data["state"], serde_json::Value::Null, "{status}");
    assert_eq!(data["sending"], false, "{status}");
    let early = c.call0("vcam.start");
    assert_eq!(kind(&early), "failed", "{early}");
    assert!(message(&early).contains("starting"), "{early}");

    // Not added: refused, saying where to add it.
    set_up(&host, &rx, InstallState::NotInstalled);
    c.changed(|d| d["state"] == "not_added" && d["width"] == 640 && d["rate"] == 15);
    let refused = c.call0("vcam.start");
    assert_eq!(kind(&refused), "failed", "{refused}");
    assert!(
        message(&refused).contains("Settings, Virtual camera"),
        "{refused}"
    );
    let stop = c.call0("vcam.stop");
    assert_eq!(kind(&stop), "conflict", "{stop}");
    // Only the window's own stop tells the window the camera's state (ADR-0168).
    assert!(
        rx.try_iter()
            .all(|u| !matches!(u, Update::VirtualCamera(_))),
        "a client's refused stop told the window"
    );

    // Settings is changing the camera: no start meanwhile.
    factory.install().unwrap();
    host.send(Command::SetVirtualCamera(CameraSetup {
        state: Some(InstallState::Installed),
        width: 640,
        height: 360,
        fps: FrameRate::new(15, 1).unwrap(),
        busy: true,
    }));
    host.send(crate::stop_camera());
    stopped(&rx);
    let busy = c.call0("vcam.start");
    assert_eq!(kind(&busy), "conflict", "{busy}");

    // Added: it starts with the saved format, and the app sees it as its own start.
    set_up(&host, &rx, InstallState::Installed);
    c.changed(|d| d["state"] == "added" && d["sending"] == false);
    let started = c.call0("vcam.start");
    assert!(started["result"].is_object(), "{started}");
    let s = camera(&rx).expect("the app sees it sending");
    assert_eq!((s.width, s.height, s.rate), (640, 360, 15));
    let on = c.changed(|d| d["sending"] == true);
    assert_eq!(
        on["params"]["data"]["frames"],
        serde_json::Value::Null,
        "{on}"
    );
    let status = c.call0("vcam.status");
    let data = &status["result"]["data"];
    assert_eq!(data["sending"], true, "{status}");
    assert!(data["frames"]["sent"].is_u64(), "{status}");
    assert_eq!(
        status["result"]["seq"]["vcam.changed"], on["params"]["seq"],
        "{status}"
    );
    let again = c.call0("vcam.start");
    assert_eq!(kind(&again), "conflict", "{again}");

    let stop = c.call0("vcam.stop");
    assert!(stop["result"].is_object(), "{stop}");
    stopped(&rx);
    c.changed(|d| d["sending"] == false && d["state"] == "added");
    let stop = c.call0("vcam.stop");
    assert_eq!(kind(&stop), "conflict", "{stop}");

    // A start from the app reaches API clients too.
    start(&host);
    assert!(camera(&rx).is_some());
    c.changed(|d| d["sending"] == true);
    // A second start (the app's button losing a race to an API client) shows the
    // camera that runs, with no error.
    start(&host);
    next(&rx, |u| match u {
        Update::Error(e) => panic!("{e}"),
        Update::VirtualCamera(Some(_)) => Some(()),
        _ => None,
    });
    drop(c);
    drop(api);
}

#[test]
fn the_api_says_when_this_computer_has_no_camera() {
    let Some(gpu) = gpu() else { return };
    let (host, _rx) = studio(gpu, None);
    let (api, mut c) = Client::connect(&host);
    let status = c.call0("vcam.status");
    assert_eq!(status["result"]["data"]["state"], "unavailable", "{status}");
    assert!(status["result"]["data"]["reason"].is_string(), "{status}");
    let refused = c.call0("vcam.start");
    assert_eq!(kind(&refused), "failed", "{refused}");
    assert!(message(&refused).contains("not available"), "{refused}");
    drop(c);
    drop(api);
}
