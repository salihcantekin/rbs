//! The sync offset remembered per audio device and the sync measurement through the
//! control API and the host (ADR-0126 decisions 7 to 10, protocol 1.66): an added or
//! retargeted source takes its device's value inside the step, a committed change
//! updates the device, undo puts the source back, applying to the device's other
//! sources, forgetting a device, and the measurement's refusals. In-memory transport,
//! reference adapters, an engine on a `TestClock`, no config folder (the file stays in
//! memory); waits end when the answer arrives, with a bound only as a hang guard
//! (ADR-0032). Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_api::request::ClientInfo;
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, SendError};
use rbs_api::{ApiServer, TokenStore};
use rbs_app::studio_host::{HostConfig, StudioHost, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use serde_json::{Value, json};

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("{e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no adapter was found"
        );
        rbs_crash::errln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

struct ChannelSender(Sender<Value>);

impl ConnectionSender for ChannelSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        self.0
            .send(serde_json::from_str(text).unwrap())
            .map_err(|_| SendError::Closed)
    }

    fn close(&mut self, _reason: CloseReason) {}
}

struct Client {
    handle: ConnectionHandle,
    rx: Receiver<Value>,
    next: i64,
    events: Vec<Value>,
}

impl Client {
    /// Calls a method and returns its answer; events that arrive meanwhile are kept.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let text = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        loop {
            let v = self.rx.recv_timeout(LIMIT).expect("an answer");
            if v["id"] == id {
                return v;
            }
            self.events.push(v);
        }
    }
}

fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if let Some(t) = pick(rx.recv_timeout(left).expect("the host answered")) {
            return t;
        }
    }
}

fn start() -> Option<(StudioHost, Receiver<Update>, ApiServer, Client)> {
    let gpu = gpu()?;
    let (tx, updates) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources
                .register_audio(Arc::new(TestAudioBackend::new()))
                .unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        || {
            Engine::with_clock(
                Arc::new(TestClock::new()),
                EngineConfig::default().swap_timeout(LIMIT),
            )
        },
        move |u| {
            let _ = tx.send(u);
        },
    )
    .unwrap();
    next(&updates, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let server = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
    let (api, endpoint) =
        ApiServer::start(server, TokenStore::new(), ApiOptions::new(host.api_waker())).unwrap();
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
    let mut c = Client {
        handle,
        rx: out_rx,
        next: 1,
        events: Vec::new(),
    };
    let hello = c.call(
        "session.hello",
        json!({"protocols": ["1.66"], "client": {"name": "test", "version": "1"}}),
    );
    assert_eq!(
        hello["result"]["protocol"],
        rbs_api::session::PROTOCOL_VERSION,
        "{hello}"
    );
    Some((host, updates, api, c))
}

fn ok(answer: &Value) -> &Value {
    assert!(answer.get("error").is_none(), "{answer}");
    &answer["result"]
}

fn kind(answer: &Value) -> &Value {
    &answer["error"]["data"]["kind"]
}

/// Adds a source of `kind` with its first target and returns its id.
fn add(c: &mut Client, kind: &str, name: &str) -> u64 {
    let targets = c.call("source.targets", json!({"kind": kind}));
    let t = &ok(&targets)["targets"][0];
    let added = c.call(
        "source.add",
        json!({"kind": kind, "adapter": t["adapter"], "target": t["target"], "name": name}),
    );
    ok(&added)["source"].as_u64().unwrap()
}

fn sources(c: &mut Client) -> Vec<Value> {
    ok(&c.call("source.list", json!({})))["data"]
        .as_array()
        .unwrap()
        .clone()
}

fn offset_of(c: &mut Client, source: u64) -> i64 {
    let row = sources(c).into_iter().find(|s| s["id"] == source).unwrap();
    row["sync_offset_ms"].as_i64().unwrap_or(0)
}

fn devices(c: &mut Client) -> Vec<Value> {
    ok(&c.call("sync.devices", json!({})))["devices"]
        .as_array()
        .unwrap()
        .clone()
}

fn reason(answer: &Value) -> &Value {
    &answer["error"]["data"]["reason"]
}

