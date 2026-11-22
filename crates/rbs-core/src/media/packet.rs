//! Encoded media: codec ids, stream descriptors and packets.
//!
//! Payloads are [`Bytes`]: cloning is O(1) and sub-slices share memory, so one encode
//! feeds any number of muxers and transports without copying (performance budget
//! "packets fan out without copying payloads"). See `docs/specs/media-types.md#packets`.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;

use super::MediaError;
use super::audio::AudioFormat;
use super::video::VideoFormat;
use crate::time::Timestamp;
use crate::timing::FrameRate;

/// Video or audio.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Pictures.
    Video,
    /// Sound.
    Audio,
}

/// A compression format.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecId {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
    /// AAC (any profile).
    Aac,
    /// Opus.
    Opus,
    /// Uncompressed PCM (ADR-0085): 16-bit or 24-bit integer, or 32-bit float.
    Pcm,
    /// FLAC (ADR-0085). `rbs-codec-flac` writes one native frame per block.
    Flac,
}

impl CodecId {
    /// Stable lowercase name, used in capability values (`codec.video.encode`) and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            CodecId::H264 => "h264",
            CodecId::Hevc => "hevc",
            CodecId::Av1 => "av1",
            CodecId::Vp9 => "vp9",
            CodecId::Aac => "aac",
            CodecId::Opus => "opus",
            CodecId::Pcm => "pcm",
            CodecId::Flac => "flac",
        }
    }

    /// Whether this codec carries video or audio.
    pub const fn media_kind(self) -> MediaKind {
        match self {
            CodecId::H264 | CodecId::Hevc | CodecId::Av1 | CodecId::Vp9 => MediaKind::Video,
            CodecId::Aac | CodecId::Opus | CodecId::Pcm | CodecId::Flac => MediaKind::Audio,
        }
    }
}

impl fmt::Display for CodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the bytes of a payload are framed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitstreamFormat {
    /// H.264 / HEVC with start codes (`00 00 01`). Parameter sets may be in-band.
    AnnexB,
    /// H.264 / HEVC NAL units prefixed by their length (AVCC / HVCC), with parameter
    /// sets in [`StreamDescriptor::extradata`].
    LengthPrefixed {
        /// Size of each length prefix in bytes: 1, 2 or 4.
        length_size: u8,
    },
    /// AV1 low-overhead OBU format (`obu_has_size_field = 1`).
    Obu,
    /// VP9 frames, or raw audio frames (AAC without ADTS headers, Opus packets).
    Raw,
    /// AAC with ADTS headers.
    Adts,
}

/// Codec-specific parameters of a stream. Build with [`StreamParams::video`] or
/// [`StreamParams::audio`]; variants may gain fields.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamParams {
    /// A video stream.
    #[non_exhaustive]
    Video {
        /// Coded picture format: size, encoder input pixel format (bit depth and
        /// chroma subsampling), colour description to signal in the bitstream.
        format: VideoFormat,
        /// Nominal frame rate.
        frame_rate: FrameRate,
    },
    /// An audio stream.
    #[non_exhaustive]
    Audio {
        /// Sample rate and channels.
        format: AudioFormat,
        /// Sample frames per packet (1024 for AAC-LC, 960 for 20 ms Opus at 48 kHz).
        frame_size: u32,
    },
}

impl StreamParams {
    /// Video parameters.
    pub const fn video(format: VideoFormat, frame_rate: FrameRate) -> Self {
        Self::Video { format, frame_rate }
    }

    /// Audio parameters.
    pub const fn audio(format: AudioFormat, frame_size: u32) -> Self {
        Self::Audio { format, frame_size }
    }
}

