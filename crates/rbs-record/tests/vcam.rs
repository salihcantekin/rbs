//! The studio's virtual camera (ADR-0066 decision 6; spec `recording.md`, "Virtual
//! camera"): it sends the program alongside recordings without touching them, holds
//! nothing while not sending, and its session is dropped off the render thread.
//! Reference adapters on a `TestClock`; tests wait for counts, never for a duration.
//! One test only: it counts the camera's threads in this process.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::ContainerId;
use rbs_core::timing::FrameRate;
use rbs_record::{
    OutputSettings, RENDER, Studio, StudioSettings, VirtualCameraSettings, VirtualCameraState,
};
use rbs_vcam::VirtualCameraFactory;
use rbs_vcam::conformance::Probe as _;
use rbs_vcam_test::TestVcamFactory;
use support::*;

fn sent(state: &Option<VirtualCameraState>) -> u64 {
    state
        .as_ref()
        .map_or(0, |s| s.stats.sent.load(Ordering::Relaxed))
}

/// Threads of the test camera's sessions in this process (Linux lists them by name).
fn camera_threads() -> Option<usize> {
    let tasks = std::fs::read_dir("/proc/self/task").ok()?;
    Some(
        tasks
            .filter_map(Result::ok)
            .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
            .filter(|name| name.trim() == "rbs-vcam-test")
            .count(),
    )
}

#[test]
fn the_camera_sends_the_program_alongside_a_recording_and_stops_cleanly() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let settings = StudioSettings::new(canvas);
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let cam = TestVcamFactory::new();

    // A camera that is not installed fails the start and leaves nothing.
    let e = studio
        .start_virtual_camera(&cam, VirtualCameraSettings::new(32, 18, FrameRate::FPS_30))
        .unwrap_err();
    assert!(matches!(e, rbs_vcam::VcamError::NotInstalled), "{e:?}");
    cam.install().unwrap();

    // Not sending: no node, no render worker, no camera thread.
    assert!(studio.virtual_camera().is_none());
    assert!(!studio.engine().has_worker(RENDER));
    assert_eq!(camera_threads().unwrap_or(0), 0);

    let cam_settings = VirtualCameraSettings::new(32, 18, FrameRate::FPS_30);
    studio.start_virtual_camera(&cam, cam_settings).unwrap();
    assert!(
        studio.start_virtual_camera(&cam, cam_settings).is_err(),
        "a second start while sending"
    );
    // The camera is not an output, and it schedules the program on its own.
    assert!(studio.outputs().is_empty());
    assert!(studio.engine().has_worker(RENDER));
    assert!(run_until(&mut studio, &clock, |s| sent(
        &s.virtual_camera()
    ) >= 3));
    let state = studio.virtual_camera().unwrap();
    assert_eq!(state.config.format.width, 32);
    assert_eq!(state.failure, None);

    // A recording starts and stops while the camera sends; the camera keeps sending.
    let out = OutputSettings::new("mem://with-camera", ContainerId::Matroska);
    let id = studio.start_output(&r.adapters, &out).unwrap();
    let before = r.log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 4));
    let report = studio.stop_output(id).unwrap();
    assert_eq!(report.failure, None);
    let at = sent(&studio.virtual_camera());
    assert!(run_until(&mut studio, &clock, |s| sent(
        &s.virtual_camera()
    ) > at));

    // Frames reached the camera's consumer side in order.
    let delivered = cam.consumer().delivered();
    assert!(!delivered.is_empty());
    assert!(delivered.windows(2).all(|w| w[0] < w[1]), "{delivered:?}");

    // Stopping removes the node; the session (and its thread) is dropped on this thread
    // once the render partition hands the node back.
    assert!(studio.stop_virtual_camera());
    assert!(!studio.stop_virtual_camera());
    assert!(studio.virtual_camera().is_none());
    assert!(run_until(&mut studio, &clock, |s| {
        !s.engine().has_worker(RENDER) && camera_threads().unwrap_or(0) == 0
    }));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
