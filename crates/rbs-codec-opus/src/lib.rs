//! Opus encoder and live decoder for the RBS codec ports (`rbs-codec`).
//!
//! The codec is Xiph's reference libopus 1.5.2 (BSD-3-Clause), vendored under
//! `vendor/libopus` and compiled by `build.rs` with the `cc` crate: no CMake, no system
//! library (ADR-0046, ADR-0139 decision 3). It is the float build without SIMD dispatch;
//! the deep-learning extensions (DRED, deep PLC) are not built.
//!
//! Streams are 48 kHz, mono or stereo. Sessions are created on demand; an installed
//! adapter costs nothing until then (ADR-0013). `submit` and `decode` allocate only the
//! returned packet's bytes (encoder) or nothing (decoder, with a reserved output buffer):
//! libopus works on the stack and the staging buffers are reserved at creation.
//!
//! Encoder `adapter_options`: `application` (`audio` default, `voip`, `lowdelay`),
//! `frame_ms` (`2.5`, `5`, `10`, `20` default, `40`, `60`), `complexity` (`0` to `10`,
//! default 9), `vbr` (`on` default, `off` for constant bitrate), `fec` (`on` or `off`,
//! default `off`) and `loss_percent` (`0` to `100`, default 0; tells the encoder how much
//! redundancy to spend when `fec` is on). Decoder options: none.

mod ffi;
mod options;
mod session;
#[cfg(test)]
mod vectors;

use rbs_codec::{
    AudioDecoder, AudioDecoderConfig, AudioDecoderFactory, AudioEncoder, AudioEncoderConfig,
    AudioEncoderFactory, CreateError, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

pub use ffi::version as libopus_version;

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.codec.opus";

/// Sample rate of every Opus stream this crate handles.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Most sample frames (per channel) one Opus packet decodes to: 120 ms at 48 kHz.
pub const MAX_PACKET_FRAMES: usize = 5760;

/// Factory for Opus encoder sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpusEncoderFactory;

/// Factory for Opus decoder sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpusDecoderFactory;

fn base_caps() -> CapabilitySet {
    CapabilitySet::new()
        .with(caps::HARDWARE, CapabilityValue::Flag(false))
        .with(
            caps::AUDIO_SAMPLE_RATES,
            CapabilityValue::Set(vec![SAMPLE_RATE_HZ.to_string()]),
        )
        .with(caps::AUDIO_CHANNELS_MAX, CapabilityValue::Int(2))
}

impl AdapterFactory for OpusEncoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Opus (libopus)"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(base_caps()
            .with(
                caps::AUDIO_ENCODE,
                CapabilityValue::Set(vec![CodecId::Opus.as_str().to_owned()]),
            )
            .with(
                caps::RECONFIGURE,
                CapabilityValue::Set(vec![caps::RECONFIGURE_BITRATE.to_owned()]),
            ))
    }
}

impl AdapterFactory for OpusDecoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Opus (libopus)"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(base_caps().with(
            caps::AUDIO_DECODE,
            CapabilityValue::Set(vec![CodecId::Opus.as_str().to_owned()]),
        ))
    }
}

impl AudioEncoderFactory for OpusEncoderFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(session::EncoderSession::new(config)?))
    }
}

impl AudioDecoderFactory for OpusDecoderFactory {
    fn create(&self, config: &AudioDecoderConfig) -> Result<Box<dyn AudioDecoder>, CreateError> {
        Ok(Box::new(session::DecoderSession::new(config)?))
    }
}