/// Describes one encoded stream: produced by an encoder, consumed by muxers and
/// transports.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDescriptor {
    /// The codec.
    pub codec: CodecId,
    /// Framing of the packet payloads.
    pub bitstream: BitstreamFormat,
    /// Codec-private data: `AVCDecoderConfigurationRecord`, `HEVCDecoderConfigurationRecord`,
    /// `AV1CodecConfigurationRecord`, `AudioSpecificConfig`, `OpusHead`. Empty if none.
    pub extradata: Bytes,
    /// Video or audio parameters.
    pub params: StreamParams,
    /// The track's name in the file, `None` for none (ADR-0170 decision 6: an audio
    /// track is named after its bus). Containers without track names drop it.
    pub name: Option<String>,
    /// A player plays this track unless the user picks another (ADR-0170 decision 6).
    /// True from [`StreamDescriptor::new`]; a recording's tracks after track 1 are
    /// false, so players do not play them all at once.
    pub default: bool,
}

impl StreamDescriptor {
    /// Creates a descriptor.
    pub fn new(
        codec: CodecId,
        bitstream: BitstreamFormat,
        extradata: Bytes,
        params: StreamParams,
    ) -> Self {
        Self {
            codec,
            bitstream,
            extradata,
            params,
            name: None,
            default: true,
        }
    }

    /// The descriptor with `name` as its track name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The descriptor with `default` as its default flag.
    pub fn with_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

/// One encoded packet (an access unit for video, one or more frames for audio).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    codec: CodecId,
    bitstream: BitstreamFormat,
    pts: Timestamp,
    dts: Timestamp,
    duration: Option<Duration>,
    keyframe: bool,
    data: Bytes,
    /// Set by [`Self::attach_packet_share`] before the packet is cloned to several
    /// outputs. The first filtered conversion stores its bytes here; the next output
    /// clones them. Absent until attached, so creating and cloning a packet allocates
    /// nothing for it.
    share: Option<Arc<PacketShare>>,
}

/// One filtered conversion of a packet, shared by every clone.
///
/// The key is the parameter-set bytes the conversion was checked against. A caller
/// whose key differs does not see the sample.
#[derive(Debug)]
pub struct PacketShare {
    slot: OnceLock<(Bytes, Bytes)>,
}

impl PacketShare {
    /// The stored sample when `key` is the one it was stored under.
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let (stored, sample) = self.slot.get()?;
        (stored.as_ref() == key).then(|| sample.clone())
    }

    /// Keeps `sample` for `key` when the slot is empty. A slot already filled for
    /// another key is left as it is.
    pub fn store(&self, key: &[u8], sample: Bytes) {
        let _ = self
            .slot
            .get_or_init(|| (Bytes::copy_from_slice(key), sample));
    }
}

impl PartialEq for PacketShare {
    fn eq(&self, other: &Self) -> bool {
        self.slot.get() == other.slot.get()
    }
}

impl Eq for PacketShare {}

impl EncodedPacket {
    /// Creates a packet with no duration. Fails with [`MediaError::DtsAfterPts`] if
    /// `dts > pts`, which points to a timestamp bug in the encoder adapter.
    pub fn new(
        codec: CodecId,
        bitstream: BitstreamFormat,
        pts: Timestamp,
        dts: Timestamp,
        keyframe: bool,
        data: Bytes,
    ) -> Result<Self, MediaError> {
        if dts > pts {
            return Err(MediaError::DtsAfterPts);
        }
        Ok(Self {
            codec,
            bitstream,
            pts,
            dts,
            duration: None,
            keyframe,
            data,
            share: None,
        })
    }

    /// Allocates the shared slot for an Annex B H.264 or HEVC packet, once.
    /// Other packets are unchanged. Clones made after this call share the slot.
    pub fn attach_packet_share(&mut self) {
        if self.share.is_none()
            && self.bitstream == BitstreamFormat::AnnexB
            && matches!(self.codec, CodecId::H264 | CodecId::Hevc)
        {
            self.share = Some(Arc::new(PacketShare {
                slot: OnceLock::new(),
            }));
        }
    }

