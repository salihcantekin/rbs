//! Runs the audio I/O port conformance suite against the synthetic adapter.

use rbs_audio_io::conformance::{self, Options, TestPacer};
use rbs_audio_io_test::TestAudioBackend;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[test]
fn synthetic_adapter_passes_conformance() {
    let backend = TestAudioBackend::new();
    let options = Options {
        measure_allocations: Some(measure),
        hooks: Some(&backend),
        strict: true,
        max_endpoints: usize::MAX,
        ..Options::default()
    };
    let report = conformance::run(&backend, &mut TestPacer::new(), &options);
    report.assert_ran(&[
        "probe",
        "enumerate",
        "unknown endpoint",
        "format",
        "standby",
        "buffer mismatch",
        "delivery",
        "timestamps",
        "no allocation",
        "overflow",
        "accounting",
        "stop",
        "reads",
        "restart",
        "drop",
        "endpoint lost",
    ]);
    // Every endpoint ran every case (three endpoints).
    let passed = report
        .cases
        .iter()
        .filter(|c| c.name == "timestamps")
        .count();
    assert_eq!(passed, 3, "{report}");
}
