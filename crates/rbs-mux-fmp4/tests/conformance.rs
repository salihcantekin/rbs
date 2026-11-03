//! Runs the `rbs-mux` conformance suite against the fragmented MP4 muxer.

use rbs_mux::conformance;
use rbs_mux_fmp4::Fmp4MuxerFactory;

#[test]
fn passes_the_muxer_suite() {
    let report = conformance::muxer(&Fmp4MuxerFactory);
    report.assert_passed();
    report.assert_ran(&[
        "mp4.fragmented/chunk-order",
        "mp4.fragmented/keyframe-entry-points",
        "mp4.fragmented/interleaving",
        "mp4.fragmented/patches",
        "mp4.fragmented/live-stream-structure",
        "mp4.fragmented/non-monotonic-dts",
        "mp4.fragmented/undeclared-codec",
    ]);
}