/// Adds the audio input `target` and returns its id.
fn add_mic(c: &mut Client, target: &str, name: &str) -> u64 {
    let targets = c.call("source.targets", json!({"kind": "audio_input"}));
    let t = ok(&targets)["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["target"] == target)
        .unwrap()
        .clone();
    let added = c.call(
        "source.add",
        json!({"kind": "audio_input", "adapter": t["adapter"], "target": target, "name": name}),
    );
    ok(&added)["source"].as_u64().unwrap()
}

#[test]
fn a_device_remembers_its_offset_and_new_sources_of_it_take_it() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mono = rbs_audio_io_test::MONO_INPUT;
    let first = add_mic(&mut c, mono, "Mic");
    assert!(devices(&mut c).is_empty());
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": first, "offset_ms": 40}),
    ));
    let listed = devices(&mut c);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["key"]["type"], "org.rbs.source.audio-input");
    assert_eq!(listed[0]["key"]["target"], mono);
    assert_eq!(listed[0]["offset_ms"], 40);
    assert_eq!(listed[0]["origin"], "manual");

    // A second source of the same device starts with its value.
    let second = add_mic(&mut c, mono, "Mic 2");
    assert_eq!(offset_of(&mut c, second), 40);
    // Undoing the add removes the source with its value; nothing else changed.
    ok(&c.call("edit.undo", json!({})));
    assert!(sources(&mut c).iter().all(|s| s["id"] != second));
    let second = add_mic(&mut c, mono, "Mic 2");
    assert_eq!(offset_of(&mut c, second), 40);

    // A committed change updates the device; the other source keeps its value until
    // asked.
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": first, "offset_ms": 60}),
    ));
    assert_eq!(devices(&mut c)[0]["offset_ms"], 60);
    assert_eq!(offset_of(&mut c, second), 40);
    let applied = c.call("sync.apply_to_device_sources", json!({"source": first}));
    assert_eq!(ok(&applied)["changed"], json!([second]));
    assert_eq!(offset_of(&mut c, second), 60);
    let again = c.call("sync.apply_to_device_sources", json!({"source": first}));
    assert_eq!(ok(&again)["changed"], json!([]));

    // A reset removes the entry; forgetting it again finds nothing.
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": first, "offset_ms": 0}),
    ));
    assert!(devices(&mut c).is_empty());
    let key = json!({"type": "org.rbs.source.audio-input", "adapter": "x", "target": mono});
    let gone = c.call("sync.forget_device", json!({"key": key}));
    assert_eq!(kind(&gone), "not_found", "{gone}");
}

#[test]
fn a_retarget_follows_the_device_and_undo_puts_the_value_back() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let (mono, stereo) = (
        rbs_audio_io_test::MONO_INPUT,
        rbs_audio_io_test::STEREO_INPUT,
    );
    let mic = add_mic(&mut c, mono, "Mic");
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": mic, "offset_ms": 30}),
    ));
    let adapter = devices(&mut c)[0]["key"]["adapter"].clone();
    // The value was the old device's: the new device (no entry) gives 0.
    ok(&c.call(
        "source.retarget",
        json!({"source": mic, "adapter": adapter, "target": stereo}),
    ));
    assert_eq!(offset_of(&mut c, mic), 0);
    // The fill is not a committed change: the old device keeps its entry.
    let listed = devices(&mut c);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["key"]["target"], mono);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(offset_of(&mut c, mic), 30);
    ok(&c.call("edit.redo", json!({})));
    assert_eq!(offset_of(&mut c, mic), 0);

    // Every committed value is its device's, so moving back takes the old device's
    // value (ADR-0126 decision 8).
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": mic, "offset_ms": -20}),
    ));
    ok(&c.call(
        "source.retarget",
        json!({"source": mic, "adapter": adapter, "target": mono}),
    ));
    assert_eq!(offset_of(&mut c, mic), 30);
    // Forgetting a device keeps every source's value.
    let key = json!({"type": "org.rbs.source.audio-input", "adapter": adapter, "target": stereo});
    ok(&c.call("sync.forget_device", json!({"key": key})));
    assert_eq!(offset_of(&mut c, mic), 30);
}

