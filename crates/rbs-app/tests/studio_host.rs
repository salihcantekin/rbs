//! The studio host on the reference adapters: the add-source flow, delete and undo, and
//! shutdown, without a window. The engine runs on a `TestClock` that never moves, so
//! sources open and the scene runs without any tick timing; the test waits for the
//! host's updates, with a bound only as a hang guard (ADR-0032). Needs a GPU adapter
//! (software is fine); skipped without one unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_app::studio_host::{
    Command, HostConfig, SourceKind, SourceRow, SourceStatus, StudioHost, Target, Update,
};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
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
    let (tx, rx) = mpsc::channel();
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
        || Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default()),
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
        Update::Deleted(n) => Some(n),
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
