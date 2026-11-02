//! Fragmented MP4 muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes ISO BMFF with movie
//! fragments (`ftyp` + `moov` with `mvex`, then `moof` + `mdat` pairs): H.264 in `avc1`
//! sample entries and AAC in `mp4a`/`esds`. Every fragment is self-contained, so a
//! recording is playable up to its last complete fragment after a crash (`mux.live`),
//! and the same bytes serve CMAF-style segmenting later.
//!
//! Behaviour:
//! - `begin` emits `ftyp` and `moov` as one `Header` chunk.
//! - Samples are collected and emitted as one `Media` chunk per fragment (`moof` +
//!   `mdat`, sized exactly: one allocation and one copy of the media per fragment).
//!   A fragment ends before each video keyframe, so fragments start at entry points
//!   (`keyframe` set), or once it spans `MuxerConfig::fragment_duration` (1 s by
//!   default), which bounds how long media is held back. `finish` emits the last
//!   fragment; there is no `mfra` index.
//! - Media time starts one interleaving bound before the first dts, so streams that
//!   arrive slightly out of order keep their timestamps; a dts earlier than that is
//!   refused. Video uses a 90 kHz timescale, audio its sample rate; sample durations
//!   come from the next sample's dts (the packet duration or the nominal frame
//!   interval for the last sample of a fragment). Composition offsets are signed
//!   (`trun` version 1).
//! - Length-prefixed H.264 is copied as is; Annex B input is converted to 4-byte
//!   lengths without AUD, SPS and PPS, which live in the sample entry. Annex B input
//!   needs SPS and PPS in the extradata.
//!
//! Limitations: H.264 and AAC only (HEVC, AV1 and Opus to come); no `sidx` or `mfra`;
//! a parameter set change mid-stream gets no new sample entry.

mod boxes;
mod muxer;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.fmp4";

/// Factory for fragmented MP4 muxer sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fmp4MuxerFactory;

impl AdapterFactory for Fmp4MuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Fragmented MP4"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::Mp4Fragmented.as_str()]))
            .with(caps::CODECS, set(&["h264", "aac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for Fmp4MuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::Mp4Fragmented {
            return Err(CreateError::Unsupported {
                what: format!(
                    "container {} (this adapter writes mp4.fragmented)",
                    config.container
                ),
            });
        }
        Ok(Box::new(muxer::Fmp4Muxer::new(config)?))
    }
}
