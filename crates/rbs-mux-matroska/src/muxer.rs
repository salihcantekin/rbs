//! The Matroska muxer session.

use std::time::Duration;

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, EncodedPacket, MuxChunk, StreamDescriptor,
    StreamIndex, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

use crate::ebml;

// Element ids (Matroska specification, RFC 9559).
const EBML_HEADER: u32 = 0x1a45_dfa3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42f7;
const EBML_MAX_ID_LENGTH: u32 = 0x42f2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42f3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_a966;
const TIMESTAMP_SCALE: u32 = 0x2a_d7b1;
const MUXING_APP: u32 = 0x4d80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_ae6b;
const TRACK_ENTRY: u32 = 0xae;
const TRACK_NUMBER: u32 = 0xd7;
const TRACK_UID: u32 = 0x73c5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9c;
const LANGUAGE: u32 = 0x22_b59c;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63a2;
const DEFAULT_DURATION: u32 = 0x23_e383;
const VIDEO: u32 = 0xe0;
const PIXEL_WIDTH: u32 = 0xb0;
const PIXEL_HEIGHT: u32 = 0xba;
const AUDIO: u32 = 0xe1;
const SAMPLING_FREQUENCY: u32 = 0xb5;
const CHANNELS: u32 = 0x9f;
const CLUSTER: u32 = 0x1f43_b675;
const CLUSTER_TIMESTAMP: u32 = 0xe7;
const SIMPLE_BLOCK: u32 = 0xa3;

/// Timestamps are in milliseconds (`TimestampScale` 1 000 000 ns), the common choice.
const SCALE_NS: u64 = 1_000_000;
/// Cluster: id (4) + 8-byte size + Timestamp element (1 + 1 + 8).
const CLUSTER_FIXED: usize = 4 + 8 + 10;
/// SimpleBlock: id (1) + 4-byte size + track number (1) + timestamp (2) + flags (1).
const BLOCK_FIXED: usize = 1 + 4 + 1 + 2 + 1;
/// Largest block payload: a 4-byte size field holds up to 2^28 - 2.
const MAX_SAMPLE_BYTES: usize = (1 << 28) - 16;
/// Payload bytes and blocks after which a cluster ends early, bounding its memory.
const MAX_CLUSTER_BYTES: usize = 1 << 30;
const MAX_CLUSTER_BLOCKS: usize = 1 << 16;
/// Block timestamps are signed 16-bit milliseconds relative to the cluster: a
/// cluster spans at most this long, and pts may trail dts by at most
/// `MAX_COMPOSITION_DELAY`, so every relative timestamp fits.
const MAX_CLUSTER_SPAN: Duration = Duration::from_secs(20);
const MAX_COMPOSITION_DELAY: Duration = Duration::from_secs(10);
/// Track numbers are written as one-byte variable-length integers.
const MAX_TRACKS: usize = 126;

enum Kind {
    /// H.264; `length_size` is `None` for Annex B input, converted to 4-byte lengths.
    Video {
        length_size: Option<u8>,
    },
    Audio,
}

enum Payload {
    /// The packet's own bytes.
    Shared(Bytes),
    /// `len` bytes of the muxer's staging buffer from `start` (converted Annex B input).
    Staged { start: usize },
}

struct Block {
    track: u8,
    payload: Payload,
    len: u32,
    dts: Timestamp,
    pts: Timestamp,
    keyframe: bool,
    /// Arrival order, so blocks with equal dts keep it after sorting.
    seq: u32,
}

struct Track {
    codec: CodecId,
    bitstream: BitstreamFormat,
    kind: Kind,
    /// SPS and PPS of the codec private data. In-band parameter sets must be one of
    /// these: a changed set would make the rest of the file undecodable.
    parameter_sets: Vec<Vec<u8>>,
    last_dts: Option<Timestamp>,
    /// Whether this track has a block in the cluster being collected.
    in_cluster: bool,
}

