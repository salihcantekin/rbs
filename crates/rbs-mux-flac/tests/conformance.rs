//! Runs the `rbs-mux` conformance suite against the FLAC muxer.
//!
//! The suite's timed sequence is H.264 and AAC. This adapter declares neither, so
//! those cases are skipped; `probe` still has to pass, and the byte-level checks
//! live next to the muxer.

use rbs_mux::conformance;
use rbs_mux_flac::FlacMuxerFactory;

#[test]
fn passes_the_muxer_suite() {
    let report = conformance::muxer(&FlacMuxerFactory);
    report.assert_passed();
}
