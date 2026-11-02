//! The MPEG-TS muxer session.

use std::time::Duration;

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, EncodedPacket, MuxChunk, StreamDescriptor,
    StreamIndex,
};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

use crate::packetizer::{self, Continuity, TS_PACKET, UnitStart, UnitWriter, unit_size};
use crate::psi;

/// PAT and PMT are repeated at least this often, and before every video keyframe; the
/// PCR is sent at least this often too.
const PSI_INTERVAL: Duration = Duration::from_millis(100);
const PCR_INTERVAL: Duration = Duration::from_millis(100);
/// Most streams per session: the PMT (12 + 5 per stream + 4 bytes) must fit in the 183
/// bytes after the pointer field of one TS packet; 32 streams take 176.
pub(crate) const MAX_STREAMS: usize = 32;
/// PTS and DTS run this far ahead of the PCR, the decoder buffering delay.
const MUX_DELAY_90K: u64 = 63_000; // 0.7 s
const TS_WRAP: u64 = 1 << 33;
const START_CODE: [u8; 4] = [0, 0, 0, 1];
const AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xf0];
const ADTS_HEADER: usize = 7;

enum Codec {
    /// H.264. `length_size` is `None` for Annex B input. `parameter_sets` holds SPS and
    /// PPS in Annex B form (from the extradata, or learned from the first keyframe
    /// that carries them), inserted before keyframes that do not carry their own.
    H264 {
        length_size: Option<u8>,
        parameter_sets: Vec<u8>,
    },
    /// AAC raw frames, written with ADTS headers.
    Aac { config: AudioSpecificConfig },
}

struct Stream {
    bitstream: BitstreamFormat,
    pid: u16,
    stream_id: u8,
    codec: Codec,
    continuity: Continuity,
    last_dts: Option<Timestamp>,
}

pub(crate) struct TsMuxer {
    streams: Vec<Stream>,
    pcr_stream: usize,
    has_video: bool,
    pat: Vec<u8>,
    pmt: Vec<u8>,
    pat_cc: Continuity,
    pmt_cc: Continuity,
    last_psi: Option<Timestamp>,
    /// Master time of the newest PCR; PCRs never go backwards.
    last_pcr: Option<Timestamp>,
    last_chunk: Option<Timestamp>,
    began: bool,
    finished: bool,
}

/// Converts master-clock time to the 90 kHz clock, modulo 2^33.
fn clock_90k(t: Timestamp) -> u64 {
    ((u128::from(t.as_nanos()) * 9 / 100_000) % u128::from(TS_WRAP)) as u64
}

fn timestamp_bytes(prefix: u8, ts: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((ts >> 29) as u8) & 0x0e) | 1,
        (ts >> 22) as u8,
        (((ts >> 14) as u8) & 0xfe) | 1,
        (ts >> 7) as u8,
        (((ts << 1) as u8) & 0xfe) | 1,
    ]
}

/// Whether units with these header bytes carry both an SPS and a PPS before their first
/// slice. Never allocates; stops at the first slice.
fn has_parameter_sets(headers: impl Iterator<Item = u8>) -> bool {
    let (mut sps, mut pps) = (false, false);
    for header in headers {
        match Some(header & 0x1f) {
            Some(h264::SPS) => sps = true,
            Some(h264::PPS) => pps = true,
            Some(kind) if nal::h264_is_vcl(kind) => break,
            _ => {}
        }
    }
    sps && pps
}

/// SPS and PPS of `units` up to the first slice, in Annex B form; empty unless both
/// are present.
fn collect_parameter_sets<'a>(units: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut sps, mut pps) = (false, false);
    for unit in units {
        match nal::h264_type(unit) {
            Some(h264::SPS) => sps = true,
            Some(h264::PPS) => pps = true,
            Some(kind) if nal::h264_is_vcl(kind) => break,
            _ => continue,
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(unit);
    }
    if !(sps && pps) {
        out.clear();
    }
    out
}

