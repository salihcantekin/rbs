//! Runs the `rbs-mux` conformance suite against the MPEG-TS muxer.

use rbs_mux::conformance;
use rbs_mux_mpegts::MpegTsMuxerFactory;

#[test]
fn passes_the_muxer_suite() {
    let report = conformance::muxer(&MpegTsMuxerFactory);
    report.assert_passed();
    report.assert_ran(&[
        "mpegts/chunk-order",
        "mpegts/keyframe-entry-points",
        "mpegts/interleaving",
        "mpegts/patches",
        "mpegts/live-stream-structure",
        "mpegts/non-monotonic-dts",
        "mpegts/undeclared-codec",
    ]);
}
