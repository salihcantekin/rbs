//! Runs the capture port conformance suite against the WGC adapter.
//!
//! Without a GPU import target (every CI runner) the suite checks probing,
//! enumeration, the pick and unknown-target rules and that GPU memory is refused
//! without a matching import target; session checks are skipped. On other platforms
//! the probe fails and the suite reports everything skipped.
//!
//! With `RBS_CAPTURE_HW=1` on a Windows desktop, `hardware_session_passes_conformance`
//! also opens real sessions on the first DXGI adapter. WGC only delivers frames when
//! content changes, so keep something moving on screen (a video) while it runs.

use rbs_capture::conformance::{self, Options, RealTimePacer};
use rbs_capture_wgc::WgcCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[test]
fn passes_conformance_without_gpu() {
    let factory = WgcCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            if cfg!(windows) && report.passed.iter().any(|c| c == "caps") {
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

#[cfg(windows)]
#[test]
fn hardware_session_passes_conformance() {
    use rbs_capture::{GpuDeviceId, GpuImportTarget, SyncKind};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Windows desktop to run");
        return;
    }
    // SAFETY: plain DXGI factory calls with no pointers of ours.
    let luid = unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(e) => panic!("DXGI factory: {e}"),
        };
        match factory.EnumAdapters1(0).and_then(|a| a.GetDesc1()) {
            Ok(desc) => desc.AdapterLuid,
            Err(e) => panic!("DXGI adapter 0: {e}"),
        }
    };
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    let gpu = GpuImportTarget::new(GpuDeviceId::Luid(bytes), &[SyncKind::TimelineFence]);

    let factory = WgcCaptureFactory::new();
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

/// A real display and window thumbnail on a Windows 11 desktop (`RBS_CAPTURE_HW=1`):
/// a picture, not `Unsupported`, inside the limit, with more than one colour, within
/// the 1 s budget. Prints each grab's size and time for the hardware record.
#[cfg(windows)]
#[test]
fn hardware_thumbnails_are_pictures() {
    use rbs_capture::{CaptureFactory, THUMBNAIL_MAX, TargetKind};
    use std::time::{Duration, Instant};

    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Windows desktop to run");
        return;
    }
    let factory = WgcCaptureFactory::new();
    let displays = match factory.enumerate(TargetKind::Display) {
        Ok(list) => list,
        Err(e) => panic!("enumerate displays: {e}"),
    };
    let Some(display) = displays.first() else {
        panic!("no display to grab");
    };
    let windows = factory.enumerate(TargetKind::Window).unwrap_or_default();
    let mut pictures = 0;
    for target in std::iter::once(display).chain(windows.iter().take(5)) {
        let started = Instant::now();
        let result = factory.thumbnail(&target.id, THUMBNAIL_MAX);
        let took = started.elapsed();
        let summary = match &result {
            Ok(p) => format!("{}x{}", p.width(), p.height()),
            Err(e) => format!("{e}"),
        };
        eprintln!("{:?} {:?}: {summary} in {took:?}", target.kind, target.name);
        // Hang guard only: the adapter's own budget is 1 s plus device creation.
        assert!(
            took < Duration::from_secs(3),
            "{}: took {took:?}",
            target.name
        );
        let Ok(picture) = result else {
            assert!(
                target.kind == TargetKind::Window,
                "display thumbnail failed: {summary}"
            );
            continue;
        };
        assert!(picture.width() <= THUMBNAIL_MAX.width);
        assert!(picture.height() <= THUMBNAIL_MAX.height);
        let rgba = picture.rgba();
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255), "not opaque");
        assert!(
            rgba.chunks_exact(4).any(|p| p[..3] != rgba[..3]),
            "{}: one colour only (a black frame?)",
            target.name
        );
        pictures += 1;
    }
    assert!(pictures >= 1);
}
