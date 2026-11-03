//! FLV tags and tag bodies for H.264 and AAC, shared by the FLV muxer and RTMP (an
//! RTMP audio, video or data message body is an FLV tag body).
//!
//! [`FlvTracks`] checks the stream descriptors once (session set-up, allocates) and
//! then plans and writes per-packet bodies without allocating: [`FlvTracks::plan`]
//! validates a packet and gives its exact body size, [`FlvTracks::write_body`]
//! appends that body. Access unit delimiters are left out of video bodies, and Annex B
//! input is re-framed with 4-byte lengths. Time (FLV milliseconds and composition
//! offsets) stays with the caller, which owns the clock origin.

use std::fmt;

use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor, StreamParams,
};
use rbs_core::time::Timestamp;

use crate::aac::AudioSpecificConfig;
use crate::amf0::{self, Value};
use crate::avc::AvcConfig;
use crate::nal::{self, h264};

/// Audio tag type.
pub const TAG_AUDIO: u8 = 8;
/// Video tag type.
pub const TAG_VIDEO: u8 = 9;
/// Script data tag type.
pub const TAG_SCRIPT: u8 = 18;
/// Bytes in a tag header.
pub const TAG_HEADER_LEN: usize = 11;
/// Bytes of the `PreviousTagSize` field after each tag.
pub const PREVIOUS_TAG_SIZE_LEN: usize = 4;
/// Largest tag body (24-bit data size).
pub const MAX_DATA_SIZE: usize = 0x00ff_ffff;
/// Video codec id of H.264 (AVC).
pub const CODEC_AVC: u8 = 7;
/// Sound format of AAC.
pub const SOUND_AAC: u8 = 10;
/// Video body prefix: frame type and codec, AVC packet type, composition time.
pub const VIDEO_PREFIX_LEN: usize = 5;
/// Audio body prefix: sound format byte, AAC packet type.
pub const AUDIO_PREFIX_LEN: usize = 2;
/// AAC, 44 kHz (always signalled for AAC), 16-bit, stereo.
const AUDIO_AAC: u8 = (SOUND_AAC << 4) | 0x0f;
const KEY_FRAME: u8 = 0x10;
const INTER_FRAME: u8 = 0x20;

/// The 11-byte tag header: type, 24-bit data size, timestamp (low 24 bits, then the
/// extension byte), stream id 0. `data_size` must be at most [`MAX_DATA_SIZE`].
pub fn tag_header(kind: u8, data_size: usize, ms: u32) -> [u8; TAG_HEADER_LEN] {
    let [ts3, ts2, ts1, ts0] = ms.to_be_bytes();
    let [_, s2, s1, s0] = (data_size as u32).to_be_bytes();
    [kind, s2, s1, s0, ts2, ts1, ts0, ts3, 0, 0, 0]
}

/// Appends the `PreviousTagSize` that follows a tag with a `data_size` body.
pub fn previous_tag_size(out: &mut Vec<u8>, data_size: usize) {
    out.extend_from_slice(&((TAG_HEADER_LEN + data_size) as u32).to_be_bytes());
}

/// Appends a whole tag: header, body, `PreviousTagSize`.
pub fn write_tag(out: &mut Vec<u8>, kind: u8, ms: u32, body: &[u8]) {
    out.extend_from_slice(&tag_header(kind, body.len(), ms));
    out.extend_from_slice(body);
    previous_tag_size(out, body.len());
}

/// Milliseconds from `origin` to `t`, rounded down; negative when `t` is earlier.
pub fn millis_since(t: Timestamp, origin: Timestamp) -> i64 {
    ((i128::from(t.as_nanos()) - i128::from(origin.as_nanos())).div_euclid(1_000_000)) as i64
}

