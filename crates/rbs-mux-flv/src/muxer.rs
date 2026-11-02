//! The FLV muxer session.

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, EncodedPacket, MuxChunk, StreamDescriptor,
    StreamIndex, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

use crate::amf0::{self, Value};

const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;
const TAG_HEADER: usize = 11;
/// Video tag body prefix: frame type and codec, AVC packet type, composition time.
const VIDEO_PREFIX: usize = 5;
/// Audio tag body prefix: sound format byte, AAC packet type.
const AUDIO_PREFIX: usize = 2;
const CODEC_AVC: u8 = 7;
/// AAC, 44 kHz (always signalled for AAC), 16-bit, stereo.
const AUDIO_AAC: u8 = 0xaf;
const MAX_DATA_SIZE: usize = 0x00ff_ffff;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
}

struct Video {
    /// `None` for Annex B input, which is converted to 4-byte lengths.
    length_size: Option<u8>,
    /// avcC record for the sequence header.
    avcc: Vec<u8>,
}

pub(crate) struct FlvMuxer {
    kinds: Vec<Kind>,
    video: Option<Video>,
    /// The whole AudioSpecificConfig for the AAC sequence header (it may carry SBR/PS
    /// extensions beyond the first two bytes).
    audio: Option<Bytes>,
    metadata: Vec<(&'static str, Value)>,
    last_dts: Vec<Option<Timestamp>>,
    /// Master time of FLV timestamp 0: the first dts written.
    origin: Option<Timestamp>,
    last_chunk: Option<Timestamp>,
    /// How far before the first dts the FLV time origin lies, so streams that arrive
    /// slightly out of order across streams keep their timestamps.
    origin_lead: std::time::Duration,
    began: bool,
    finished: bool,
}

fn video_setup(stream: &StreamDescriptor) -> Result<Video, CreateError> {
    let invalid = |e| CreateError::InvalidConfig(format!("H.264 extradata: {e}"));
    let extradata = stream.extradata.as_ref();
    match stream.bitstream {
        BitstreamFormat::LengthPrefixed { length_size } => {
            if !matches!(length_size, 1 | 2 | 4) {
                return Err(CreateError::InvalidConfig(format!(
                    "NAL length size {length_size}"
                )));
            }
            let avcc = AvcConfig::parse(extradata).map_err(invalid)?;
            if avcc.pps.is_empty() {
                return Err(CreateError::InvalidConfig("avcC without PPS".into()));
            }
            if avcc.length_size != length_size {
                return Err(CreateError::InvalidConfig(format!(
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
                        return Err(CreateError::InvalidConfig(
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
        other => Err(CreateError::InvalidConfig(format!(
            "H.264 in {other:?} framing"
        ))),
    }
}

impl FlvMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        let mut kinds = Vec::with_capacity(config.streams.len());
        let mut video = None;
        let mut audio = None;
        let mut metadata = vec![("duration", Value::Number(0.0))];
        for stream in &config.streams {
            match stream.codec {
                CodecId::H264 => {
                    if video.is_some() {
                        return Err(CreateError::InvalidConfig(
                            "FLV carries one video stream".into(),
                        ));
                    }
                    video = Some(video_setup(stream)?);
                    kinds.push(Kind::Video);
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
                        return Err(CreateError::InvalidConfig(
                            "FLV carries one audio stream".into(),
                        ));
                    }
                    if stream.bitstream != BitstreamFormat::Raw {
                        return Err(CreateError::InvalidConfig(format!(
                            "AAC in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    let asc = AudioSpecificConfig::parse(&stream.extradata)
                        .map_err(|e| CreateError::InvalidConfig(format!("AAC extradata: {e}")))?;
                    audio = Some(stream.extradata.clone());
                    kinds.push(Kind::Audio);
                    metadata.push(("audiocodecid", Value::Number(10.0)));
                    metadata.push((
                        "audiosamplerate",
                        Value::Number(f64::from(asc.sample_rate())),
                    ));
                    metadata.push(("stereo", Value::Bool(asc.channels >= 2)));
                }
                other => {
                    return Err(CreateError::Unsupported {
                        what: format!("{other} in FLV"),
                    });
                }
            }
        }
        let too_big = video
            .as_ref()
            .is_some_and(|v| VIDEO_PREFIX + v.avcc.len() > MAX_DATA_SIZE)
            || audio
                .as_ref()
                .is_some_and(|a| AUDIO_PREFIX + a.len() > MAX_DATA_SIZE);
        if too_big {
            return Err(CreateError::InvalidConfig(
                "extradata larger than an FLV tag".into(),
            ));
        }
        Ok(Self {
            last_dts: vec![None; kinds.len()],
            kinds,
            video,
            audio,
            metadata,
            origin: None,
            last_chunk: None,
            origin_lead: config.max_buffered(),
            began: false,
            finished: false,
        })
    }
}

/// The 11-byte tag header: type, 24-bit data size, timestamp (low 24 bits, then the
/// extension byte), stream id 0.
fn tag_header(kind: u8, data_size: usize, ms: u32) -> [u8; TAG_HEADER] {
    let [ts3, ts2, ts1, ts0] = ms.to_be_bytes();
    let [_, s2, s1, s0] = (data_size as u32).to_be_bytes();
    [kind, s2, s1, s0, ts2, ts1, ts0, ts3, 0, 0, 0]
}

/// Appends a whole tag (header, body, PreviousTagSize).
fn whole_tag(out: &mut Vec<u8>, kind: u8, ms: u32, body: &[u8]) {
    out.extend_from_slice(&tag_header(kind, body.len(), ms));
    out.extend_from_slice(body);
    out.extend_from_slice(&((TAG_HEADER + body.len()) as u32).to_be_bytes());
}

/// Milliseconds from `origin` to `t`, which may be earlier (negative).
fn millis_since(t: Timestamp, origin: Timestamp) -> i64 {
    ((i128::from(t.as_nanos()) - i128::from(origin.as_nanos())).div_euclid(1_000_000)) as i64
}

/// Whether a NAL unit goes into an FLV video tag: access unit delimiters are dropped,
/// as other FLV writers do; empty units carry nothing.
fn keep_unit(unit: &[u8]) -> bool {
    !unit.is_empty() && nal::h264_type(unit) != Some(h264::AUD)
}

impl Muxer for FlvMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.began {
            return Err(MuxError::InvalidState("begin called twice"));
        }
        self.began = true;
        let flags = (u8::from(self.audio.is_some()) << 2) | u8::from(self.video.is_some());
        let mut header = vec![b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut body = Vec::new();
        amf0::string(&mut body, "onMetaData");
        amf0::ecma_array(&mut body, &self.metadata);
        whole_tag(&mut header, TAG_SCRIPT, 0, &body);
        if let Some(video) = &self.video {
            body.clear();
            body.extend_from_slice(&[0x10 | CODEC_AVC, 0, 0, 0, 0]);
            body.extend_from_slice(&video.avcc);
            whole_tag(&mut header, TAG_VIDEO, 0, &body);
        }
        if let Some(asc) = &self.audio {
            body.clear();
            body.extend_from_slice(&[AUDIO_AAC, 0]);
            body.extend_from_slice(asc);
            whole_tag(&mut header, TAG_AUDIO, 0, &body);
        }
        out.push(MuxChunk::new(ChunkKind::Header, Bytes::from(header)));
        Ok(())
    }

    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("write outside begin..finish"));
        }
        let index = stream.get();
        let Some(kind) = self.kinds.get(index).copied() else {
            return Err(MuxError::UnknownStream(stream.0));
        };
        let expected = match kind {
            Kind::Video => CodecId::H264,
            Kind::Audio => CodecId::Aac,
        };
        if packet.codec() != expected {
            return Err(MuxError::InvalidPacket(format!(
                "{} packet on a {expected} stream",
                packet.codec()
            )));
        }
        if packet.data().is_empty() {
            return Err(MuxError::InvalidPacket("empty packet".into()));
        }
        let dts = packet.dts();
        if self.last_dts[index].is_some_and(|last| dts < last) {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }

        let data = packet.data().as_ref();
        // Payload size in the tag and how to write it. Video units are re-framed with
        // the avcC length size (4 for Annex B input), without AUDs.
        let (payload_len, video_framing) = match (kind, &self.video) {
            (Kind::Video, Some(video)) => {
                let (input, out_size) = match (video.length_size, packet.bitstream()) {
                    (Some(size), BitstreamFormat::LengthPrefixed { length_size })
                        if size == length_size =>
                    {
                        (Some(size), usize::from(size))
                    }
                    (None, BitstreamFormat::AnnexB) => (None, 4),
                    (_, other) => {
                        return Err(MuxError::InvalidPacket(format!(
                            "{other:?} packet on this H.264 stream"
                        )));
                    }
                };
                let mut total = 0;
                match input {
                    Some(size) => {
                        for unit in nal::length_prefixed_units(data, size) {
                            let unit = unit.map_err(|_| {
                                MuxError::InvalidPacket("truncated length-prefixed NAL unit".into())
                            })?;
                            if keep_unit(unit) {
                                total += out_size + unit.len();
                            }
                        }
                    }
                    None => {
                        for unit in nal::annexb_units(data).filter(|u| keep_unit(u)) {
                            total += out_size + unit.len();
                        }
                    }
                }
                if total == 0 {
                    return Err(MuxError::InvalidPacket(
                        "H.264 packet without slices".into(),
                    ));
                }
                (total, Some((input, out_size)))
            }
            _ => {
                if packet.bitstream() != BitstreamFormat::Raw {
                    return Err(MuxError::InvalidPacket("AAC packets must be raw".into()));
                }
                (data.len(), None)
            }
        };
        let prefix = if kind == Kind::Video {
            VIDEO_PREFIX
        } else {
            AUDIO_PREFIX
        };
        let data_size = prefix + payload_len;
        if data_size > MAX_DATA_SIZE {
            return Err(MuxError::InvalidPacket(
                "packet larger than an FLV tag".into(),
            ));
        }
        let lead = self.origin_lead;
        let origin = *self.origin.get_or_insert(dts - lead);
        if dts < origin {
            return Err(MuxError::InvalidPacket(
                "dts more than the interleaving bound before the first packet".into(),
            ));
        }
        let dts_ms = millis_since(dts, origin);
        // FLV time is 32-bit milliseconds; it wraps after 49.7 days, which RTMP
        // servers and players handle.
        let ms = dts_ms as u32;
        // Composition time from the same rounded clock as the dts, signed 24-bit.
        let cts = (millis_since(packet.pts(), origin) - dts_ms).clamp(-0x80_0000, 0x7f_ffff) as i32;
        self.last_dts[index] = Some(dts);

        let keyframe = match kind {
            Kind::Video => packet.keyframe(),
            Kind::Audio => self.video.is_none(),
        };
        // One chunk per tag, sized exactly: a transport may drop or refuse whole tags
        // only, and the chunk is the only allocation for this packet.
        let mut tag = Vec::with_capacity(TAG_HEADER + data_size + 4);
        match kind {
            Kind::Video => {
                tag.extend_from_slice(&tag_header(TAG_VIDEO, data_size, ms));
                let frame = if packet.keyframe() { 0x10 } else { 0x20 };
                let [_, c2, c1, c0] = cts.to_be_bytes();
                tag.extend_from_slice(&[frame | CODEC_AVC, 1, c2, c1, c0]);
            }
            Kind::Audio => {
                tag.extend_from_slice(&tag_header(TAG_AUDIO, data_size, ms));
                tag.extend_from_slice(&[AUDIO_AAC, 1]);
            }
        }
        match video_framing {
            Some((input, out_size)) => {
                let mut put = |unit: &[u8]| {
                    let len = (unit.len() as u32).to_be_bytes();
                    tag.extend_from_slice(&len[4 - out_size..]);
                    tag.extend_from_slice(unit);
                };
                match input {
                    Some(size) => nal::length_prefixed_units(data, size)
                        .filter_map(Result::ok)
                        .filter(|u| keep_unit(u))
                        .for_each(&mut put),
                    None => nal::annexb_units(data)
                        .filter(|u| keep_unit(u))
                        .for_each(&mut put),
                }
            }
            None => tag.extend_from_slice(data),
        }
        tag.extend_from_slice(&((TAG_HEADER + data_size) as u32).to_be_bytes());
        debug_assert_eq!(tag.len(), tag.capacity());

        let timestamp = self.last_chunk.map_or(dts, |last| last.max(dts));
        self.last_chunk = Some(timestamp);
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe,
                timestamp,
            },
            Bytes::from(tag),
        ));
        Ok(())
    }

    fn finish(&mut self, _out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("finish outside begin..finish"));
        }
        // Live FLV: no trailer; duration and file size stay 0 in the metadata.
        self.finished = true;
        Ok(())
    }
}
