//! An adapter that only declares a GPU memory kind (like Windows Graphics Capture)
//! passes the non-strict suite without a GPU import target: sessions are skipped, the
//! refusal without an import target is still checked.

use rbs_capture::conformance::{self, Options, TestPacer};
use rbs_capture::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureTarget, GpuDeviceId,
    TargetKind, caps,
};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::MemoryKind;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

struct GpuOnly {
    inner: TestCaptureFactory,
    /// Accept requests without an import target (a broken adapter).
    lenient: bool,
}

impl AdapterFactory for GpuOnly {
    fn id(&self) -> AdapterId {
        self.inner.id()
    }
    fn display_name(&self) -> &str {
        "gpu only"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(self.inner.probe()?.with(
            caps::MEMORY,
            CapabilityValue::Set(vec![MemoryKind::D3d11.as_str().to_owned()]),
        ))
    }
}

impl CaptureFactory for GpuOnly {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        self.inner.enumerate(kind)
    }
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        // Look the target up first, as a real adapter does.
        if !TargetKind::ALL.into_iter().any(|kind| {
            self.inner
                .enumerate(kind)
                .is_ok_and(|ts| ts.iter().any(|t| t.id == request.target))
        }) {
            return Err(CaptureError::TargetNotFound);
        }
        let shareable = request
            .gpu
            .as_ref()
            .is_some_and(|g| matches!(g.device, GpuDeviceId::Luid(_)));
        if !shareable && !self.lenient {
            return Err(CaptureError::FormatUnsupported);
        }
        self.inner
            .open(&request.clone().with_memory(&[MemoryKind::System]))
    }
}

#[test]
fn gpu_only_adapter_skips_sessions_without_import_target() {
    let factory = GpuOnly {
        inner: TestCaptureFactory::new(),
        lenient: false,
    };
    let report = match conformance::run(&factory, &mut TestPacer::new(), &Options::default()) {
        Ok(report) => report,
        Err(failure) => panic!("conformance failed: {failure}"),
    };
    assert!(
        report.passed.iter().any(|c| c == "gpu import"),
        "{report:?}"
    );
    assert!(
        report.skipped.iter().any(|s| s.starts_with("sessions")),
        "{report:?}"
    );

    let strict = Options {
        strict: true,
        ..Options::default()
    };
    let failure = match conformance::run(&factory, &mut TestPacer::new(), &strict) {
        Ok(report) => panic!("strict run must fail on skipped sessions: {report:?}"),
        Err(failure) => failure,
    };
    assert_eq!(failure.check, "strict");
}

#[test]
fn gpu_import_refusal_is_checked_without_import_target() {
    let factory = GpuOnly {
        inner: TestCaptureFactory::new(),
        lenient: true,
    };
    match conformance::run(&factory, &mut TestPacer::new(), &Options::default()) {
        Ok(report) => panic!("a GPU kind opened without an import target passed: {report:?}"),
        Err(failure) => assert_eq!(failure.check, "gpu import"),
    }
}
