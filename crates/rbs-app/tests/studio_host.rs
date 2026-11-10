//! The studio host on the reference adapters: the add-source flow, delete and undo, and
//! shutdown, without a window. The engine runs on a `TestClock` that never moves, so
//! sources open and the scene runs without any tick timing; the test waits for the
//! host's updates, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_app::edit_history::{EditKind, StepInfo};
use rbs_app::studio_host::{
    Command, Fault, HostConfig, SourceKind, SourceRow, SourceStatus, StudioHost, Target, Update,
};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;

/// Hang guard only: every wait ends as soon as its update arrives.
const LIMIT: Duration = Duration::from_secs(20);

fn gpu() -> Option<Arc<GpuContext>> {
    // One device per test binary: software Vulkan drivers crash with several.
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
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

fn host(gpu: Arc<GpuContext>) -> (StudioHost, Receiver<Update>) {
    let mut config = HostConfig::desktop();
    config.preview = None;
    host_with(gpu, config)
}

fn host_with(gpu: Arc<GpuContext>, config: HostConfig) -> (StudioHost, Receiver<Update>) {
    let (tx, rx) = mpsc::channel();
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

fn rows_where(rx: &Receiver<Update>, mut ok: impl FnMut(&[SourceRow]) -> bool) -> Vec<SourceRow> {
    next(rx, |u| match u {
        Update::Sources(rows) if ok(&rows) => Some(rows),
        _ => None,
    })
}

fn targets(host: &StudioHost, rx: &Receiver<Update>, kind: SourceKind) -> Vec<Target> {
    host.send(Command::ListTargets(kind));
    next(rx, |u| match u {
        Update::Targets(k, t) if k == kind => Some(t),
        _ => None,
    })
}

#[test]
fn sources_are_added_deleted_and_restored_through_the_host() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    let kinds = next(&rx, |u| match u {
        Update::Started { kinds, preview, .. } => {
            assert!(preview.is_none());
            Some(kinds)
        }
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    for k in [
        SourceKind::Screen,
        SourceKind::Window,
        SourceKind::Camera,
        SourceKind::AudioInput,
        SourceKind::SystemAudio,
    ] {
        assert!(kinds.contains(&k), "{k:?} in {kinds:?}");
    }

    // A display: without a preview or a recording nothing shows it, so it stays closed.
    let displays = targets(&host, &rx, SourceKind::Screen);
    assert!(!displays.is_empty());
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: displays[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 1);
    assert_eq!(rows[0].kind, Some(SourceKind::Screen));
    assert_eq!(rows[0].status, SourceStatus::Idle);

    // The default microphone plays for the meters, on top of the list.
    let inputs = targets(&host, &rx, SourceKind::AudioInput);
    let mic = inputs.iter().find(|t| t.is_default).unwrap_or(&inputs[0]);
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: mic.clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 2);
    assert_eq!(rows[0].kind, Some(SourceKind::AudioInput));
    assert_eq!(rows[0].status, SourceStatus::Running, "{rows:?}");
    assert!(rows[0].meter.is_some());
    let mic_id = rows[0].id;

    // The same target again gets a unique name.
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: mic.clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 3);
    assert_eq!(rows[0].name, format!("{} 2", rows[1].name));

    // Delete, then undo.
    host.send(Command::DeleteSource(mic_id));
    let name = next(&rx, |u| match u {
        Update::Deleted { name: n, .. } => Some(n),
        _ => None,
    });
    assert_eq!(name, rows[1].name);
    host.send(Command::UndoDelete);
    let rows = rows_where(&rx, |r| r.iter().any(|s| s.id == mic_id));
    assert_eq!(rows.len(), 3);

    // Dropping the host stops the studio and joins its thread.
    drop(host);
    loop {
        match rx.recv_timeout(LIMIT) {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e, RecvTimeoutError::Disconnected, "the thread ended");
                break;
            }
        }
    }
}

