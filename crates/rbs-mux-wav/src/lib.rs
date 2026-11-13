//! WAV muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes one PCM stream (ADR-0085):
//! a RIFF WAVE header with a `JUNK` chunk the size of `ds64`, so the file stays a
//! classic WAV under 4 GiB and becomes RF64, by patches in place, when the 32-bit
//! size would overflow. Sizes are patched on the first packet and about every 1 MiB
//! after that, and again in `finish`, so a crash keeps the samples of the last patch.
//! The output worker does this; the audio callback does not.
//!
//! The stream's extradata is one byte, the bit depth: 16, 24 or 32 (float). Empty
//! extradata means 16-bit. Payloads are interleaved little-endian PCM
//! (`BitstreamFormat::Raw`).
//!
//! `write` clones the packet payload (`Bytes` is O(1) after its first shared header)
//! and does not copy the samples. A patch allocates the handful of size bytes, and
//! only on those periodic calls.

mod muxer;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.wav";

/// Factory for WAV muxer sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct WavMuxerFactory;

impl AdapterFactory for WavMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "WAV"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::Wav.as_str()]))
            .with(caps::CODECS, set(&["wav.pcm"]))
            .with(caps::LIVE, CapabilityValue::Flag(true))
            .with(caps::PATCHES, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for WavMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::Wav {
            return Err(CreateError::Unsupported {
                what: format!("container {} (this adapter writes wav)", config.container),
            });
        }
        Ok(Box::new(muxer::WavMuxer::new(config)?))
    }
}
