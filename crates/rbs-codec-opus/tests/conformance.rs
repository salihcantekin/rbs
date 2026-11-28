//! Runs the `rbs-codec` audio conformance suite against the Opus encoder.

use rbs_codec::conformance;
use rbs_codec_opus::OpusEncoderFactory;

#[test]
fn passes_the_audio_encoder_suite() {
    let report = conformance::audio_encoder(&OpusEncoderFactory);
    report.assert_ran(&["probe", "opus/stream", "opus/packets"]);
}