    /// The shared conversion slot, after [`Self::attach_packet_share`].
    pub fn packet_share(&self) -> Option<&Arc<PacketShare>> {
        self.share.as_ref()
    }

    /// Sets the duration.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// The codec.
    pub fn codec(&self) -> CodecId {
        self.codec
    }

    /// Framing of the payload.
    pub fn bitstream(&self) -> BitstreamFormat {
        self.bitstream
    }

    /// Presentation time in master-clock time.
    pub fn pts(&self) -> Timestamp {
        self.pts
    }

    /// Decode time in master-clock time; equal to `pts` without frame reordering.
    /// Always `dts <= pts`.
    pub fn dts(&self) -> Timestamp {
        self.dts
    }

    /// Duration, when known.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// True if decoding can start at this packet (IDR / key frame).
    pub fn keyframe(&self) -> bool {
        self.keyframe
    }

    /// The payload. Clone it (O(1)) or `slice` it to keep parts without copying.
    pub fn data(&self) -> &Bytes {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::video::{ColorSpace, PixelFormat, Size};

    #[test]
    fn codec_names_and_kinds() {
        assert_eq!(CodecId::Hevc.to_string(), "hevc");
        assert_eq!(CodecId::Opus.media_kind(), MediaKind::Audio);
        assert_eq!(CodecId::Av1.media_kind(), MediaKind::Video);
    }

    #[test]
    fn packets_share_payloads_without_copying() {
        let payload = Bytes::from(vec![0_u8, 0, 0, 1, 0x65, 1, 2, 3]);
        let packet = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::from_nanos(33),
            Timestamp::from_nanos(0),
            true,
            payload.clone(),
        )
        .map(|p| p.with_duration(Duration::from_millis(16)));
        let Ok(packet) = packet else {
            panic!("dts <= pts is valid");
        };
        let reordered = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::from_nanos(10),
            Timestamp::from_nanos(20),
            false,
            Bytes::new(),
        );
        assert_eq!(reordered.err(), Some(MediaError::DtsAfterPts));
        let fan_out = [packet.clone(), packet.clone()];
        for p in &fan_out {
            assert_eq!(p.data().as_ptr(), payload.as_ptr());
        }
        let nal = packet.data().slice(4..);
        assert_eq!(packet.duration(), Some(Duration::from_millis(16)));
        assert!(packet.keyframe() && packet.dts() <= packet.pts());
        assert_eq!(nal.as_ptr(), payload[4..].as_ptr());

        let descriptor = StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::new(),
            StreamParams::video(
                VideoFormat::new(
                    Size::new(1920, 1080),
                    PixelFormat::Nv12,
                    ColorSpace::BT709_LIMITED,
                ),
                FrameRate::FPS_60,
            ),
        );
        assert_eq!(descriptor.codec.media_kind(), MediaKind::Video);
    }

    #[test]
    fn a_shared_filter_is_one_sample_for_every_clone() {
        let mut packet = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::ZERO,
            Timestamp::ZERO,
            true,
            Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        )
        .unwrap();
        assert!(packet.packet_share().is_none());
        packet.attach_packet_share();
        let share = std::sync::Arc::clone(packet.packet_share().unwrap());
        let clone = packet.clone();
        assert!(std::sync::Arc::ptr_eq(
            &share,
            clone.packet_share().unwrap()
        ));
        let sample = Bytes::from_static(&[0, 0, 0, 1, 0x65]);
        share.store(b"sets", sample.clone());
        let got = clone.packet_share().unwrap().get(b"sets").unwrap();
        assert_eq!(got.as_ptr(), sample.as_ptr());
        assert!(clone.packet_share().unwrap().get(b"other").is_none());
        let raw = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Timestamp::ZERO,
            Timestamp::ZERO,
            true,
            Bytes::new(),
        )
        .unwrap();
        let mut raw = raw;
        raw.attach_packet_share();
        assert!(raw.packet_share().is_none());
    }
}
