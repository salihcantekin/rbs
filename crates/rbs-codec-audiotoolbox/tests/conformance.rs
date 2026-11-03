//! Runs the encoder port's audio suite against the AudioToolbox AAC adapter. AAC is
//! part of every macOS, so the suite must run, not skip.

#![cfg(target_os = "macos")]

use rbs_codec::conformance;
use rbs_codec_audiotoolbox::AacFactory;

#[test]
fn passes_the_audio_suite() {
    let report = conformance::audio_encoder(&AacFactory::new());
    eprintln!("{report}");
    report.assert_ran(&["aac/stream", "aac/packets"]);
}