#[test]
fn a_measurement_is_refused_with_a_reason() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add_mic(&mut c, rbs_audio_io_test::MONO_INPUT, "Mic");
    let screen = add(&mut c, "screen", "Screen");
    for (params, want_kind, want_reason) in [
        (json!({"source": screen}), "invalid_params", "not_audio"),
        (json!({"source": mic}), "invalid_params", "no_reference"),
        (
            json!({"source": mic, "reference": mic}),
            "invalid_params",
            "not_picture",
        ),
    ] {
        let bad = c.call("sync.measure_start", params.clone());
        assert_eq!(kind(&bad), want_kind, "{params}: {bad}");
        assert_eq!(reason(&bad), want_reason, "{params}: {bad}");
    }
    let bad = c.call("sync.measure_start", json!({"source": 999}));
    assert_eq!(kind(&bad), "not_found", "{bad}");
    let bad = c.call("sync.measure_cancel", json!({"measurement": "1"}));
    assert_eq!(kind(&bad), "not_found", "{bad}");
    let bad = c.call("sync.apply_to_device_sources", json!({"source": screen}));
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
}

#[test]
fn the_measure_window_lists_cameras_and_closes() {
    use rbs_app::studio_host::{Command, SyncMeasurePhase, SyncMeasureRefusal};
    use rbs_scene::model::SourceId;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add_mic(&mut c, rbs_audio_io_test::MONO_INPUT, "Mic");
    let _screen = add(&mut c, "screen", "Screen");
    let cam = add(&mut c, "camera", "Cam");
    let id = |n: u64| SourceId::from_raw(u32::try_from(n).unwrap());

    host.send(Command::OpenSyncMeasure(id(mic)));
    let view = next(&updates, |u| match u {
        Update::SyncMeasure(Some(v)) => Some(v),
        _ => None,
    });
    assert_eq!(view.source, id(mic));
    assert_eq!(view.name, "Mic");
    assert_eq!(view.phase, SyncMeasurePhase::Ready);
    // Only cameras are offered, not the screen.
    assert_eq!(view.references, vec![(id(cam), "Cam".to_owned())]);

    // A source without a picture is refused, and the window says why.
    host.send(Command::StartSyncMeasure(id(mic)));
    let view = next(&updates, |u| match u {
        Update::SyncMeasure(Some(v)) if v.phase != SyncMeasurePhase::Ready => Some(v),
        _ => None,
    });
    assert_eq!(
        view.phase,
        SyncMeasurePhase::Refused(SyncMeasureRefusal::NotPicture)
    );

    host.send(Command::CloseSyncMeasure);
    next(&updates, |u| {
        matches!(u, Update::SyncMeasure(None)).then_some(())
    });
}

#[test]
fn a_source_row_names_the_other_sources_of_its_device() {
    let Some((_host, updates, _api, mut c)) = start() else {
        return;
    };
    let a = add_mic(&mut c, rbs_audio_io_test::MONO_INPUT, "Mic A");
    let b = add_mic(&mut c, rbs_audio_io_test::MONO_INPUT, "Mic B");
    // B takes the device's value; a value of its own makes them differ.
    ok(&c.call(
        "source.set_sync_offset",
        json!({"source": b, "offset_ms": 40}),
    ));
    let peers = next(&updates, |u| match u {
        Update::Sources(rows) => rows
            .iter()
            .find(|r| u64::from(r.id.get()) == a && !r.device_peers.is_empty())
            .map(|r| r.device_peers.clone()),
        _ => None,
    });
    assert_eq!(peers, vec!["Mic B".to_owned()]);
}

/// The `media.*` methods (protocol 1.83, ADR-0108 decision 10) refuse an unknown source
/// and a source of another type with typed errors.
#[test]
fn media_commands_refuse_an_unknown_source_and_another_type() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add_mic(&mut c, rbs_audio_io_test::MONO_INPUT, "Mic");
    for method in ["media.play", "media.pause", "media.restart", "media.state"] {
        assert_eq!(kind(&c.call(method, json!({"source": 9_999}))), "not_found");
        assert_eq!(
            kind(&c.call(method, json!({"source": mic}))),
            "invalid_params",
            "{method}"
        );
    }
    assert_eq!(
        kind(&c.call("media.seek", json!({"source": mic, "position_ms": 10}))),
        "invalid_params"
    );
}
