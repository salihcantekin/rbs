//! Sending to the virtual camera through the studio host (ADR-0066 decision 12) with
//! the reference camera adapter: start and stop, a refused start, and sending on after
//! a canvas change restarts the studio. The engine runs on a `TestClock` that never
//! moves; the test waits for the host's updates, with a bound only as a hang guard
//! (ADR-0032). Needs a GPU adapter (software is fine); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_app::studio_host::{CameraFactory, CameraStatus, Command, HostConfig, StudioHost, Update};
use rbs_core::media::Size;
use rbs_core::time::TestClock;
use rbs_core::timing::FrameRate;
use rbs_engine::{Engine, EngineConfig};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_record::RecordAdapters;
use rbs_vcam::VirtualCameraFactory;
use rbs_vcam_test::TestVcamFactory;

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

fn start(host: &StudioHost) {
    host.send(Command::StartVirtualCamera {
        width: 640,
        height: 360,
        fps: FrameRate::new(15, 1).unwrap(),
    });
}

#[test]
fn the_camera_sends_until_stopped_and_across_a_canvas_change() {
    let Some(gpu) = gpu() else { return };
    let factory = Arc::new(TestVcamFactory::new());
    factory.install().unwrap();
    let (host, rx) = studio(gpu, Some(Arc::clone(&factory)));
    start(&host);
    let s = camera(&rx).expect("sending");
    assert_eq!((s.width, s.height, s.rate), (640, 360, 15));
    assert!(
        s.counters.is_some(),
        "the reference adapter shares its counters"
    );
    assert_eq!(s.failure, None);

    host.send(Command::SetCanvas {
        size: Size::new(1280, 720),
        fps: FrameRate::FPS_30,
    });
    next(&rx, |u| match u {
        Update::Canvas { .. } => Some(()),
        Update::Error(e) | Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    // Still sending, at the camera's own size, with no error on the way.
    host.send(Command::StopVirtualCamera);
    next(&rx, |u| match u {
        Update::Error(e) => panic!("{e}"),
        Update::VirtualCamera(None) => Some(()),
        _ => None,
    });
    // Stopping again is harmless.
    host.send(Command::StopVirtualCamera);
    assert_eq!(camera(&rx), None);
}

#[test]
fn a_start_is_refused_without_the_camera() {
    let Some(gpu) = gpu() else { return };
    // Not added.
    let (host, rx) = studio(Arc::clone(&gpu), Some(Arc::new(TestVcamFactory::new())));
    start(&host);
    let error = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(
        error.starts_with("The virtual camera did not start"),
        "{error}"
    );
    assert_eq!(camera(&rx), None);
    drop(host);
    // No adapter on this platform.
    let (host, rx) = studio(gpu, None);
    start(&host);
    let error = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert_eq!(
        error,
        "The virtual camera is not available on this computer."
    );
    assert_eq!(camera(&rx), None);
}