#[test]
fn sources_change_layer_order_and_box_through_the_host() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let displays = targets(&host, &rx, SourceKind::Screen);
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: displays[0].clone(),
    });
    rows_where(&rx, |r| r.len() == 1);
    let cameras = targets(&host, &rx, SourceKind::Camera);
    host.send(Command::AddSource {
        kind: SourceKind::Camera,
        target: cameras[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 2);
    // Top first: the camera was added last, so it is over the screen.
    assert_eq!(rows[0].kind, Some(SourceKind::Camera));
    let (camera, screen) = (rows[0].id, rows[1].id);
    let camera_box = rows[0].layout.expect("a camera has a picture");

    // Already the top layer: nothing to do, so no update; the down move answers.
    host.send(Command::MoveSource {
        id: camera,
        up: true,
    });
    host.send(Command::MoveSource {
        id: camera,
        up: false,
    });
    let rows = rows_where(&rx, |r| r.len() == 2 && r[0].id == screen);
    assert_eq!(rows[1].id, camera);
    host.send(Command::MoveSource {
        id: camera,
        up: true,
    });
    rows_where(&rx, |r| r.len() == 2 && r[0].id == camera);

    // A new box keeps the rest of the layout.
    host.send(Command::SetSourceBox {
        gesture: 0,
        seq: 1,
        id: camera,
        position: [10.0, 20.0],
        size: [640.0, 360.0],
    });
    let rows = rows_where(&rx, |r| {
        r[0].layout.is_some_and(|l| l.position == [10.0, 20.0])
    });
    let placed_answer = |rx: &Receiver<Update>, want: u64| {
        next(rx, |u| match u {
            Update::BoxPlaced { seq, ok } if seq == want => Some(ok),
            Update::BoxPlaced { seq, .. } => panic!("answer {seq}, expected {want}"),
            _ => None,
        })
    };
    assert!(placed_answer(&rx, 1));
    // The same box again changes nothing and is still answered.
    host.send(Command::SetSourceBox {
        gesture: 0,
        seq: 2,
        id: camera,
        position: [10.0, 20.0],
        size: [640.0, 360.0],
    });
    assert!(placed_answer(&rx, 2));
    let placed = rows[0].layout.unwrap();
    assert_eq!(placed.size, [640.0, 360.0]);
    assert_eq!(placed.rotation, camera_box.rotation);
    assert_eq!(placed.opacity, camera_box.opacity);

    // A sound-only source between them does not count: one move takes the camera
    // under the screen.
    let inputs = targets(&host, &rx, SourceKind::AudioInput);
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: inputs[0].clone(),
    });
    rows_where(&rx, |r| r.len() == 3);
    host.send(Command::MoveSource {
        id: camera,
        up: false,
    });
    let rows = rows_where(&rx, |r| r.len() == 3 && r[2].id == camera);
    assert_eq!(rows[1].id, screen, "{rows:?}");
    assert!(rows[0].layout.is_none(), "audio has no box");

    // A zero size is refused and changes nothing.
    host.send(Command::SetSourceBox {
        gesture: 0,
        seq: 3,
        id: camera,
        position: [0.0, 0.0],
        size: [0.0, 360.0],
    });
    next(&rx, |u| match u {
        Update::Error(_) => Some(()),
        Update::Sources(r) => panic!("refused box changed the rows: {r:?}"),
        _ => None,
    });
    assert!(!placed_answer(&rx, 3));
}

fn history(rx: &Receiver<Update>) -> (Option<StepInfo>, Option<StepInfo>) {
    next(rx, |u| match u {
        Update::History { undo, redo } => Some((undo, redo)),
        _ => None,
    })
}

