//! MPEG-TS muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes one program with H.264
//! (stream type 0x1b, Annex B with an access unit delimiter and parameter sets before
//! every keyframe) and AAC (stream type 0x0f, ADTS). Length-prefixed H.264 input is
//! converted with `rbs-media-format` (one counted copy, see `docs/specs/output-ports.md`).
//!
//! Behaviour:
//! - `begin` emits PAT and PMT; they are repeated before every video keyframe and at
//!   least every 100 ms.
//! - Every packet becomes one `Media` chunk at once: nothing is held back, so the
//!   interleaving bound is always met and the output is live and crash-safe
//!   (`mux.live`). There is no trailer.
//! - The PCR rides on the first video stream (or the first stream in an audio-only
//!   output); PTS and DTS lead it by 0.7 s. Timestamps are master-clock time on the
//!   90 kHz clock, modulo 2^33.
//! - Chunk timestamps are kept non-decreasing when streams arrive slightly out of dts
//!   order.
//!
//! Limitations: HEVC, Opus and other codecs are refused at `create`; at most 32
//! streams; no SCTE-35, no PCR-only packets.

mod muxer;
mod packetizer;
mod psi;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_mux::{CreateError, Muxer, MuxerConfig, MuxerFactory, caps};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.mux.mpegts";

/// Factory for MPEG-TS muxer sessions. Probing is free: no devices or libraries.
#[derive(Debug, Default, Clone, Copy)]
pub struct MpegTsMuxerFactory;

impl AdapterFactory for MpegTsMuxerFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "MPEG-TS"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |values: &[&str]| {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        };
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&[ContainerId::MpegTs.as_str()]))
            .with(caps::CODECS, set(&["h264", "aac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for MpegTsMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        if config.container != ContainerId::MpegTs {
            return Err(CreateError::Unsupported {
                what: format!(
                    "container {} (this adapter writes mpegts)",
                    config.container
                ),
            });
        }
        Ok(Box::new(muxer::TsMuxer::new(config)?))
    }
}