pub(crate) struct MatroskaMuxer {
    tracks: Vec<Track>,
    has_video: bool,
    header: Vec<u8>,
    cluster_span: Duration,
    /// How far before the first dts the time origin lies, so streams that arrive
    /// slightly out of order keep their timestamps.
    origin_lead: Duration,
    /// Time 0 in nanoseconds of the master clock (signed: a first dts closer than the
    /// lead to the clock's zero still works).
    origin: Option<i128>,
    blocks: Vec<Block>,
    /// Converted Annex B payloads of `blocks`; bytes from `committed` on belong to
    /// the packet being written.
    staging: Vec<u8>,
    committed: usize,
    /// Earliest and latest dts of the blocks in the cluster being collected.
    cluster_start: Option<Timestamp>,
    cluster_end: Timestamp,
    /// Latest dts written to any stream: packets more than the interleaving bound
    /// behind it are refused (port invariant 2).
    newest_dts: Option<Timestamp>,
    cluster_bytes: usize,
    seq: u32,
    last_chunk: Option<Timestamp>,
    began: bool,
    finished: bool,
}

fn nanos(t: Timestamp) -> i128 {
    i128::from(t.as_nanos())
}

/// Whether an Annex B unit goes into a block: parameter sets live in the codec
/// private data and access unit delimiters are not needed.
fn keep_unit(unit: &[u8]) -> bool {
    !unit.is_empty() && !is_parameter_set(unit) && nal::h264_type(unit) != Some(h264::AUD)
}

fn is_parameter_set(unit: &[u8]) -> bool {
    matches!(nal::h264_type(unit), Some(h264::SPS | h264::PPS))
}

/// Refuses an in-band SPS or PPS that differs from the codec private data's.
fn check_parameter_set(track: &Track, unit: &[u8]) -> Result<(), MuxError> {
    if is_parameter_set(unit) && !track.parameter_sets.iter().any(|p| p == unit) {
        return Err(MuxError::InvalidPacket(
            "H.264 parameter sets changed; Matroska needs a new output".into(),
        ));
    }
    Ok(())
}

fn avcc_for(stream: &StreamDescriptor) -> Result<(Vec<u8>, Option<u8>), CreateError> {
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
            if avcc.length_size != length_size || avcc.pps.is_empty() {
                return Err(CreateError::InvalidConfig(
                    "avcC must have a PPS and match the packets' NAL length size".into(),
                ));
            }
            Ok((extradata.to_vec(), Some(length_size)))
        }
        BitstreamFormat::AnnexB => {
            let mut avcc = Vec::new();
            if extradata.first() == Some(&1) {
                let parsed = AvcConfig::parse(extradata).map_err(invalid)?;
                AvcConfig::build(&parsed.sps, &parsed.pps, &mut avcc).map_err(invalid)?;
            } else if let Some(result) = AvcConfig::from_annexb(extradata, &mut avcc) {
                result.map_err(invalid)?;
            } else {
                return Err(CreateError::InvalidConfig(
                    "Annex B H.264 in Matroska needs SPS and PPS in the extradata".into(),
                ));
            }
            Ok((avcc, None))
        }
        other => Err(CreateError::InvalidConfig(format!(
            "H.264 in {other:?} framing"
        ))),
    }
}

