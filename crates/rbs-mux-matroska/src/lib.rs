//! Matroska muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes Matroska (RFC 9559) for
//! live recording: an EBML header and a Segment of unknown size with `Info` and
//! `Tracks`, then one Cluster per chunk. H.264 is `V_MPEG4/ISO/AVC` with the avcC as
//! codec private data; AAC is `A_AAC` with the AudioSpecificConfig. Every Cluster is
//! complete, so a recording is playable up to its last Cluster after a crash
//! (`mux.live`).
//!
//! Behaviour:
//! - `begin` emits the EBML header, the Segment start, `Info` and `Tracks` as one
//!   `Header` chunk.
//! - Blocks are collected and emitted as one `Media` chunk per Cluster (sized exactly:
//!   one allocation per Cluster, one copy of the media for length-prefixed input and
//!   AAC, two for Annex B input), in decode order across tracks, as `SimpleBlock`s
//!   without lacing. A Cluster ends before each video keyframe, so Clusters start at
//!   entry points (`keyframe` set), or once it spans `MuxerConfig::fragment_duration`
//!   (1 s by default, at most 20 s), or at 1 GiB or 65 536 blocks. `finish` emits the
//!   last Cluster, then two `Patch` chunks (`mux.patches`): the Segment `Duration`
//!   (latest pts plus duration of any block, from time 0), written over an 11-byte
//!   `Void` the header reserves in `Info`, and the Segment size over its unknown
//!   size. A block lasts the frame interval (video) or the AAC frame size
//!   (audio). Outputs that cannot seek ignore patches and keep a
//!   valid live file without a duration. There are no Cues (a seek index).
//! - At most one video stream: a keyframe chunk must be an entry point for every
//!   stream, which two independent GOP structures would rarely give.
//! - A failed `write` emits nothing and changes nothing.
//! - Timestamps are milliseconds (`TimestampScale` 1 000 000), rounded to the nearest.
//!   Time 0 is one interleaving bound (at most 20 s) before the first dts, so streams
//!   that arrive slightly out of order keep their timestamps; a dts earlier than that,
//!   or more than the bound behind the newest dts of any stream, is refused, and pts
//!   may trail dts by at most 10 s. A Cluster's dts range stays below its span even
//!   for late blocks, so every 16-bit block timestamp is exact. Players show the recording from its first
//!   block, about one second in. Block timestamps are presentation times; the Cluster
//!   timestamp is the earliest in the Cluster.
//! - Length-prefixed H.264 is copied as is; Annex B input is converted to 4-byte
//!   lengths without AUD, SPS and PPS, which live in the codec private data. An
//!   in-band SPS or PPS that differs from the extradata is refused (`InvalidPacket`).
//!
//! Limitations: H.264 and AAC only (HEVC, AV1 and Opus to come); no Cues, chapters or
//! tags; no WebM doc type yet (it needs VP9/AV1 and Opus).

mod ebml;
mod muxer;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.matroska";

/// Factory for Matroska muxer sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct MatroskaMuxerFactory;

impl AdapterFactory for MatroskaMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Matroska"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::Matroska.as_str()]))
            .with(caps::CODECS, set(&["h264", "aac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true))
            .with(caps::PATCHES, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for MatroskaMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::Matroska {
            return Err(CreateError::Unsupported {
                what: format!(
                    "container {} (this adapter writes matroska)",
                    config.container
                ),
            });
        }
        Ok(Box::new(muxer::MatroskaMuxer::new(config)?))
    }
}