#[test]
fn edits_are_undone_and_redone_and_a_drag_is_one_step() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let displays = targets(&host, &rx, SourceKind::Screen);
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: displays[0].clone(),
    });
    rows_where(&rx, |r| r.len() == 1);
    let cameras = targets(&host, &rx, SourceKind::Camera);
    host.send(Command::AddSource {
        kind: SourceKind::Camera,
        target: cameras[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 2);
    let (camera, screen) = (rows[0].id, rows[1].id);
    let start = rows[0].layout.unwrap();
    let (undo, redo) = history(&rx);
    assert_eq!(undo.as_ref().map(|s| s.kind), Some(EditKind::Add));
    assert_eq!(undo.unwrap().name, rows[0].name);
    assert!(redo.is_none());

    // A drag: three boxes of one gesture, then one of its own from the fields.
    for (seq, x) in [(1, 10.0), (2, 20.0), (3, 30.0)] {
        host.send(Command::SetSourceBox {
            seq,
            gesture: 5,
            id: camera,
            position: [x, 0.0],
            size: start.size,
        });
    }
    host.send(Command::SetSourceBox {
        seq: 4,
        gesture: 0,
        id: camera,
        position: [30.0, 0.0],
        size: [320.0, 180.0],
    });
    rows_where(&rx, |r| {
        r[0].layout.is_some_and(|l| l.size == [320.0, 180.0])
    });
    host.send(Command::MoveSource {
        id: camera,
        up: false,
    });
    rows_where(&rx, |r| r[0].id == screen);

    // Undo the move, the resize, then the whole drag in one step.
    host.send(Command::Undo);
    rows_where(&rx, |r| r[0].id == camera);
    host.send(Command::Undo);
    let rows = rows_where(&rx, |r| r[0].layout.is_some_and(|l| l.size == start.size));
    assert_eq!(rows[0].layout.unwrap().position, [30.0, 0.0]);
    host.send(Command::Undo);
    let rows = rows_where(&rx, |r| {
        r[0].layout.is_some_and(|l| l.position == start.position)
    });
    assert_eq!(rows[0].layout.unwrap(), start);
    let (undo, redo) = history(&rx);
    assert_eq!(undo.map(|s| s.kind), Some(EditKind::Add));
    assert_eq!(redo.map(|s| s.kind), Some(EditKind::Place));

    // Redo the drag, then a new edit clears what was left to redo.
    host.send(Command::Redo);
    rows_where(&rx, |r| {
        r[0].layout.is_some_and(|l| l.position == [30.0, 0.0])
    });
    host.send(Command::DeleteSource(screen));
    rows_where(&rx, |r| r.len() == 1);
    let (undo, redo) = history(&rx);
    assert_eq!(undo.map(|s| s.kind), Some(EditKind::Delete));
    assert!(redo.is_none());
    // The toast's Undo brings the screen back; after another edit it does nothing.
    host.send(Command::UndoDelete);
    rows_where(&rx, |r| r.len() == 2);
    host.send(Command::Undo);
    rows_where(&rx, |r| {
        r.len() == 2 && r[0].layout.is_some_and(|l| l.position == start.position)
    });
    host.send(Command::UndoDelete);
    // Undo everything: the adds go too.
    host.send(Command::Undo);
    rows_where(&rx, |r| r.len() == 1);
    host.send(Command::Undo);
    rows_where(&rx, |r| r.is_empty());
    let (undo, redo) = history(&rx);
    assert!(undo.is_none());
    assert_eq!(redo.map(|s| s.kind), Some(EditKind::Add));
    // Nothing to undo: nothing happens.
    host.send(Command::Undo);
    host.send(Command::Redo);
    let rows = rows_where(&rx, |r| r.len() == 1);
    assert_eq!(
        rows[0].id, screen,
        "the same source comes back under its old id"
    );
}

#[test]
fn a_new_canvas_restarts_the_studio_with_the_sources() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    let size = next(&rx, |u| match u {
        Update::Started { size, fps, .. } => Some((size, fps)),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(size, (Size::new(1920, 1080), FrameRate::FPS_30));
    let inputs = targets(&host, &rx, SourceKind::AudioInput);
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: inputs[0].clone(),
    });
    let rows = rows_where(&rx, |r| {
        r.len() == 1 && r[0].status == SourceStatus::Running
    });
    let id = rows[0].id;

    host.send(Command::SetCanvas {
        size: Size::new(1280, 720),
        fps: FrameRate::FPS_60,
    });
    let (preview, size, fps) = next(&rx, |u| match u {
        Update::Canvas { preview, size, fps } => Some((preview, size, fps)),
        Update::Error(e) | Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    assert!(preview.is_none(), "this host has no preview");
    assert_eq!((size, fps), (Size::new(1280, 720), FrameRate::FPS_60));
    // The new studio runs the same source.
    let rows = rows_where(&rx, |r| r.len() == 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].status, SourceStatus::Running, "{rows:?}");
    assert!(rows[0].meter.is_some());

    // The same canvas again changes nothing and still answers.
    host.send(Command::SetCanvas {
        size: Size::new(1280, 720),
        fps: FrameRate::FPS_60,
    });
    let again = next(&rx, |u| match u {
        Update::Canvas { size, .. } => Some(size),
        Update::Error(e) | Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(again, Size::new(1280, 720));
}

#[test]
fn a_canvas_change_that_panics_while_restarting_fails_the_studio() {
    let Some(gpu) = gpu() else { return };
    let mut config = HostConfig::desktop();
    config.preview = None;
    config.fault = Some(Fault::DuringCanvasRestart);
    let (host, rx) = host_with(gpu, config);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::SetCanvas {
        size: Size::new(1280, 720),
        fps: FrameRate::FPS_60,
    });
    let why = next(&rx, |u| match u {
        Update::Failed(why) => Some(why),
        _ => None,
    });
    assert!(
        why.contains("restarted"),
        "the studio should fail in the same round: {why}"
    );
}

/// Lists no screens and picks them with an OS picker, like the Linux portal: the first
/// pick chooses the reference display, the second is cancelled, later ones fail.
struct Picking {
    inner: TestCaptureFactory,
    picks: std::sync::atomic::AtomicU32,
}

impl rbs_core::registry::AdapterFactory for Picking {
    fn id(&self) -> rbs_core::registry::AdapterId {
        rbs_core::registry::AdapterId::new("org.example.picking")
    }
    fn display_name(&self) -> &str {
        "Picking"
    }
    fn probe(&self) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
        let mut caps = self.inner.probe()?;
        caps.insert(
            rbs_capture::caps::TARGET_PICKER,
            rbs_core::capability::CapabilityValue::Set(vec!["display".to_owned()]),
        );
        Ok(caps)
    }
}

impl rbs_capture::CaptureFactory for Picking {
    fn enumerate(
        &self,
        kind: rbs_capture::TargetKind,
    ) -> Result<Vec<rbs_capture::CaptureTarget>, rbs_capture::CaptureError> {
        match kind {
            rbs_capture::TargetKind::Display => {
                Err(rbs_capture::CaptureError::Unsupported("picker only"))
            }
            k => self.inner.enumerate(k),
        }
    }
    fn open(
        &self,
        request: &rbs_capture::CaptureRequest,
    ) -> Result<Box<dyn rbs_capture::CaptureSession>, rbs_capture::CaptureError> {
        self.inner.open(request)
    }
    fn pick(
        &self,
        kind: rbs_capture::TargetKind,
    ) -> Result<rbs_capture::CaptureTarget, rbs_capture::CaptureError> {
        match self
            .picks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        {
            0 => {
                let mut t = self.inner.enumerate(kind)?.remove(0);
                t.name = "Picked display".into();
                Ok(t)
            }
            1 => Err(rbs_capture::CaptureError::Cancelled),
            _ => Err(rbs_capture::CaptureError::PermissionDenied),
        }
    }
}

#[test]
fn screens_only_an_os_picker_offers_are_added_through_it() {
    let Some(gpu) = gpu() else { return };
    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(Picking {
                    inner: TestCaptureFactory::new(),
                    picks: Default::default(),
                }))
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
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let screens = targets(&host, &rx, SourceKind::Screen);
    assert_eq!(screens.len(), 1, "{screens:?}");
    assert!(screens[0].picker);
    assert_eq!(screens[0].name, "Choose a screen…");
    assert_eq!(screens[0].adapter, "org.example.picking");
    assert!(
        targets(&host, &rx, SourceKind::Camera)
            .iter()
            .all(|t| !t.picker)
    );

    // The choice is added like a listed screen, with the adapter that picked it.
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: screens[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 1);
    assert_eq!(rows[0].kind, Some(SourceKind::Screen));
    assert_eq!(rows[0].name, "Picked display");

    // A closed dialog adds nothing and says nothing: the next row is the microphone.
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: screens[0].clone(),
    });
    let inputs = targets(&host, &rx, SourceKind::AudioInput);
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: inputs[0].clone(),
    });
    let rows = next(&rx, |u| match u {
        Update::Error(e) => panic!("{e}"),
        Update::Sources(rows) if rows.len() >= 2 => Some(rows),
        _ => None,
    });
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, Some(SourceKind::AudioInput));

    // A refusal is shown.
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: screens[0].clone(),
    });
    let error = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(
        error.starts_with("The screen could not be chosen"),
        "{error}"
    );
}