impl MatroskaMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        if config.streams.len() > MAX_TRACKS {
            return Err(CreateError::InvalidConfig(format!(
                "more than {MAX_TRACKS} streams"
            )));
        }
        let mut header = Vec::new();
        let ebml_header = ebml::open(&mut header, EBML_HEADER);
        ebml::uint(&mut header, EBML_VERSION, 1);
        ebml::uint(&mut header, EBML_READ_VERSION, 1);
        ebml::uint(&mut header, EBML_MAX_ID_LENGTH, 4);
        ebml::uint(&mut header, EBML_MAX_SIZE_LENGTH, 8);
        ebml::bytes(&mut header, DOC_TYPE, b"matroska");
        ebml::uint(&mut header, DOC_TYPE_VERSION, 4);
        ebml::uint(&mut header, DOC_TYPE_READ_VERSION, 2);
        ebml::close(&mut header, ebml_header);
        // Live: the Segment's size is unknown, so the file is valid at every Cluster
        // boundary (`mux.live`).
        ebml::id(&mut header, SEGMENT);
        header.extend_from_slice(&ebml::UNKNOWN_SIZE);
        let info = ebml::open(&mut header, INFO);
        ebml::uint(&mut header, TIMESTAMP_SCALE, SCALE_NS);
        ebml::bytes(&mut header, MUXING_APP, b"RBS");
        ebml::bytes(&mut header, WRITING_APP, b"RBS");
        ebml::close(&mut header, info);

        let mut tracks = Vec::with_capacity(config.streams.len());
        let tracks_element = ebml::open(&mut header, TRACKS);
        for (i, stream) in config.streams.iter().enumerate() {
            let number = i as u64 + 1;
            let entry = ebml::open(&mut header, TRACK_ENTRY);
            ebml::uint(&mut header, TRACK_NUMBER, number);
            ebml::uint(&mut header, TRACK_UID, number);
            ebml::uint(&mut header, FLAG_LACING, 0);
            ebml::bytes(&mut header, LANGUAGE, b"und");
            match (stream.codec, stream.params) {
                (
                    CodecId::H264,
                    StreamParams::Video {
                        format, frame_rate, ..
                    },
                ) => {
                    if tracks
                        .iter()
                        .any(|t: &Track| matches!(t.kind, Kind::Video { .. }))
                    {
                        // A keyframe chunk must be an entry point for every stream; with
                        // two video tracks whose GOPs differ, clusters rarely are.
                        return Err(CreateError::Unsupported {
                            what: "more than one video stream in Matroska".into(),
                        });
                    }
                    let (avcc, length_size) = avcc_for(stream)?;
                    let parameter_sets = AvcConfig::parse(&avcc)
                        .map(|c| c.sps.iter().chain(&c.pps).map(|p| p.to_vec()).collect())
                        .map_err(|e| CreateError::InvalidConfig(format!("H.264 extradata: {e}")))?;
                    ebml::uint(&mut header, TRACK_TYPE, 1);
                    ebml::bytes(&mut header, CODEC_ID, b"V_MPEG4/ISO/AVC");
                    ebml::bytes(&mut header, CODEC_PRIVATE, &avcc);
                    let interval =
                        u64::try_from(frame_rate.frame_duration().as_nanos()).unwrap_or(u64::MAX);
                    // 0 is not allowed (RFC 9559); leave it out for absurd frame rates.
                    if interval > 0 {
                        ebml::uint(&mut header, DEFAULT_DURATION, interval);
                    }
                    let video = ebml::open(&mut header, VIDEO);
                    ebml::uint(&mut header, PIXEL_WIDTH, u64::from(format.size.width));
                    ebml::uint(&mut header, PIXEL_HEIGHT, u64::from(format.size.height));
                    ebml::close(&mut header, video);
                    tracks.push(Track {
                        codec: CodecId::H264,
                        bitstream: stream.bitstream,
                        kind: Kind::Video { length_size },
                        parameter_sets,
                        last_dts: None,
                        in_cluster: false,
                    });
                }
                (CodecId::Aac, StreamParams::Audio { format, .. }) => {
                    if stream.bitstream != BitstreamFormat::Raw {
                        return Err(CreateError::InvalidConfig(format!(
                            "AAC in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    AudioSpecificConfig::parse(&stream.extradata)
                        .map_err(|e| CreateError::InvalidConfig(format!("AAC extradata: {e}")))?;
                    ebml::uint(&mut header, TRACK_TYPE, 2);
                    ebml::bytes(&mut header, CODEC_ID, b"A_AAC");
                    ebml::bytes(&mut header, CODEC_PRIVATE, &stream.extradata);
                    let audio = ebml::open(&mut header, AUDIO);
                    ebml::float(
                        &mut header,
                        SAMPLING_FREQUENCY,
                        f64::from(format.sample_rate.hz()),
                    );
                    ebml::uint(&mut header, CHANNELS, format.channels.channels() as u64);
                    ebml::close(&mut header, audio);
                    tracks.push(Track {
                        codec: CodecId::Aac,
                        bitstream: BitstreamFormat::Raw,
                        kind: Kind::Audio,
                        parameter_sets: Vec::new(),
                        last_dts: None,
                        in_cluster: false,
                    });
                }
                (CodecId::H264 | CodecId::Aac, _) => {
                    return Err(CreateError::InvalidConfig(format!(
                        "{} stream with {:?} parameters",
                        stream.codec, stream.params
                    )));
                }
                (other, _) => {
                    return Err(CreateError::Unsupported {
                        what: format!("{other} in Matroska"),
                    });
                }
            }
            ebml::close(&mut header, entry);
        }
        ebml::close(&mut header, tracks_element);

        let lead = config.max_buffered();
        Ok(Self {
            has_video: tracks.iter().any(|t| matches!(t.kind, Kind::Video { .. })),
            tracks,
            header,
            cluster_span: lead.min(MAX_CLUSTER_SPAN),
            // The interleaving bound, capped like the span: block timestamps are 16-bit,
            // and players show the lead as the file's start time.
            origin_lead: lead.min(MAX_CLUSTER_SPAN),
            origin: None,
            blocks: Vec::new(),
            staging: Vec::new(),
            committed: 0,
            cluster_start: None,
            cluster_end: Timestamp::from_nanos(0),
            newest_dts: None,
            cluster_bytes: 0,
            seq: 0,
            last_chunk: None,
            began: false,
            finished: false,
        })
    }

    /// Milliseconds from the origin to `t`, rounded to the nearest. `write` refuses
    /// timestamps before the origin.
    fn millis(&self, t: Timestamp) -> i64 {
        let since = nanos(t) - self.origin.unwrap_or_else(|| nanos(t));
        i64::try_from((since + i128::from(SCALE_NS / 2)).div_euclid(i128::from(SCALE_NS)))
            .unwrap_or(i64::MAX)
    }

    /// Emits the collected blocks as one Cluster chunk, in decode order.
    fn flush(&mut self, out: &mut Vec<MuxChunk>) {
        let Some(start) = self.cluster_start.take() else {
            return;
        };
        // Decode order across tracks; arrival order breaks ties. Unstable sort: it does
        // not allocate, and `seq` makes the keys unique.
        self.blocks.sort_unstable_by_key(|b| (b.dts, b.seq));
        // The cluster timestamp is the earliest block pts, so relative timestamps are
        // non-negative and below the span plus composition delay (< 32.768 s).
        let base = self
            .blocks
            .iter()
            .map(|b| self.millis(b.pts))
            .min()
            .unwrap_or(0);
        let size = CLUSTER_FIXED
            + self
                .blocks
                .iter()
                .map(|b| BLOCK_FIXED + b.len as usize)
                .sum::<usize>();
        let mut chunk = Vec::with_capacity(size);
        let cluster = ebml::open(&mut chunk, CLUSTER);
        ebml::uint(
            &mut chunk,
            CLUSTER_TIMESTAMP,
            u64::try_from(base).unwrap_or(0),
        );
        for b in &self.blocks {
            ebml::id(&mut chunk, SIMPLE_BLOCK);
            // At most MAX_SAMPLE_BYTES, so the 4-byte size holds it.
            ebml::size4(&mut chunk, 4 + b.len);
            ebml::size1(&mut chunk, b.track);
            // 0 <= relative < span (<= 20 s) + composition delay (<= 10 s) + rounding,
            // which fits an i16 of milliseconds: `write` keeps the dts range of a cluster
            // below the span and refuses larger composition delays.
            let relative = self.millis(b.pts) - base;
            debug_assert!((0..=i64::from(i16::MAX)).contains(&relative));
            let relative = i16::try_from(relative).unwrap_or(i16::MAX);
            chunk.extend_from_slice(&relative.to_be_bytes());
            chunk.push(if b.keyframe { 0x80 } else { 0 });
            match &b.payload {
                Payload::Shared(data) => chunk.extend_from_slice(data),
                Payload::Staged { start } => {
                    chunk.extend_from_slice(&self.staging[*start..*start + b.len as usize]);
                }
            }
        }
        ebml::close(&mut chunk, cluster);
        debug_assert_eq!(chunk.len(), chunk.capacity());

        // An entry point for every stream: audio blocks all are, and there is at most
        // one video track (checked at create), whose first block must be a keyframe.
        let keyframe = if self.has_video {
            self.blocks
                .iter()
                .find(|b| {
                    matches!(
                        self.tracks[usize::from(b.track) - 1].kind,
                        Kind::Video { .. }
                    )
                })
                .is_some_and(|b| b.keyframe)
        } else {
            true
        };
        self.blocks.clear();
        // Keep the packet being written (after `committed`) for the next cluster.
        self.staging.copy_within(self.committed.., 0);
        self.staging.truncate(self.staging.len() - self.committed);
        self.committed = 0;
        self.cluster_bytes = 0;
        self.seq = 0;
        for track in &mut self.tracks {
            track.in_cluster = false;
        }
        // Chunk timestamps never go back: a cluster can start earlier than the one
        // before when a stream arrives late (within the interleaving bound).
        let timestamp = self.last_chunk.map_or(start, |last| last.max(start));
        self.last_chunk = Some(timestamp);
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe,
                timestamp,
            },
            Bytes::from(chunk),
        ));
    }
}

