//! Runs the encoder port's audio suite against the session over the fake engine (every
//! platform) and against the Media Foundation encoder (Windows with Media Foundation;
//! skipped elsewhere unless `RBS_REQUIRE_MF_AAC=1`).

mod common;

use rbs_codec::conformance;

const CASES: [&str; 2] = ["aac/stream", "aac/packets"];

#[test]
fn fake_engine_passes_the_audio_suite() {
    let report = conformance::audio_encoder(&rbs_codec_mf::testing::FakeAacFactory::new());
    eprintln!("{report}");
    report.assert_ran(&CASES);
}

#[test]
fn media_foundation_passes_the_audio_suite() {
    let Some(factory) = common::real() else {
        return;
    };
    let report = conformance::audio_encoder(&factory);
    eprintln!("{report}");
    report.assert_ran(&CASES);
}