/// Ends the host and waits for its thread.
fn stop(host: StudioHost, rx: &Receiver<Update>) {
    drop(host);
    while rx.recv_timeout(LIMIT).is_ok() {}
}

#[test]
fn the_scene_collection_is_saved_after_edits_and_runs_again_after_a_restart() {
    let Some(gpu) = gpu() else { return };
    let d = std::env::temp_dir().join(format!("rbs-app-host-collection-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (user, collection) = rbs_app::user_session::open(&d, Arc::new(|_| {}));
    let start = |user: &rbs_app::user_session::UserSession, collection| {
        let mut config = HostConfig::canvas(Size::new(1280, 720), FrameRate::FPS_30);
        config.preview = None;
        config.collection = Some(collection);
        config.autosave = user.collection_saver();
        host_with(Arc::clone(&gpu), config)
    };
    let (host, rx) = start(&user, collection);
    // The collection's canvas, not the configured size.
    let size = next(&rx, |u| match u {
        Update::Started { size, .. } => Some(size),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(size, Size::new(1920, 1080));
    let displays = targets(&host, &rx, SourceKind::Screen);
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: displays[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 1);
    stop(host, &rx);
    user.flush();
    drop(user);

    // A new app: the same collection, with its source, runs again.
    let (user, collection) = rbs_app::user_session::open(&d, Arc::new(|_| {}));
    assert_eq!(collection.sources().count(), 1);
    let (host, rx) = start(&user, collection);
    let again = rows_where(&rx, |r| r.len() == 1);
    assert_eq!((again[0].id, &again[0].name), (rows[0].id, &rows[0].name));
    stop(host, &rx);
    drop(user);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn another_scene_collection_runs_and_keeps_its_own_edits() {
    let Some(gpu) = gpu() else { return };
    let d = std::env::temp_dir().join(format!("rbs-app-host-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let (user, collection) = rbs_app::user_session::open(&d, Arc::new(|_| {}));
    let first = user.collection_id.clone();
    let mut config = HostConfig::canvas(Size::new(1920, 1080), FrameRate::FPS_30);
    config.preview = None;
    config.collection = Some(collection);
    config.autosave = user.collection_saver();
    let (host, rx) = host_with(Arc::clone(&gpu), config);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });

    let small = rbs_app::canvas_profile::CanvasProfile {
        width: 1280,
        height: 720,
        ..rbs_app::canvas_profile::CanvasProfile::new()
    };
    let second = user.new_collection("Second", &small, false).unwrap();
    let (name, loaded) = user.load_collection(&second).unwrap();
    host.send(Command::SwitchCollection {
        seq: 7,
        collection: Box::new(loaded),
        autosave: user.saver_for(&second, &name),
    });
    let size = next(&rx, |u| match u {
        Update::Canvas { size, .. } => Some(size),
        _ => None,
    });
    assert_eq!(size, Size::new(1280, 720), "its own canvas runs");
    let result = next(&rx, |u| match u {
        Update::CollectionSwitched { seq, result } => Some((seq, result)),
        _ => None,
    });
    assert_eq!(result, (7, Ok(())));

    let displays = targets(&host, &rx, SourceKind::Screen);
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: displays[0].clone(),
    });
    rows_where(&rx, |r| r.len() == 1);
    stop(host, &rx);
    // The edit went into the second collection only.
    assert_eq!(
        user.load_collection(&second).unwrap().1.sources().count(),
        1
    );
    assert_eq!(user.load_collection(&first).unwrap().1.sources().count(), 0);
    drop(user);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_source_is_pointed_at_another_target_keeping_its_place_and_one_undo_step() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let mics = targets(&host, &rx, SourceKind::AudioInput);
    assert!(mics.len() >= 2, "the reference adapter lists two mics");
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: mics[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 1);
    let (id, name, item, layout) = (
        rows[0].id,
        rows[0].name.clone(),
        rows[0].item,
        rows[0].layout,
    );

    host.send(Command::RetargetSource {
        id,
        target: mics[1].clone(),
    });
    // The rows and the history arrive in either order; wait for the history step.
    let mut last_rows = None;
    let (undo, _) = next(&rx, |u| match u {
        Update::Sources(rows) => {
            last_rows = Some(rows);
            None
        }
        Update::History { undo, redo }
            if undo.as_ref().is_some_and(|s| s.kind == EditKind::Retarget) =>
        {
            Some((undo, redo))
        }
        _ => None,
    });
    assert_eq!(undo.unwrap().name, name);
    // Same source, same item and layout: only the target changed.
    if let Some(rows) = last_rows {
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].id, &rows[0].name, rows[0].item, rows[0].layout),
            (id, &name, item, layout)
        );
    }

    // The same target again records nothing.
    host.send(Command::RetargetSource {
        id,
        target: mics[1].clone(),
    });
    // A healthy source is left running: reload finds no failure record.
    host.send(Command::ReloadSource(id));
    host.send(Command::Undo);
    let (undo, redo) = history(&rx);
    assert_eq!(
        undo.map(|s| s.kind),
        Some(EditKind::Add),
        "one undo step undid the retarget"
    );
    assert_eq!(redo.map(|s| s.kind), Some(EditKind::Retarget));
}

