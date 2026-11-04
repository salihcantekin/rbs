//! Runs the encoder port's audio suite against the session over the fake engine (every
//! platform) and against the FFmpeg encoder (Linux with FFmpeg 6, 7 or 8;
//! skipped elsewhere unless `RBS_REQUIRE_FFMPEG_AAC=1`).

mod common;

use rbs_codec::conformance;

const CASES: [&str; 2] = ["aac/stream", "aac/packets"];

#[test]
fn fake_engine_passes_the_audio_suite() {
    let report = conformance::audio_encoder(&rbs_codec_ffmpeg::testing::FakeAacFactory::new());
    eprintln!("{report}");
    report.assert_ran(&CASES);
}

#[test]
fn ffmpeg_passes_the_audio_suite() {
    let Some(factory) = common::real() else {
        return;
    };
    let report = conformance::audio_encoder(&factory);
    eprintln!("{report}");
    report.assert_ran(&CASES);
}
