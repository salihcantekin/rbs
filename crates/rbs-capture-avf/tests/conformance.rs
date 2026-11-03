//! Runs the capture port conformance suite against the AVFoundation camera adapter.
//!
//! CI runners have no camera: the suite checks probing, enumeration (an empty list),
//! the pick and unknown-target rules, and that GPU memory is refused without a matching
//! import target; session checks have no target to run on. On other platforms the probe
//! fails and the suite reports everything skipped.
//!
//! With `RBS_CAPTURE_HW=1` on a Mac with a camera whose terminal has camera permission,
//! the hardware tests also open real sessions. They never ask for permission (a prompt
//! would block the run): grant it first, for example by running any camera app from the
//! same terminal, or under Claude Code to its helper app.

use rbs_capture::conformance::{self, Options, RealTimePacer};
use rbs_capture_avf::AvfCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[test]
fn passes_conformance_without_gpu() {
    let factory = AvfCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            if cfg!(target_os = "macos") {
                for check in ["caps", "enumerate", "pick", "unknown target"] {
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
fn hardware() -> bool {
    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Mac with a camera and camera permission");
        return false;
    }
    true
}

#[cfg(target_os = "macos")]
#[test]
fn hardware_session_passes_conformance() {
    use rbs_capture::{GpuDeviceId, GpuImportTarget, SyncKind};

    if !hardware() {
        return;
    }
    // IOSurfaces can be imported by any Metal device, so the registry id is not checked.
    let gpu = GpuImportTarget::new(GpuDeviceId::MetalRegistryId(0), &[SyncKind::Ready]);
    let factory = AvfCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        gpu: Some(&gpu),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            for check in [
                "formats",
                "standby",
                "timestamps",
                "latest wins",
                "in flight",
                "stop",
            ] {
                assert!(
                    report.passed.iter().any(|c| c.starts_with(check)),
                    "{check} did not run: {report:?}"
                );
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
}

/// The adapter's soundness rests on AVFoundation not writing into an IOSurface while
/// the adapter retains its pixel buffer for a held frame, and on its pool still
/// delivering while frames are held. This checks both on real hardware: hold one frame
/// (and two more) for a second, and require that the held surface is never delivered
/// again, its seed (bumped on every write) stays put, and other frames keep arriving.
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

    if !hardware() {
        return;
    }
    let factory = AvfCaptureFactory::new();
    let targets = match factory.enumerate(TargetKind::Camera) {
        Ok(t) => t,
        Err(e) => panic!("enumerate: {e}"),
    };
    let Some(target) = targets.first() else {
        panic!("no camera")
    };
    for pixel_format in [PixelFormat::Nv12, PixelFormat::Bgra8] {
        let gpu = GpuImportTarget::new(GpuDeviceId::MetalRegistryId(0), &[SyncKind::Ready]);
        let request = CaptureRequest::new(target.id.clone(), Arc::new(MonotonicClock::new()))
            .with_pixel_formats(&[pixel_format])
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
        let surface_of = |frame: &rbs_core::media::VideoFrame| match frame.payload() {
            FramePayload::Native {
                surface: NativeSurface::IoSurface { surface, .. },
                ..
            } => *surface,
            other => panic!("expected an IOSurface, got {other:?}"),
        };
        let Some(held) = next(&mut session, Instant::now() + Duration::from_secs(5)) else {
            panic!("no frame within 5 s")
        };
        assert_eq!(held.format().pixel_format, pixel_format);
        let held_ptr = surface_of(&held);
        let mut extra = Vec::new();
        for _ in 0..2 {
            if let Some(frame) = next(&mut session, Instant::now() + Duration::from_secs(1)) {
                extra.push(frame);
            }
        }
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
            "AVFoundation wrote into a held surface"
        );
        assert!(
            others > 0,
            "no other frame arrived while three frames were held ({pixel_format:?})"
        );
        drop(extra);
        drop(held);
        session.stop();
    }
}
