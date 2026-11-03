//! Runs the `rbs-mux` conformance suite against the Matroska muxer.

use rbs_mux::conformance;
use rbs_mux_matroska::MatroskaMuxerFactory;

#[test]
fn passes_the_muxer_suite() {
    let report = conformance::muxer(&MatroskaMuxerFactory);
    report.assert_passed();
    report.assert_ran(&[
        "matroska/chunk-order",
        "matroska/keyframe-entry-points",
        "matroska/interleaving",
        "matroska/patches",
        "matroska/live-stream-structure",
        "matroska/non-monotonic-dts",
        "matroska/undeclared-codec",
    ]);
}
