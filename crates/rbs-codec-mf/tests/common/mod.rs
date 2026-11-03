//! Shared by the test files: the factories every behaviour test runs against.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::AudioEncoderFactory;
use rbs_core::registry::AdapterFactory;

/// `RBS_REQUIRE_MF_AAC=1`: the Media Foundation encoder must be present (Windows
/// desktop editions have it), so its tests cannot skip.
pub fn required() -> bool {
    std::env::var("RBS_REQUIRE_MF_AAC").is_ok_and(|v| v == "1")
}

/// The real factory when this machine has the encoder.
pub fn real() -> Option<rbs_codec_mf::MfAacFactory> {
    let factory = rbs_codec_mf::MfAacFactory::new();
    let available = !factory.probe().unwrap().is_empty();
    assert!(
        available || !required(),
        "RBS_REQUIRE_MF_AAC=1 but the Media Foundation AAC encoder probed to nothing"
    );
    if !available {
        eprintln!("no Media Foundation AAC encoder here: real-encoder checks skipped");
    }
    available.then_some(factory)
}

/// The fake engine everywhere, plus the real encoder where present.
pub fn factories() -> Vec<(&'static str, Box<dyn AudioEncoderFactory>)> {
    let mut all: Vec<(&'static str, Box<dyn AudioEncoderFactory>)> = vec![(
        "fake",
        Box::new(rbs_codec_mf::testing::FakeAacFactory::new()),
    )];
    if let Some(f) = real() {
        all.push(("mf", Box::new(f)));
    }
    all
}
