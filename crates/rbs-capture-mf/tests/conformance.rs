//! Runs the capture port conformance suite against the Media Foundation camera adapter.
//!
//! CI runners have no camera: the suite checks probing, enumeration (an empty list), the
//! pick and unknown-target rules; session checks have no target to run on. On other
//! platforms the probe fails and the suite reports everything skipped.
//!
//! With `RBS_CAPTURE_HW=1` on a Windows machine with a camera that desktop apps may use
//! (Settings, Privacy, Camera), `hardware_session_passes_conformance` opens real
//! sessions.

use rbs_capture::conformance::{self, Options, RealTimePacer};
use rbs_capture_mf::MfCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[test]
fn passes_conformance() {
    let factory = MfCaptureFactory::new();
    let options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            if cfg!(windows) {
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

#[cfg(windows)]
#[test]
fn hardware_session_passes_conformance() {
    if std::env::var_os("RBS_CAPTURE_HW").is_none() {
        eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Windows machine with a camera");
        return;
    }
    let factory = Connected(MfCaptureFactory::new());
    let options = Options {
        measure_allocations: Some(measure),
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
                    report.passed.iter().any(|c| c == check),
                    "{check} did not run: {report:?}"
                );
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
}

/// The adapter with its declared pixel formats narrowed to those the connected cameras
/// offer. The probe cannot ask devices, so it declares every format the adapter can
/// deliver; a given camera offers some of them (capture-port.md, Media Foundation
/// notes), and invariant 10 can only hold for the cameras at hand.
#[cfg(windows)]
struct Connected(MfCaptureFactory);

#[cfg(windows)]
impl rbs_core::registry::AdapterFactory for Connected {
    fn id(&self) -> rbs_core::registry::AdapterId {
        self.0.id()
    }

    fn display_name(&self) -> &str {
        self.0.display_name()
    }

    fn probe(&self) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
        use rbs_capture::{CaptureFactory, TargetKind, caps};
        use rbs_core::capability::CapabilityValue;

        let set = self.0.probe()?;
        let offered: std::collections::BTreeSet<String> = self
            .0
            .enumerate(TargetKind::Camera)
            .unwrap_or_default()
            .iter()
            .flat_map(|t| t.modes.iter().map(|m| m.pixel_format.as_str().to_owned()))
            .collect();
        Ok(set.with(
            caps::PIXEL_FORMATS,
            CapabilityValue::Set(offered.into_iter().collect()),
        ))
    }
}

#[cfg(windows)]
impl rbs_capture::CaptureFactory for Connected {
    fn enumerate(
        &self,
        kind: rbs_capture::TargetKind,
    ) -> Result<Vec<rbs_capture::CaptureTarget>, rbs_capture::CaptureError> {
        self.0.enumerate(kind)
    }

    fn open(
        &self,
        request: &rbs_capture::CaptureRequest,
    ) -> Result<Box<dyn rbs_capture::CaptureSession>, rbs_capture::CaptureError> {
        self.0.open(request)
    }
}
