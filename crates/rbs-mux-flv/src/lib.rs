//! FLV muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes FLV version 1 with one
//! H.264 video stream (codec id 7) and one AAC audio stream (sound format 10), the
//! format RTMP carries.
//!
//! Behaviour:
//! - `begin` emits the FLV header, an `onMetaData` script tag (size, frame rate,
//!   codecs, sample rate) and the AVC and AAC sequence headers, as one `Header` chunk.
//! - Each packet becomes three `Media` chunks at once: the tag header with the body
//!   prefix, the payload, and `PreviousTagSize`. The payload is passed through as a
//!   `Bytes` slice when the packets are length-prefixed with the avcC length size (and
//!   for AAC); Annex B input is converted into one exactly sized buffer. Tag headers
//!   come from a shared arena, so they allocate once per 16 KiB.
//! - Nothing is held back and there is no trailer, so the output is live and
//!   crash-safe (`mux.live`). Duration and file size stay 0 in the metadata.
//! - FLV timestamps are milliseconds from the first dts written; the composition time
//!   offset carries `pts - dts`.
//!
//! Limitations: one video and one audio stream; H.264 and AAC only (no enhanced-RTMP
//! HEVC/AV1 yet); sessions longer than 49 days overflow the 32-bit timestamp.

mod amf0;
mod muxer;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.flv";

/// Factory for FLV muxer sessions. Probing is free: no devices or libraries.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlvMuxerFactory;

impl AdapterFactory for FlvMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "FLV"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::Flv.as_str()]))
            .with(caps::CODECS, set(&["h264", "aac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for FlvMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::Flv {
            return Err(CreateError::Unsupported {
                what: format!("container {} (this adapter writes flv)", config.container),
            });
        }
        Ok(Box::new(muxer::FlvMuxer::new(config)?))
    }
}