/// Why the stream descriptors cannot be carried in FLV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A codec FLV (here) does not carry.
    Unsupported(String),
    /// The descriptors are wrong or incomplete.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unsupported(what) => write!(f, "unsupported: {what}"),
            ConfigError::Invalid(what) => write!(f, "invalid: {what}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Why a packet cannot become an FLV body.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// No stream with this index.
    UnknownStream,
    /// The packet codec is not the stream's.
    WrongCodec,
    /// The packet framing is not the stream's.
    WrongFraming,
    /// The packet has no data.
    Empty,
    /// A length-prefixed NAL unit runs past the end of the packet.
    Truncated,
    /// An H.264 packet with no NAL unit to carry (only delimiters).
    NoSlices,
    /// The body would exceed [`MAX_DATA_SIZE`].
    TooLarge,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PacketError::UnknownStream => "unknown stream",
            PacketError::WrongCodec => "packet codec differs from the stream's",
            PacketError::WrongFraming => "packet framing differs from the stream's",
            PacketError::Empty => "empty packet",
            PacketError::Truncated => "truncated length-prefixed NAL unit",
            PacketError::NoSlices => "H.264 packet without slices",
            PacketError::TooLarge => "packet larger than an FLV tag",
        })
    }
}

impl std::error::Error for PacketError {}

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// The H.264 stream.
    Video,
    /// The AAC stream.
    Audio,
}

/// A packet checked by [`FlvTracks::plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPlan {
    /// The stream's kind.
    pub kind: TrackKind,
    /// Exact body size: prefix and payload.
    pub data_size: usize,
}

impl BodyPlan {
    /// The tag type for this body.
    pub fn tag_type(&self) -> u8 {
        match self.kind {
            TrackKind::Video => TAG_VIDEO,
            TrackKind::Audio => TAG_AUDIO,
        }
    }
}

#[derive(Debug, Clone)]
struct Video {
    /// `None` for Annex B input, which is converted to 4-byte lengths.
    length_size: Option<u8>,
    /// avcC record for the sequence header.
    avcc: Vec<u8>,
}

impl Video {
    fn out_length_size(&self) -> usize {
        self.length_size.map_or(4, usize::from)
    }
}

/// The streams of one FLV output: at most one H.264 and one AAC stream.
#[derive(Debug, Clone)]
pub struct FlvTracks {
    kinds: Vec<TrackKind>,
    video: Option<Video>,
    /// The whole AudioSpecificConfig (it may carry SBR/PS extensions beyond the first
    /// two bytes).
    audio: Option<Bytes>,
    metadata: Vec<(&'static str, Value<'static>)>,
}

/// Whether a NAL unit goes into a video body: access unit delimiters are dropped, as
/// other FLV writers do; empty units carry nothing.
fn keep_unit(unit: &[u8]) -> bool {
    !unit.is_empty() && nal::h264_type(unit) != Some(h264::AUD)
}

fn video_setup(stream: &StreamDescriptor) -> Result<Video, ConfigError> {
    let invalid = |e| ConfigError::Invalid(format!("H.264 extradata: {e}"));
    let extradata = stream.extradata.as_ref();
    match stream.bitstream {
        BitstreamFormat::LengthPrefixed { length_size } => {
            if !matches!(length_size, 1 | 2 | 4) {
                return Err(ConfigError::Invalid(format!(
                    "NAL length size {length_size}"
                )));
            }
            let avcc = AvcConfig::parse(extradata).map_err(invalid)?;
            if avcc.pps.is_empty() {
                return Err(ConfigError::Invalid("avcC without PPS".into()));
            }
            if avcc.length_size != length_size {
                return Err(ConfigError::Invalid(format!(
                    "packets use {length_size}-byte NAL lengths, avcC says {}",
                    avcc.length_size
                )));
            }
            Ok(Video {
                length_size: Some(length_size),
                avcc: extradata.to_vec(),
            })
        }
        BitstreamFormat::AnnexB => {
            let mut avcc = Vec::new();
            if extradata.first() == Some(&1) {
                let parsed = AvcConfig::parse(extradata).map_err(invalid)?;
                AvcConfig::build(&parsed.sps, &parsed.pps, &mut avcc).map_err(invalid)?;
            } else {
                match AvcConfig::from_annexb(extradata, &mut avcc) {
                    Some(result) => result.map_err(invalid)?,
                    None => {
                        return Err(ConfigError::Invalid(
                            "Annex B H.264 in FLV needs SPS and PPS in the extradata".into(),
                        ));
                    }
                }
            }
            Ok(Video {
                length_size: None,
                avcc,
            })
        }
        other => Err(ConfigError::Invalid(format!("H.264 in {other:?} framing"))),
    }
}

