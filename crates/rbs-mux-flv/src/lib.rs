//! FLV muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes FLV version 1 with one
//! H.264 video stream (codec id 7) and one AAC audio stream (sound format 10), the
//! format RTMP carries.
//!
//! Behaviour:
//! - `begin` emits the FLV header, an `onMetaData` script tag (size, frame rate,
//!   codecs, sample rate) and the AVC and AAC sequence headers, as one `Header` chunk.
//! - Each packet becomes one `Media` chunk at once holding the whole tag (header,
//!   body, `PreviousTagSize`), so a transport that drops or refuses chunks always
//!   drops whole tags. The chunk is sized exactly and is the only allocation per
//!   packet; the payload is copied into it once, re-framed with the avcC length size
//!   (4 bytes for Annex B input). Access unit delimiters are left out.
//! - Nothing is held back and there is no trailer, so the output is live and
//!   crash-safe (`mux.live`). Duration and file size stay 0 in the metadata.
//! - FLV time is milliseconds from one interleaving bound (`MuxerConfig::max_buffered`)
//!   before the first dts written, so streams that arrive slightly out of order keep
//!   their timestamps; a dts earlier than that is refused. The composition time is
//!   derived from the same rounded clock. Time wraps after 49.7 days, as RTMP allows.
//! - Annex B input needs SPS and PPS in the extradata (avcC or Annex B), because the
//!   AVC sequence header is written in `begin`; the output builder supplies them or
//!   picks a length-prefixed encoder. A parameter set change mid-stream gets no new
//!   sequence header.
//!
//! Limitations: one video and one audio stream; H.264 and AAC only (no enhanced-RTMP
//! HEVC/AV1 yet).

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