fn extradata_parameter_sets(stream: &StreamDescriptor) -> Result<Vec<u8>, CreateError> {
    let extradata = stream.extradata.as_ref();
    match extradata.first() {
        None => Ok(Vec::new()),
        Some(1) => {
            let avcc = AvcConfig::parse(extradata)
                .map_err(|e| CreateError::InvalidConfig(format!("H.264 extradata: {e}")))?;
            if let BitstreamFormat::LengthPrefixed { length_size } = stream.bitstream {
                if length_size != avcc.length_size {
                    return Err(CreateError::InvalidConfig(format!(
                        "packets use {length_size}-byte NAL lengths, avcC says {}",
                        avcc.length_size
                    )));
                }
            }
            Ok(collect_parameter_sets(
                avcc.sps.iter().chain(&avcc.pps).copied(),
            ))
        }
        Some(_) => Ok(collect_parameter_sets(nal::annexb_units(extradata))),
    }
}

fn stream_setup(index: usize, stream: &StreamDescriptor) -> Result<(Stream, u8), CreateError> {
    let pid = psi::ES_PID_BASE + index as u16;
    let (codec, stream_type, stream_id) = match (stream.codec, stream.bitstream) {
        (CodecId::H264, BitstreamFormat::AnnexB) => (
            Codec::H264 {
                length_size: None,
                parameter_sets: extradata_parameter_sets(stream)?,
            },
            psi::STREAM_TYPE_H264,
            0xe0,
        ),
        (CodecId::H264, BitstreamFormat::LengthPrefixed { length_size }) => {
            if !matches!(length_size, 1 | 2 | 4) {
                return Err(CreateError::InvalidConfig(format!(
                    "NAL length size {length_size}"
                )));
            }
            if stream.extradata.first() != Some(&1) {
                return Err(CreateError::InvalidConfig(
                    "length-prefixed H.264 needs avcC extradata".into(),
                ));
            }
            let parameter_sets = extradata_parameter_sets(stream)?;
            if parameter_sets.is_empty() {
                return Err(CreateError::InvalidConfig(
                    "avcC extradata without SPS and PPS".into(),
                ));
            }
            (
                Codec::H264 {
                    length_size: Some(length_size),
                    parameter_sets,
                },
                psi::STREAM_TYPE_H264,
                0xe0,
            )
        }
        (CodecId::Aac, BitstreamFormat::Raw) => {
            let config = AudioSpecificConfig::parse(&stream.extradata)
                .map_err(|e| CreateError::InvalidConfig(format!("AAC extradata: {e}")))?;
            // ADTS signals only some configurations (no explicit HE-AAC, no PCE).
            config
                .adts_header(0)
                .map_err(|e| CreateError::Unsupported {
                    what: format!("AAC configuration in ADTS: {e}"),
                })?;
            (Codec::Aac { config }, psi::STREAM_TYPE_AAC_ADTS, 0xc0)
        }
        (CodecId::H264 | CodecId::Aac, other) => {
            return Err(CreateError::InvalidConfig(format!(
                "{} in {other:?} framing",
                stream.codec
            )));
        }
        (codec, _) => {
            return Err(CreateError::Unsupported {
                what: format!("{codec} in MPEG-TS"),
            });
        }
    };
    Ok((
        Stream {
            bitstream: stream.bitstream,
            pid,
            stream_id,
            codec,
            continuity: Continuity::default(),
            last_dts: None,
        },
        stream_type,
    ))
}

/// How a video access unit is laid out in its PES packet.
struct VideoLayout {
    /// Bytes of the leading access unit delimiter to write (an inserted AUD, or the
    /// input's own first unit), then optionally the parameter sets, then the rest.
    insert_aud: bool,
    /// Input bytes (Annex B) or units (length-prefixed) that form the leading AUD.
    lead: usize,
    with_parameter_sets: bool,
    /// The keyframe can be decoded from this packet alone (parameter sets known).
    decodable: bool,
    /// Elementary stream bytes in total.
    size: usize,
}

