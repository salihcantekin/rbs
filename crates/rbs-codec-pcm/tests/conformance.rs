//! Runs the `rbs-codec` audio conformance suite against the PCM encoder.

use rbs_codec::conformance;
use rbs_codec_pcm::PcmEncoderFactory;

#[test]
fn passes_the_audio_encoder_suite() {
    let report = conformance::audio_encoder(&PcmEncoderFactory);
    report.assert_ran(&["probe", "pcm/stream", "pcm/packets"]);
}