#[test]
fn targets_a_source_was_given_are_listed_as_recent_newest_first() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let mics = targets(&host, &rx, SourceKind::AudioInput);
    assert!(
        mics.iter().all(|t| t.recent.is_none()),
        "nothing picked yet"
    );
    host.send(Command::AddSource {
        kind: SourceKind::AudioInput,
        target: mics[0].clone(),
    });
    let rows = rows_where(&rx, |r| r.len() == 1);
    host.send(Command::RetargetSource {
        id: rows[0].id,
        target: mics[1].clone(),
    });
    // Commands run in order, so the list below sees both picks.
    let after = targets(&host, &rx, SourceKind::AudioInput);
    let rank = |t: &Target| {
        after
            .iter()
            .find(|a| a.target == t.target)
            .and_then(|a| a.recent)
    };
    assert_eq!(rank(&mics[1]), Some(0), "the last pick is the newest");
    assert_eq!(rank(&mics[0]), Some(1));
    stop(host, &rx);
}

#[test]
fn thumbnails_are_grabbed_off_the_control_thread_and_only_for_the_asked_rows() {
    let Some(gpu) = gpu() else { return };
    let (host, rx) = host(gpu);
    let screens = targets(&host, &rx, SourceKind::Screen);
    let screen = screens.first().expect("the test display");
    host.send(Command::GrabThumbnails {
        kind: SourceKind::Screen,
        rows: vec![
            (screen.adapter.clone(), screen.target.clone()),
            (screen.adapter.clone(), "no-such-target".to_owned()),
        ],
        max: (96, 54),
    });
    let (mut got, mut missing) = (false, false);
    while !(got && missing) {
        next(&rx, |u| match u {
            Update::Thumbnail {
                target, picture, ..
            } => {
                match (target == screen.target, picture) {
                    (true, Some(p)) => {
                        assert!(p.width() <= 96 && p.height() <= 54);
                        got = true;
                    }
                    (false, None) => missing = true,
                    other => panic!("unexpected picture {other:?}"),
                }
                Some(())
            }
            _ => None,
        });
    }
    host.send(Command::StopThumbnails);
}
