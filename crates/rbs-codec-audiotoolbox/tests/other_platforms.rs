//! Off macOS the adapter compiles, probes to nothing and refuses sessions.

#![cfg(not(target_os = "macos"))]
#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_audiotoolbox::AacFactory;
use rbs_core::media::{AudioFormat, CodecId};
use rbs_core::registry::AdapterFactory;

#[test]
fn probes_empty_and_refuses_sessions() {
    let factory = AacFactory::new();
    assert!(factory.probe().unwrap().is_empty());
    let config = AudioEncoderConfig::new(CodecId::Aac, AudioFormat::STEREO_48K, 128_000);
    assert!(factory.create(&config).is_err());
}
