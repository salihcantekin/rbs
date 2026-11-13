//! Runs the `rbs-codec` audio conformance suite against the FLAC encoder.

use rbs_codec::conformance;
use rbs_codec_flac::FlacEncoderFactory;

#[test]
fn passes_the_audio_encoder_suite() {
    let report = conformance::audio_encoder(&FlacEncoderFactory);
    report.assert_ran(&["probe", "flac/stream", "flac/packets"]);
}
