//! Fragmented MP4 muxer adapter for the RBS muxer port (`rbs-mux`).
//!
//! Pure Rust, no external libraries, all platforms. Writes ISO BMFF with movie
//! fragments (`ftyp` + `moov` with `mvex`, then `moof` + `mdat` pairs): H.264 in `avc1`
//! sample entries, HEVC in `hvc1`, AV1 in `av01` and AAC in `mp4a`/`esds`. Every fragment is self-contained, so a
//! recording is playable up to its last complete fragment after a crash (`mux.live`),
//! and the same bytes serve CMAF-style segmenting later (the file itself carries all
//! tracks in each fragment, so it does not claim the `cmfc` brand).
//!
//! Behaviour:
//! - `begin` emits `ftyp` and `moov` as one `Header` chunk.
//! - Samples are collected and emitted as one `Media` chunk per fragment (`moof` +
//!   `mdat`, sized exactly: one allocation per fragment, and one copy of the media
//!   for length-prefixed input and AAC, two for Annex B input). A fragment ends before
//!   each video keyframe, so fragments start at entry points (`keyframe` set), or once
//!   it spans `MuxerConfig::fragment_duration` (1 s by default), which bounds how long
//!   media is held back, or at 1 GiB or 65 536 samples. Very short GOPs give short
//!   fragments. `finish` emits the last fragment, then `Patch` chunks (`mux.patches`)
//!   that fill the movie and track durations (`mvhd` and `tkhd`, timescale 1000) over
//!   the zeros the header reserved. Each track keeps only its latest presentation
//!   end, updated from the timestamps a fragment already walks, so a recording holds
//!   nothing extra for the length. Outputs that cannot seek ignore the patches, and
//!   a crash never applies them: the fields stay 0 and the fragments stay playable.
//!   The media-header duration (`mdhd`) stays 0, because a 32-bit field at 90 kHz
//!   overflows past about 13 hours and the fragments already carry each sample's
//!   duration. There is no `mfra` index.
//! - At most one video stream: a keyframe chunk must be an entry point for every
//!   stream, which two independent GOP structures would rarely give.
//! - A failed `write` emits nothing and changes nothing.
//! - Media time 0 is the earliest sample of the first fragment, so the file starts at
//!   0 without an edit list (FFmpeg reads a positive edit media time on an AAC track as
//!   encoder priming and drops that much audio). The origin is chosen when the first
//!   fragment is emitted; if that fragment spans less than the interleaving bound
//!   (a GOP shorter than `fragment_duration`), the origin is one bound before the
//!   newest packet so far instead, and the file starts that much after 0. Until then a
//!   dts more than one bound before the first packet is refused, and afterwards one
//!   before the origin. Video uses a 90 kHz timescale, audio its sample rate.
//!   Positions are rounded to the nearest tick and durations are differences of
//!   positions: a sample lasts until the next one of its track (the last of a fragment
//!   until the packet that ended it, or for its packet duration or the nominal frame
//!   interval).
//!   Composition offsets use `trun` version 1; pts may trail dts by at most 60 s.
//! - Length-prefixed H.264 is copied as is; Annex B input is converted to 4-byte
//!   lengths without AUD, SPS and PPS, which live in the sample entry. Annex B input
//!   needs SPS and PPS in the extradata.
//!
//! Limitations: H.264, HEVC, AV1 (OBU samples, an `av01` entry) and AAC (Opus to come);
//! no `sidx` or `mfra`;
//! an in-band SPS or PPS that differs from the extradata is refused (`InvalidPacket`),
//! since a fragmented file has one sample entry: a format change needs a new output.

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
            .with(
                caps::CODECS,
                set(&[
                    "mp4.fragmented.h264",
                    "mp4.fragmented.hevc",
                    "mp4.fragmented.av1",
                    "mp4.fragmented.aac",
                ]),
            )
            .with(caps::LIVE, CapabilityValue::Flag(true))
            .with(caps::PATCHES, CapabilityValue::Flag(true)))
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
