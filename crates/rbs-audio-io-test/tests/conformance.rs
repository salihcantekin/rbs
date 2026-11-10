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

#[allow(clippy::expect_used)] // Tests fail by panicking.
#[test]
fn synthetic_output_passes_the_output_conformance_suite() {
    use rbs_audio_io::conformance_output;
    use rbs_audio_io::{AudioBackend, EndpointKind};
    use rbs_core::registry::AdapterFactory;

    let backend = TestAudioBackend::new();
    let caps = backend.probe().expect("the test adapter always probes");
    let listed = backend
        .enumerate(EndpointKind::Output)
        .expect("the test adapter lists outputs");
    let endpoint = listed.first().expect("one test output").id.clone();
    let report = conformance_output::run(&backend, &caps, &endpoint, &mut |frames| {
        backend.pump_outputs(frames);
    });
    report.assert_ran(&[
        "output capability",
        "output enumerate",
        "output unknown endpoint",
        "output standby",
        "output accounting",
        "output overrun",
        "output restart",
    ]);
}

#[test]
fn a_playback_device_and_its_loopback_share_a_device_key() {
    use rbs_audio_io::{AudioBackend, EndpointKind};
    let backend = TestAudioBackend::new();
    let key = |kind| {
        backend
            .enumerate(kind)
            .ok()
            .and_then(|l| l.into_iter().find_map(|e| e.device_key))
    };
    assert!(key(EndpointKind::Output).is_some());
    assert_eq!(key(EndpointKind::Output), key(EndpointKind::OutputLoopback));
}
