//! Runs the `rbs-mux` conformance suite against the FLV muxer.

use rbs_mux::conformance;
use rbs_mux_flv::FlvMuxerFactory;

#[test]
fn passes_the_muxer_suite() {
    let report = conformance::muxer(&FlvMuxerFactory);
    report.assert_passed();
    report.assert_ran(&[
        "flv/chunk-order",
        "flv/keyframe-entry-points",
        "flv/interleaving",
        "flv/patches",
        "flv/live-stream-structure",
        "flv/non-monotonic-dts",
        "flv/undeclared-codec",
    ]);
}
