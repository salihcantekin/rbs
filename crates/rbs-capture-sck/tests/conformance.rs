//! Runs the capture port conformance suite against the ScreenCaptureKit adapter.
//!
//! Without screen recording permission (CI runners) the suite checks probing, the pick
//! and unknown-target rules; enumeration and sessions are skipped. With permission but
//! no GPU import target it also checks enumeration and that GPU memory is refused
//! without a matching import target. On other platforms the probe fails and the suite
//! reports everything skipped.
//!
//! With `RBS_CAPTURE_HW=1` on a Mac whose terminal has screen recording permission,
//! `hardware_session_passes_conformance` also opens real sessions. ScreenCaptureKit
//! only delivers frames when content changes, so keep something moving on screen.

use rbs_capture::conformance::{self, Options, RealTimePacer};
use rbs_capture_sck::SckCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[test]
fn passes_conformance_without_gpu() {
    let factory = SckCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            if cfg!(target_os = "macos") && report.passed.iter().any(|c| c == "caps") {
                for check in ["enumerate", "pick", "unknown target"] {
                    assert!(
                        report.passed.iter().any(|c| c == check),
                        "{check} did not run: {report:?}"
                    );
                }
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn hardware_session_passes_conformance() {
    use rbs_capture::{GpuDeviceId, GpuImportTarget, SyncKind};

    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Mac with screen recording permission");
        return;
    }
    // IOSurfaces can be imported by any Metal device, so the registry id is not checked.
    let gpu = GpuImportTarget::new(GpuDeviceId::MetalRegistryId(0), &[SyncKind::Ready]);
    let factory = SckCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        gpu: Some(&gpu),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            for check in ["formats", "standby", "timestamps", "latest wins", "stop"] {
                assert!(
                    report.passed.iter().any(|c| c == check),
                    "{check} did not run: {report:?}"
                );
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
}

/// The adapter's soundness rests on ScreenCaptureKit not rendering into an IOSurface
/// while the adapter retains its pixel buffer for a held frame. This checks it on real
/// hardware: hold one frame for a second while the screen changes, and require that its
/// surface is never delivered again and its seed (bumped on every write) stays put.
#[cfg(target_os = "macos")]
#[test]
fn held_surface_is_not_rewritten() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use objc2_io_surface::IOSurfaceRef;
    use rbs_capture::TargetKind;
    use rbs_capture::{CaptureFactory, CaptureRequest, GpuDeviceId, GpuImportTarget, SyncKind};
    use rbs_core::media::{FramePayload, MemoryKind, NativeSurface, PixelFormat};
    use rbs_core::time::MonotonicClock;

    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Mac with screen recording permission");
        return;
    }
    let factory = SckCaptureFactory::new();
    let targets = match factory.enumerate(TargetKind::Display) {
        Ok(t) => t,
        Err(e) => panic!("enumerate: {e}"),
    };
    let Some(target) = targets.first() else {
        panic!("no display")
    };
    let gpu = GpuImportTarget::new(GpuDeviceId::MetalRegistryId(0), &[SyncKind::Ready]);
    let request = CaptureRequest::new(target.id.clone(), Arc::new(MonotonicClock::new()))
        .with_pixel_formats(&[PixelFormat::Bgra8])
        .with_memory(&[MemoryKind::Metal])
        .with_gpu(gpu);
    let mut session = match factory.open(&request) {
        Ok(s) => s,
        Err(e) => panic!("open: {e}"),
    };
    if let Err(e) = session.start() {
        panic!("start: {e}");
    }
    let next = |session: &mut Box<dyn rbs_capture::CaptureSession>, until: Instant| loop {
        match session.latest_frame() {
            Ok(Some(frame)) => return Some(frame),
            Ok(None) if Instant::now() < until => std::thread::yield_now(),
            Ok(None) => return None,
            Err(e) => panic!("latest_frame: {e}"),
        }
    };
    let Some(held) = next(&mut session, Instant::now() + Duration::from_secs(5)) else {
        panic!("no frame within 5 s; keep something moving on screen")
    };
    let surface_of = |frame: &rbs_core::media::VideoFrame| match frame.payload() {
        FramePayload::Native {
            surface: NativeSurface::IoSurface { surface, .. },
            ..
        } => *surface,
        other => panic!("expected an IOSurface, got {other:?}"),
    };
    let held_ptr = surface_of(&held);
    // SAFETY: the held frame keeps the surface alive and unwritten while it exists.
    let seed = |ptr: usize| unsafe { (*(ptr as *const IOSurfaceRef)).seed() };
    let before = seed(held_ptr);
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut others = 0;
    while Instant::now() < deadline {
        if let Some(frame) = next(&mut session, deadline) {
            assert_ne!(
                surface_of(&frame),
                held_ptr,
                "a held surface was delivered again"
            );
            others += 1;
        }
    }
    assert_eq!(
        seed(held_ptr),
        before,
        "ScreenCaptureKit wrote into a held surface"
    );
    assert!(
        others > 0,
        "no other frame arrived; keep something moving on screen"
    );
    drop(held);
    session.stop();
}
