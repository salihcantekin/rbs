//! Runs the capture port conformance suite against the synthetic adapter.

use rbs_capture::conformance::{self, Options, TestPacer};
use rbs_capture_test::TestCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

fn options(factory: &TestCaptureFactory) -> Options<'_> {
    Options {
        measure_allocations: Some(measure),
        hooks: Some(factory),
        strict: true,
        // This binary runs a single test, so the process's threads are its own.
        process_threads: cfg!(target_os = "linux")
            .then_some(conformance::process_thread_ids as fn() -> Option<Vec<u64>>),
        ..Options::default()
    }
}

#[test]
fn synthetic_adapter_passes_conformance() {
    let factory = TestCaptureFactory::new();
    let report = match conformance::run(&factory, &mut TestPacer::new(), &options(&factory)) {
        Ok(report) => report,
        Err(failure) => panic!("conformance failed: {failure}"),
    };
    for check in [
        "probe",
        "enumerate",
        "pick",
        "unknown target",
        "formats",
        "standby",
        "timestamps",
        "sequence",
        "latest wins",
        "no allocation",
        "in flight",
        "resize",
        "stop",
        "restart",
        "drop",
        "target lost",
    ] {
        assert!(
            report.passed.iter().any(|c| c == check),
            "{check} did not run: {report:?}"
        );
    }
}