impl FlvTracks {
    /// Checks the streams and prepares sequence headers and metadata.
    pub fn new(streams: &[StreamDescriptor]) -> Result<Self, ConfigError> {
        if streams.is_empty() {
            return Err(ConfigError::Invalid("no streams".into()));
        }
        let mut kinds = Vec::with_capacity(streams.len());
        let mut video = None;
        let mut audio = None;
        let mut metadata = vec![("duration", Value::Number(0.0))];
        for stream in streams {
            match stream.codec {
                CodecId::H264 => {
                    if video.is_some() {
                        return Err(ConfigError::Invalid("FLV carries one video stream".into()));
                    }
                    video = Some(video_setup(stream)?);
                    kinds.push(TrackKind::Video);
                    if let StreamParams::Video {
                        format, frame_rate, ..
                    } = stream.params
                    {
                        metadata.push(("width", Value::Number(f64::from(format.size.width))));
                        metadata.push(("height", Value::Number(f64::from(format.size.height))));
                        metadata
                            .push(("framerate", Value::Number(frame_rate.per_second().as_f64())));
                    }
                    metadata.push(("videocodecid", Value::Number(f64::from(CODEC_AVC))));
                }
                CodecId::Aac => {
                    if audio.is_some() {
                        return Err(ConfigError::Invalid("FLV carries one audio stream".into()));
                    }
                    if stream.bitstream != BitstreamFormat::Raw {
                        return Err(ConfigError::Invalid(format!(
                            "AAC in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    let asc = AudioSpecificConfig::parse(&stream.extradata)
                        .map_err(|e| ConfigError::Invalid(format!("AAC extradata: {e}")))?;
                    audio = Some(stream.extradata.clone());
                    kinds.push(TrackKind::Audio);
                    metadata.push(("audiocodecid", Value::Number(f64::from(SOUND_AAC))));
                    metadata.push((
                        "audiosamplerate",
                        Value::Number(f64::from(asc.sample_rate())),
                    ));
                    metadata.push(("stereo", Value::Bool(asc.channels >= 2)));
                }
                other => return Err(ConfigError::Unsupported(format!("{other} in FLV"))),
            }
        }
        let too_big = video
            .as_ref()
            .is_some_and(|v| VIDEO_PREFIX_LEN + v.avcc.len() > MAX_DATA_SIZE)
            || audio
                .as_ref()
                .is_some_and(|a| AUDIO_PREFIX_LEN + a.len() > MAX_DATA_SIZE);
        if too_big {
            return Err(ConfigError::Invalid(
                "extradata larger than an FLV tag".into(),
            ));
        }
        Ok(Self {
            kinds,
            video,
            audio,
            metadata,
        })
    }

    /// Whether there is a video stream.
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Whether there is an audio stream.
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// The kind of stream `index`, if it exists.
    pub fn kind(&self, index: usize) -> Option<TrackKind> {
        self.kinds.get(index).copied()
    }

    /// Appends the `onMetaData` script body: the name, then an ECMA array with size,
    /// frame rate, codec ids and sample rate (duration 0, as for live streams).
    pub fn metadata_body(&self, out: &mut Vec<u8>) {
        amf0::string(out, "onMetaData");
        amf0::ecma_array(out, &self.metadata);
    }

    /// Appends the AVC sequence header body; returns `false` without video.
    pub fn video_sequence_header(&self, out: &mut Vec<u8>) -> bool {
        let Some(video) = &self.video else {
            return false;
        };
        out.extend_from_slice(&[KEY_FRAME | CODEC_AVC, 0, 0, 0, 0]);
        out.extend_from_slice(&video.avcc);
        true
    }

    /// Appends the AAC sequence header body; returns `false` without audio.
    pub fn audio_sequence_header(&self, out: &mut Vec<u8>) -> bool {
        let Some(asc) = &self.audio else {
            return false;
        };
        out.extend_from_slice(&[AUDIO_AAC, 0]);
        out.extend_from_slice(asc);
        true
    }

    /// Hot path. Checks a packet for stream `index` and gives its exact body size. Reads
    /// the NAL unit lengths of H.264 packets; never allocates.
    pub fn plan(&self, index: usize, packet: &EncodedPacket) -> Result<BodyPlan, PacketError> {
        let kind = self.kind(index).ok_or(PacketError::UnknownStream)?;
        let expected = match kind {
            TrackKind::Video => CodecId::H264,
            TrackKind::Audio => CodecId::Aac,
        };
        if packet.codec() != expected {
            return Err(PacketError::WrongCodec);
        }
        let data = packet.data().as_ref();
        if data.is_empty() {
            return Err(PacketError::Empty);
        }
        let payload = match (kind, &self.video) {
            (TrackKind::Video, Some(video)) => {
                let out_size = video.out_length_size();
                let mut total = 0;
                match (video.length_size, packet.bitstream()) {
                    (Some(size), BitstreamFormat::LengthPrefixed { length_size })
                        if size == length_size =>
                    {
                        for unit in nal::length_prefixed_units(data, size) {
                            let unit = unit.map_err(|_| PacketError::Truncated)?;
                            if keep_unit(unit) {
                                total += out_size + unit.len();
                            }
                        }
                    }
                    (None, BitstreamFormat::AnnexB) => {
                        for unit in nal::annexb_units(data).filter(|u| keep_unit(u)) {
                            total += out_size + unit.len();
                        }
                    }
                    _ => return Err(PacketError::WrongFraming),
                }
                if total == 0 {
                    return Err(PacketError::NoSlices);
                }
                VIDEO_PREFIX_LEN + total
            }
            _ => {
                if packet.bitstream() != BitstreamFormat::Raw {
                    return Err(PacketError::WrongFraming);
                }
                AUDIO_PREFIX_LEN + data.len()
            }
        };
        if payload > MAX_DATA_SIZE {
            return Err(PacketError::TooLarge);
        }
        Ok(BodyPlan {
            kind,
            data_size: payload,
        })
    }

    /// Hot path. Appends the body of a packet that [`plan`](Self::plan) accepted, exactly
    /// `plan.data_size` bytes. `cts_ms` is the composition offset (pts minus dts) in
    /// milliseconds, clamped to signed 24 bits; audio ignores it. Allocates only if
    /// `out` lacks capacity. `plan` must come from this packet; a plan from another
    /// packet gives a body whose size differs from `plan.data_size`.
    pub fn write_body(
        &self,
        plan: BodyPlan,
        packet: &EncodedPacket,
        cts_ms: i64,
        out: &mut Vec<u8>,
    ) {
        let data = packet.data().as_ref();
        let start = out.len();
        match (plan.kind, &self.video) {
            (TrackKind::Video, Some(video)) => {
                let cts = cts_ms.clamp(-0x80_0000, 0x7f_ffff) as i32;
                let frame = if packet.keyframe() {
                    KEY_FRAME
                } else {
                    INTER_FRAME
                };
                let [_, c2, c1, c0] = cts.to_be_bytes();
                out.extend_from_slice(&[frame | CODEC_AVC, 1, c2, c1, c0]);
                let out_size = video.out_length_size();
                let mut put = |unit: &[u8]| {
                    let len = (unit.len() as u32).to_be_bytes();
                    out.extend_from_slice(&len[4 - out_size..]);
                    out.extend_from_slice(unit);
                };
                match video.length_size {
                    Some(size) => nal::length_prefixed_units(data, size)
                        .filter_map(Result::ok)
                        .filter(|u| keep_unit(u))
                        .for_each(&mut put),
                    None => nal::annexb_units(data)
                        .filter(|u| keep_unit(u))
                        .for_each(&mut put),
                }
            }
            _ => {
                out.extend_from_slice(&[AUDIO_AAC, 1]);
                out.extend_from_slice(data);
            }
        }
        debug_assert_eq!(
            out.len() - start,
            plan.data_size,
            "plan from another packet"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::nal::h264::{IDR, SPS};
    use rbs_core::media::AudioFormat;

    fn avcc() -> Vec<u8> {
        let mut avcc = Vec::new();
        AvcConfig::build(
            &[&[0x67, 0x42, 0xc0, 0x1f, 0xda][..]],
            &[&[0x68, 0xce, 0x3c, 0x80][..]],
            &mut avcc,
        )
        .unwrap();
        avcc
    }

    fn video(bitstream: BitstreamFormat, extradata: Vec<u8>) -> StreamDescriptor {
        StreamDescriptor::new(
            CodecId::H264,
            bitstream,
            Bytes::from(extradata),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        )
    }

    fn packet(bitstream: BitstreamFormat, keyframe: bool, data: Vec<u8>) -> EncodedPacket {
        let t = Timestamp::from_nanos(0);
        EncodedPacket::new(CodecId::H264, bitstream, t, t, keyframe, Bytes::from(data)).unwrap()
    }

    #[test]
    fn tag_header_layout() {
        assert_eq!(
            tag_header(TAG_VIDEO, 0x01_0203, 0x0a0b_0c0d),
            [9, 1, 2, 3, 0x0b, 0x0c, 0x0d, 0x0a, 0, 0, 0]
        );
        let mut out = Vec::new();
        write_tag(&mut out, TAG_AUDIO, 0, &[1, 2]);
        assert_eq!(out.len(), TAG_HEADER_LEN + 2 + PREVIOUS_TAG_SIZE_LEN);
        assert_eq!(&out[13..], &13u32.to_be_bytes());
    }

    #[test]
    fn millis_round_down() {
        let origin = Timestamp::from_nanos(1_000_000_000);
        assert_eq!(
            millis_since(Timestamp::from_nanos(1_001_999_999), origin),
            1
        );
        assert_eq!(millis_since(Timestamp::from_nanos(999_999_999), origin), -1);
    }

    #[test]
    fn annexb_bodies_are_reframed_without_delimiters() {
        let tracks = FlvTracks::new(&[video(BitstreamFormat::AnnexB, avcc())]).unwrap();
        let mut header = Vec::new();
        assert!(tracks.video_sequence_header(&mut header));
        assert_eq!(&header[..5], &[0x17, 0, 0, 0, 0]);
        assert!(!tracks.audio_sequence_header(&mut header));

        let data = vec![0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, SPS, 1, 0, 0, 1, IDR, 2, 3];
        let p = packet(BitstreamFormat::AnnexB, true, data);
        let plan = tracks.plan(0, &p).unwrap();
        let mut body = Vec::new();
        tracks.write_body(plan, &p, -1, &mut body);
        assert_eq!(body.len(), plan.data_size);
        assert_eq!(
            body,
            [
                0x17, 1, 0xff, 0xff, 0xff, 0, 0, 0, 2, SPS, 1, 0, 0, 0, 3, IDR, 2, 3
            ]
        );
    }

    #[test]
    fn refuses_bad_packets() {
        let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
        let tracks = FlvTracks::new(&[video(lp, avcc())]).unwrap();
        assert_eq!(
            tracks.plan(1, &packet(lp, true, vec![0, 0, 0, 1, IDR])),
            Err(PacketError::UnknownStream)
        );
        assert_eq!(
            tracks.plan(
                0,
                &packet(BitstreamFormat::AnnexB, true, vec![0, 0, 1, IDR])
            ),
            Err(PacketError::WrongFraming)
        );
        assert_eq!(
            tracks.plan(0, &packet(lp, true, vec![0, 0, 0, 9, IDR])),
            Err(PacketError::Truncated)
        );
        assert_eq!(
            tracks.plan(0, &packet(lp, true, vec![0, 0, 0, 2, 0x09, 0xf0])),
            Err(PacketError::NoSlices)
        );
    }

    #[test]
    fn refuses_bad_configs() {
        assert!(matches!(FlvTracks::new(&[]), Err(ConfigError::Invalid(_))));
        let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
        assert!(matches!(
            FlvTracks::new(&[video(lp, avcc()), video(lp, avcc())]),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            FlvTracks::new(&[video(BitstreamFormat::AnnexB, Vec::new())]),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            FlvTracks::new(&[video(
                BitstreamFormat::LengthPrefixed { length_size: 2 },
                avcc()
            )]),
            Err(ConfigError::Invalid(_))
        ));
    }
}