impl TsMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        if config.streams.len() > MAX_STREAMS {
            return Err(CreateError::InvalidConfig("too many streams".into()));
        }
        let mut streams = Vec::with_capacity(config.streams.len());
        let mut table = Vec::with_capacity(config.streams.len());
        for (i, descriptor) in config.streams.iter().enumerate() {
            let (stream, stream_type) = stream_setup(i, descriptor)?;
            table.push((stream_type, stream.pid));
            streams.push(stream);
        }
        let video = streams
            .iter()
            .position(|s| matches!(s.codec, Codec::H264 { .. }));
        let pcr_stream = video.unwrap_or(0);
        let pmt = psi::pmt_section(&table, streams[pcr_stream].pid);
        Ok(Self {
            streams,
            pcr_stream,
            has_video: video.is_some(),
            pat: psi::pat_section(),
            pmt,
            pat_cc: Continuity::default(),
            pmt_cc: Continuity::default(),
            last_psi: None,
            last_pcr: None,
            last_chunk: None,
            began: false,
            finished: false,
        })
    }

    fn write_psi(&mut self, out: &mut Vec<u8>) {
        for (pid, section, cc) in [
            (psi::PAT_PID, &self.pat, &mut self.pat_cc),
            (psi::PMT_PID, &self.pmt, &mut self.pmt_cc),
        ] {
            // One packet: header, pointer field 0, the section, then 0xff filler.
            // Sections fit: the PAT is 16 bytes, the PMT at most 176 (MAX_STREAMS).
            let mut packet = [0xff; TS_PACKET];
            packet[0] = 0x47;
            packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1f);
            packet[2] = pid as u8;
            packet[3] = 0x10 | cc.next();
            packet[4] = 0;
            if let Some(body) = packet.get_mut(5..5 + section.len()) {
                body.copy_from_slice(section);
            }
            out.extend_from_slice(&packet);
        }
    }

    /// Validates a video packet and decides its layout. Reads only the first unit of
    /// non-keyframes; keyframes are read up to their first slice.
    fn video_layout(
        &mut self,
        index: usize,
        packet: &EncodedPacket,
    ) -> Result<VideoLayout, MuxError> {
        let data = packet.data().as_ref();
        if data.is_empty() {
            return Err(MuxError::InvalidPacket("empty H.264 packet".into()));
        }
        let invalid = |_| MuxError::InvalidPacket("truncated length-prefixed NAL unit".into());
        let Codec::H264 {
            length_size,
            parameter_sets,
        } = &mut self.streams[index].codec
        else {
            return Err(MuxError::InvalidPacket("not a video stream".into()));
        };
        let (first_type, lead, body_size) = match *length_size {
            Some(size) => {
                // One pass over the lengths: validates every unit and sums the Annex B
                // size, so nothing is written for a malformed packet.
                let mut total = 0;
                let mut first = None;
                for unit in nal::length_prefixed_units(data, size) {
                    let unit = unit.map_err(invalid)?;
                    if !unit.is_empty() {
                        first.get_or_insert(unit);
                        total += START_CODE.len() + unit.len();
                    }
                }
                let first = first.and_then(nal::h264_type);
                (first, usize::from(first == Some(h264::AUD)), total)
            }
            None => {
                let Some(header) = nal::annexb_first_header(data) else {
                    return Err(MuxError::InvalidPacket(
                        "Annex B packet without start code".into(),
                    ));
                };
                let first = Some(header & 0x1f);
                let lead = if first == Some(h264::AUD) {
                    // Input bytes up to the end of the AUD unit.
                    nal::annexb_units(data).next().map_or(0, |unit| {
                        unit.as_ptr() as usize - data.as_ptr() as usize + unit.len()
                    })
                } else {
                    0
                };
                (first, lead, data.len())
            }
        };
        let insert_aud = first_type != Some(h264::AUD);
        let mut with_parameter_sets = false;
        let mut decodable = true;
        if packet.keyframe() {
            let lengths = |size| nal::length_prefixed_units(data, size).filter_map(Result::ok);
            let in_band = match *length_size {
                Some(size) => has_parameter_sets(lengths(size).filter_map(|u| u.first().copied())),
                None => has_parameter_sets(nal::annexb_unit_headers(data)),
            };
            if in_band {
                if parameter_sets.is_empty() {
                    // Learned once; later keyframes without their own reuse them.
                    *parameter_sets = match *length_size {
                        Some(size) => collect_parameter_sets(lengths(size)),
                        None => collect_parameter_sets(nal::annexb_units(data)),
                    };
                }
            } else if parameter_sets.is_empty() {
                decodable = false;
            } else {
                with_parameter_sets = true;
            }
        }
        let size = if insert_aud { AUD.len() } else { 0 }
            + if with_parameter_sets {
                parameter_sets.len()
            } else {
                0
            }
            + body_size;
        Ok(VideoLayout {
            insert_aud,
            lead,
            with_parameter_sets,
            decodable,
            size,
        })
    }
}

