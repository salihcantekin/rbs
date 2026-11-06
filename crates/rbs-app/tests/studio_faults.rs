//! The studio host survives a panic on its control thread (ADR-0054 decision 3): the
//! command fails with the caught-panic error and the next command runs. Uses the
//! reference adapters and a `TestClock`; waits only for updates, with a bound as a
//! hang guard (ADR-0032). Needs a GPU adapter (software is fine); skipped without one
//! unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_app::studio_host::{Command, HostConfig, SourceKind, StudioHost, Target, Update};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureTarget};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::capability::CapabilitySet;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::TestClock;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;

/// Hang guard only: every wait ends as soon as its update arrives.
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
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

/// The reference capture adapter, whose `enumerate` panics while `armed`.
struct Faulty {
    inner: TestCaptureFactory,
    armed: Arc<AtomicBool>,
}

impl AdapterFactory for Faulty {
    fn id(&self) -> AdapterId {
        self.inner.id()
    }
    fn display_name(&self) -> &str {
        self.inner.display_name()
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.inner.probe()
    }
}

impl CaptureFactory for Faulty {
    fn enumerate(&self, kind: rbs_capture::TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        assert!(!self.armed.load(Ordering::Relaxed), "injected fault");
        self.inner.enumerate(kind)
    }
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        self.inner.open(request)
    }
}

fn next<T>(rx: &Receiver<Update>, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    loop {
        let u = rx.recv_timeout(LIMIT).expect("no update in time");
        if let Some(t) = pick(u) {
            return t;
        }
    }
}

fn targets(host: &StudioHost, rx: &Receiver<Update>, kind: SourceKind) -> Vec<Target> {
    host.send(Command::ListTargets(kind));
    next(rx, |u| match u {
        Update::Targets(k, t) if k == kind => Some(t),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    })
}

#[test]
fn a_panicking_command_fails_alone_and_the_studio_goes_on() {
    let Some(gpu) = gpu() else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let mut config = HostConfig::desktop();
    config.preview = None;
    let host = StudioHost::spawn(
        config,
        {
            let armed = Arc::clone(&armed);
            move || {
                let mut a = RecordAdapters::new();
                a.sources
                    .register_capture(Arc::new(Faulty {
                        inner: TestCaptureFactory::new(),
                        armed,
                    }))
                    .unwrap();
                a.sources
                    .register_audio(Arc::new(TestAudioBackend::new()))
                    .unwrap();
                (a, Vec::new())
            }
        },
        move || Ok(gpu),
        || Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default()),
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

    armed.store(true, Ordering::Relaxed);
    host.send(Command::ListTargets(SourceKind::Camera));
    let error = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        Update::Failed(e) => panic!("the studio failed: {e}"),
        Update::Targets(..) => panic!("listed while the adapter panics"),
        _ => None,
    });
    assert_eq!(error, rbs_crash::CAUGHT_TEXT);

    // The control thread is alive and the studio still runs: a camera is added.
    armed.store(false, Ordering::Relaxed);
    let cameras = targets(&host, &rx, SourceKind::Camera);
    host.send(Command::AddSource {
        kind: SourceKind::Camera,
        target: cameras[0].clone(),
    });
    let rows = next(&rx, |u| match u {
        Update::Error(e) | Update::Failed(e) => panic!("{e}"),
        Update::Sources(rows) if !rows.is_empty() => Some(rows),
        _ => None,
    });
    assert_eq!(rows[0].kind, Some(SourceKind::Camera));
    drop(host);
    while rx.recv_timeout(LIMIT).is_ok() {}
}
