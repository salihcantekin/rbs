//! Off x86_64 and aarch64 Linux the adapter compiles, probes to nothing and refuses sessions.

#![cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_ffmpeg::FfmpegAacFactory;
use rbs_core::media::{AudioFormat, CodecId};
use rbs_core::registry::AdapterFactory;

#[test]
fn probes_empty_and_refuses_sessions() {
    let factory = FfmpegAacFactory::new();
    assert!(factory.probe().unwrap().is_empty());
    let config = AudioEncoderConfig::new(CodecId::Aac, AudioFormat::STEREO_48K, 128_000);
    assert!(factory.create(&config).is_err());
}
