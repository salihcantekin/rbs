//! Native FLAC muxer for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes one FLAC stream
//! (ADR-0085): the `fLaC` marker and a STREAMINFO block, then each packet's frame
//! bytes unchanged. Total samples, frame sizes and the MD5 are stored as unknown,
//! so nothing is patched later and a crash keeps every frame written so far.
//! The stream's extradata is one byte, the integer bit depth (16 or 24). Empty
//! extradata means 16-bit. Payloads are FLAC frames (`BitstreamFormat::Raw`).
//! `frame_size` in the stream descriptor is the fixed block size written into
//! STREAMINFO; the last frame may be shorter.

mod muxer;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.flac";

/// Factory for FLAC muxer sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlacMuxerFactory;

impl AdapterFactory for FlacMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "FLAC"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::Flac.as_str()]))
            .with(caps::CODECS, set(&["flac.flac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true))
            .with(caps::PATCHES, CapabilityValue::Flag(false)))
    }
}

impl MuxerFactory for FlacMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::Flac {
            return Err(CreateError::Unsupported {
                what: format!("container {} (this adapter writes flac)", config.container),
            });
        }
        Ok(Box::new(muxer::FlacMuxer::new(config)?))
    }
}