/// Writes the elementary stream bytes of a video access unit, in layout order.
fn write_video(w: &mut UnitWriter<'_>, codec: &Codec, layout: &VideoLayout, data: &[u8]) {
    let Codec::H264 {
        length_size,
        parameter_sets,
    } = codec
    else {
        return;
    };
    if layout.insert_aud {
        w.push(&AUD);
    }
    match *length_size {
        Some(size) => {
            let mut units = nal::length_prefixed_units(data, size)
                .filter_map(Result::ok)
                .filter(|u| !u.is_empty());
            for unit in units.by_ref().take(layout.lead) {
                w.push(&START_CODE);
                w.push(unit);
            }
            if layout.with_parameter_sets {
                w.push(parameter_sets);
            }
            for unit in units {
                w.push(&START_CODE);
                w.push(unit);
            }
        }
        None => {
            let (lead, rest) = data.split_at(layout.lead.min(data.len()));
            w.push(lead);
            if layout.with_parameter_sets {
                w.push(parameter_sets);
            }
            w.push(rest);
        }
    }
}

impl Muxer for TsMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.began {
            return Err(MuxError::InvalidState("begin called twice"));
        }
        self.began = true;
        let mut header = Vec::with_capacity(2 * TS_PACKET);
        self.write_psi(&mut header);
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
        let Some(state) = self.streams.get(index) else {
            return Err(MuxError::UnknownStream(stream.0));
        };
        let video = matches!(state.codec, Codec::H264 { .. });
        let expected = if video { CodecId::H264 } else { CodecId::Aac };
        if packet.codec() != expected {
            return Err(MuxError::InvalidPacket(format!(
                "{} packet on a {expected} stream",
                packet.codec()
            )));
        }
        if packet.bitstream() != state.bitstream {
            return Err(MuxError::InvalidPacket(format!(
                "{:?} packet on a {:?} stream",
                packet.bitstream(),
                state.bitstream
            )));
        }
        let dts = packet.dts();
        if state.last_dts.is_some_and(|last| dts < last) {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        let data = packet.data().as_ref();

        // Elementary stream size and, for video, the layout; nothing is written for a
        // packet that is refused.
        let layout = if video {
            Some(self.video_layout(index, packet)?)
        } else {
            if data.is_empty() {
                return Err(MuxError::InvalidPacket("empty AAC packet".into()));
            }
            None
        };
        let es_size = layout.as_ref().map_or(ADTS_HEADER + data.len(), |l| l.size);
        let adts = match &self.streams[index].codec {
            Codec::Aac { config } => Some(
                config
                    .adts_header(data.len())
                    .map_err(|e| MuxError::InvalidPacket(format!("AAC frame: {e}")))?,
            ),
            Codec::H264 { .. } => None,
        };

        // PES header.
        let pts = (clock_90k(packet.pts()) + MUX_DELAY_90K) % TS_WRAP;
        let dts90 = (clock_90k(dts) + MUX_DELAY_90K) % TS_WRAP;
        let mut header = [0u8; 19];
        header[..4].copy_from_slice(&[0, 0, 1, self.streams[index].stream_id]);
        header[6] = 0x84; // marker bits, data_alignment_indicator
        let header_len = if pts == dts90 {
            header[7..9].copy_from_slice(&[0x80, 5]);
            header[9..14].copy_from_slice(&timestamp_bytes(0x2, pts));
            14
        } else {
            header[7..9].copy_from_slice(&[0xc0, 10]);
            header[9..14].copy_from_slice(&timestamp_bytes(0x3, pts));
            header[14..19].copy_from_slice(&timestamp_bytes(0x1, dts90));
            19
        };
        let pes_size = header_len + es_size;
        // Video may leave PES_packet_length at 0 (unbounded); audio must fit.
        let length = match u16::try_from(pes_size - 6) {
            Ok(length) => length,
            Err(_) if video => 0,
            Err(_) => return Err(MuxError::InvalidPacket("AAC frame too large".into())),
        };
        header[4..6].copy_from_slice(&length.to_be_bytes());
        self.streams[index].last_dts = Some(dts);

        // In an output with video, only decodable video keyframes are entry points; in
        // an audio-only output every frame is.
        let keyframe = if self.has_video {
            layout
                .as_ref()
                .is_some_and(|l| packet.keyframe() && l.decodable)
        } else {
            true
        };
        let psi = keyframe
            || self
                .last_psi
                .is_none_or(|last| dts.saturating_since(last) >= PSI_INTERVAL);
        let pcr_time = self.last_pcr.map_or(dts, |last| last.max(dts));
        let on_pcr_stream = index == self.pcr_stream;
        // Keeps the PCR flowing while the PCR stream (video) is quiet.
        let pcr_only = !on_pcr_stream
            && self
                .last_pcr
                .is_none_or(|last| dts.saturating_since(last) >= PCR_INTERVAL);
        let start = UnitStart {
            pcr: on_pcr_stream.then(|| clock_90k(pcr_time)),
            random_access: keyframe,
        };
        // Sized exactly, so the chunk is the session's only allocation for this packet
        // (`Bytes::from` keeps a full vector without copying or reallocating).
        let size = if psi { 2 * TS_PACKET } else { 0 }
            + if pcr_only { TS_PACKET } else { 0 }
            + unit_size(pes_size, start);
        let mut chunk = Vec::with_capacity(size);
        if psi {
            self.write_psi(&mut chunk);
            self.last_psi = Some(dts);
        }
        if pcr_only {
            let pcr = &self.streams[self.pcr_stream];
            packetizer::write_pcr_only(&mut chunk, pcr.pid, pcr.continuity, clock_90k(pcr_time));
        }
        if on_pcr_stream || pcr_only {
            self.last_pcr = Some(pcr_time);
        }
        let state = &mut self.streams[index];
        let mut writer = UnitWriter::new(
            &mut chunk,
            state.pid,
            &mut state.continuity,
            start,
            pes_size,
        );
        writer.push(&header[..header_len]);
        match (&layout, adts) {
            (Some(layout), _) => write_video(&mut writer, &state.codec, layout, data),
            (None, Some(adts)) => {
                writer.push(&adts);
                writer.push(data);
            }
            (None, None) => {}
        }
        writer.finish();
        debug_assert_eq!(chunk.len(), chunk.capacity());

        // Chunk timestamps never go backwards, even if streams arrive slightly out of
        // dts order.
        let timestamp = self.last_chunk.map_or(dts, |last| last.max(dts));
        self.last_chunk = Some(timestamp);
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe,
                timestamp,
            },
            Bytes::from(chunk),
        ));
        Ok(())
    }

    fn finish(&mut self, _out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("finish outside begin..finish"));
        }
        // MPEG-TS has no trailer and nothing is held back.
        self.finished = true;
        Ok(())
    }
}