impl Muxer for MatroskaMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.began {
            return Err(MuxError::InvalidState("begin called twice"));
        }
        self.began = true;
        out.push(MuxChunk::new(
            ChunkKind::Header,
            Bytes::from(std::mem::take(&mut self.header)),
        ));
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
        let Some(track) = self.tracks.get(index) else {
            return Err(MuxError::UnknownStream(stream.0));
        };
        if packet.codec() != track.codec || packet.bitstream() != track.bitstream {
            return Err(MuxError::InvalidPacket(format!(
                "{} {:?} packet on a {} {:?} stream",
                packet.codec(),
                packet.bitstream(),
                track.codec,
                track.bitstream
            )));
        }
        let data = packet.data().as_ref();
        if data.is_empty() {
            return Err(MuxError::InvalidPacket("empty packet".into()));
        }
        let dts = packet.dts();
        if track.last_dts.is_some_and(|last| dts < last) {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        let lead = i128::try_from(self.origin_lead.as_nanos()).unwrap_or(i128::MAX);
        let origin = self.origin.unwrap_or(nanos(dts) - lead);
        if nanos(dts) < origin {
            return Err(MuxError::InvalidPacket(
                "dts more than the interleaving bound before the first packet".into(),
            ));
        }
        if self
            .newest_dts
            .is_some_and(|newest| newest.saturating_since(dts) > self.origin_lead)
        {
            return Err(MuxError::InvalidPacket(
                "dts more than the interleaving bound behind another stream".into(),
            ));
        }
        if packet.pts().saturating_since(dts) > MAX_COMPOSITION_DELAY {
            return Err(MuxError::InvalidPacket(
                "pts more than 10 s after dts".into(),
            ));
        }

        // Validate and size the packet before anything changes or is emitted, so an
        // error leaves no output and no state behind.
        let staged = self.staging.len();
        let (payload, len) = match track.kind {
            Kind::Video {
                length_size: Some(length_size),
            } => {
                let mut media = false;
                for unit in nal::length_prefixed_units(data, length_size) {
                    let unit = unit.map_err(|_| {
                        MuxError::InvalidPacket("truncated length-prefixed NAL unit".into())
                    })?;
                    check_parameter_set(track, unit)?;
                    media |= keep_unit(unit);
                }
                if !media {
                    return Err(MuxError::InvalidPacket("packet without media".into()));
                }
                (Payload::Shared(packet.data().clone()), data.len())
            }
            Kind::Video { length_size: None } => {
                let mut result = Ok(());
                for unit in nal::annexb_units(data) {
                    if let Err(e) = check_parameter_set(track, unit) {
                        result = Err(e);
                        break;
                    }
                    if keep_unit(unit) {
                        self.staging
                            .extend_from_slice(&(unit.len() as u32).to_be_bytes());
                        self.staging.extend_from_slice(unit);
                    }
                    if self.staging.len() - staged > MAX_SAMPLE_BYTES {
                        result = Err(MuxError::Overflow("sample size"));
                        break;
                    }
                }
                let converted = self.staging.len() - staged;
                if result.is_ok() && converted == 0 {
                    result = Err(MuxError::InvalidPacket("packet without media".into()));
                }
                if let Err(e) = result {
                    self.staging.truncate(staged);
                    return Err(e);
                }
                (Payload::Staged { start: 0 }, converted)
            }
            Kind::Audio => (Payload::Shared(packet.data().clone()), data.len()),
        };
        if len > MAX_SAMPLE_BYTES {
            return Err(MuxError::Overflow("sample size"));
        }
        let keyframe = packet.keyframe() || matches!(track.kind, Kind::Audio);
        let video_key = matches!(track.kind, Kind::Video { .. }) && packet.keyframe();
        let track_in_cluster = track.in_cluster;

        // A cluster ends before a video keyframe (so clusters start at entry points),
        // once it spans the cluster duration (the interleaving bound, at most 20 s), or
        // before it would exceed its byte or block bound. A keyframe joins a cluster its
        // track has no blocks in yet, so the cluster starts at it.
        // The span counts the earliest and latest dts with this packet included, so a
        // late block cannot stretch a cluster either.
        if let Some(start) = self.cluster_start {
            let span = self.cluster_end.max(dts).saturating_since(start.min(dts));
            if (video_key && track_in_cluster)
                || span >= self.cluster_span
                || self.cluster_bytes + len > MAX_CLUSTER_BYTES
                || self.blocks.len() >= MAX_CLUSTER_BLOCKS
            {
                self.flush(out);
            }
        }
        // The pending payload now starts at the (possibly moved) committed end.
        let payload = match payload {
            Payload::Staged { .. } => Payload::Staged {
                start: self.committed,
            },
            shared => shared,
        };
        self.origin = Some(origin);
        self.committed = self.staging.len();
        let track = &mut self.tracks[index];
        track.last_dts = Some(dts);
        track.in_cluster = true;
        self.blocks.push(Block {
            // At most MAX_TRACKS.
            track: (index + 1) as u8,
            payload,
            // At most MAX_SAMPLE_BYTES.
            len: len as u32,
            dts,
            pts: packet.pts(),
            keyframe,
            seq: self.seq,
        });
        // Below MAX_CLUSTER_BLOCKS: reset at every flush.
        self.seq += 1;
        self.cluster_bytes += len;
        let start = self.cluster_start.get_or_insert(dts);
        *start = (*start).min(dts);
        self.cluster_end = if self.blocks.len() == 1 {
            dts
        } else {
            self.cluster_end.max(dts)
        };
        self.newest_dts = Some(self.newest_dts.map_or(dts, |n| n.max(dts)));
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("finish outside begin..finish"));
        }
        self.flush(out);
        self.finished = true;
        Ok(())
    }
}
