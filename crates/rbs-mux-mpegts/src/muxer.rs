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

use crate::packetizer::{self, Continuity, UnitStart, unit_size};
use crate::psi;

/// PAT and PMT are repeated at least this often, and before every video keyframe.
const PSI_INTERVAL: Duration = Duration::from_millis(100);
/// PTS and DTS run this far ahead of the PCR, the decoder buffering delay.
const MUX_DELAY_90K: u64 = 63_000; // 0.7 s
/// Most streams per session: the PMT must fit in one TS packet.
pub(crate) const MAX_STREAMS: usize = 32;
const TS_WRAP: u64 = 1 << 33;
const START_CODE: [u8; 4] = [0, 0, 0, 1];
const AUD: [u8; 2] = [0x09, 0xf0];

enum Codec {
    /// H.264. `length_size` is `None` for Annex B input. `parameter_sets` holds the
    /// SPS and PPS from the extradata in Annex B form, inserted before keyframes that
    /// do not carry their own.
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
    last_chunk: Option<Timestamp>,
    /// PES packet under construction; grows to the largest access unit, then reused.
    pes: Vec<u8>,
    began: bool,
    finished: bool,
}

/// Converts master-clock time to the 90 kHz clock, modulo 2^33.
fn clock_90k(t: Timestamp) -> u64 {
    ((u128::from(t.as_nanos()) * 9 / 100_000) % u128::from(TS_WRAP)) as u64
}

fn write_timestamp(out: &mut Vec<u8>, prefix: u8, ts: u64) {
    out.extend_from_slice(&[
        (prefix << 4) | (((ts >> 29) as u8) & 0x0e) | 1,
        (ts >> 22) as u8,
        (((ts >> 14) as u8) & 0xfe) | 1,
        (ts >> 7) as u8,
        (((ts << 1) as u8) & 0xfe) | 1,
    ]);
}

fn parameter_sets(stream: &StreamDescriptor) -> Result<Vec<u8>, CreateError> {
    let mut out = Vec::new();
    let extradata = stream.extradata.as_ref();
    if extradata.is_empty() {
        return Ok(out);
    }
    if extradata[0] == 1 {
        let avcc = AvcConfig::parse(extradata)
            .map_err(|e| CreateError::InvalidConfig(format!("H.264 extradata: {e}")))?;
        for set in avcc.sps.iter().chain(&avcc.pps) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(set);
        }
    } else {
        for unit in nal::annexb_units(extradata) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(unit);
        }
    }
    Ok(out)
}

fn stream_setup(index: usize, stream: &StreamDescriptor) -> Result<(Stream, u8), CreateError> {
    let pid = psi::ES_PID_BASE + index as u16;
    let (codec, stream_type, stream_id) = match (stream.codec, stream.bitstream) {
        (CodecId::H264, BitstreamFormat::AnnexB) => (
            Codec::H264 {
                length_size: None,
                parameter_sets: parameter_sets(stream)?,
            },
            psi::STREAM_TYPE_H264,
            0xe0,
        ),
        (CodecId::H264, BitstreamFormat::LengthPrefixed { length_size }) => {
            let parameter_sets = parameter_sets(stream)?;
            if parameter_sets.is_empty() {
                return Err(CreateError::InvalidConfig(
                    "length-prefixed H.264 needs avcC extradata".into(),
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

impl TsMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        // The PMT must fit in one TS packet.
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
            last_chunk: None,
            pes: Vec::new(),
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
            let mut packet = [0xff; packetizer::TS_PACKET];
            packet[0] = 0x47;
            packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1f);
            packet[2] = pid as u8;
            packet[3] = 0x10 | cc.next();
            packet[4] = 0;
            packet[5..5 + section.len()].copy_from_slice(section);
            out.extend_from_slice(&packet);
        }
    }

    /// Builds the PES packet for `packet` into `self.pes`.
    fn build_pes(&mut self, index: usize, packet: &EncodedPacket) -> Result<(), MuxError> {
        let stream = &self.streams[index];
        let pts = (clock_90k(packet.pts()) + MUX_DELAY_90K) % TS_WRAP;
        let dts = (clock_90k(packet.dts()) + MUX_DELAY_90K) % TS_WRAP;
        let data = packet.data().as_ref();
        let pes = &mut self.pes;
        pes.clear();
        pes.extend_from_slice(&[0, 0, 1, stream.stream_id, 0, 0, 0x84]);
        if pts == dts {
            pes.extend_from_slice(&[0x80, 5]);
            write_timestamp(pes, 0x2, pts);
        } else {
            pes.extend_from_slice(&[0xc0, 10]);
            write_timestamp(pes, 0x3, pts);
            write_timestamp(pes, 0x1, dts);
        }
        match &stream.codec {
            Codec::H264 {
                length_size,
                parameter_sets,
            } => {
                let invalid = |e| MuxError::InvalidPacket(format!("H.264 packet: {e}"));
                let first_type = match length_size {
                    Some(size) => match nal::length_prefixed_units(data, *size).next() {
                        Some(unit) => nal::h264_type(unit.map_err(invalid)?),
                        None => None,
                    },
                    None => nal::annexb_first_header(data).map(|b| b & 0x1f),
                };
                // Only keyframes are scanned for in-band parameter sets.
                let has_sps = packet.keyframe()
                    && match length_size {
                        Some(size) => contains_sps(
                            nal::length_prefixed_units(data, *size).map(|u| u.map_err(invalid)),
                        )?,
                        None => contains_sps(nal::annexb_units(data).map(Ok))?,
                    };
                if first_type != Some(h264::AUD) {
                    pes.extend_from_slice(&START_CODE);
                    pes.extend_from_slice(&AUD);
                }
                if packet.keyframe() && !has_sps {
                    pes.extend_from_slice(parameter_sets);
                }
                match length_size {
                    Some(size) => {
                        nal::length_prefixed_to_annexb(data, *size, &[], pes).map_err(invalid)?
                    }
                    None => pes.extend_from_slice(data),
                }
                // Video PES packets may leave PES_packet_length at 0 (unbounded).
                let len = pes.len() - 6;
                if len <= usize::from(u16::MAX) {
                    pes[4..6].copy_from_slice(&(len as u16).to_be_bytes());
                }
            }
            Codec::Aac { config } => {
                let header = config
                    .adts_header(data.len())
                    .map_err(|e| MuxError::InvalidPacket(format!("AAC frame: {e}")))?;
                pes.extend_from_slice(&header);
                pes.extend_from_slice(data);
                let len = u16::try_from(pes.len() - 6)
                    .map_err(|_| MuxError::InvalidPacket("AAC frame too large".into()))?;
                pes[4..6].copy_from_slice(&len.to_be_bytes());
            }
        }
        Ok(())
    }
}

/// Whether any unit is an SPS.
fn contains_sps<'a>(
    mut units: impl Iterator<Item = Result<&'a [u8], MuxError>>,
) -> Result<bool, MuxError> {
    units.try_fold(false, |found, unit| {
        Ok(found || nal::h264_type(unit?) == Some(h264::SPS))
    })
}

impl Muxer for TsMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.began {
            return Err(MuxError::InvalidState("begin called twice"));
        }
        self.began = true;
        let mut header = Vec::with_capacity(2 * packetizer::TS_PACKET);
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
        self.build_pes(index, packet)?;
        self.streams[index].last_dts = Some(dts);

        // In an output with video, only video keyframes are entry points; in an
        // audio-only output every frame is.
        let keyframe = if self.has_video {
            video && packet.keyframe()
        } else {
            true
        };
        let psi = keyframe
            || self
                .last_psi
                .is_none_or(|last| dts.saturating_since(last) >= PSI_INTERVAL);
        let start = UnitStart {
            pcr: (index == self.pcr_stream).then(|| clock_90k(dts)),
            random_access: keyframe,
        };
        // Sized exactly, so the chunk is the session's only allocation for this packet
        // (`Bytes::from` keeps a full vector without copying or reallocating).
        let psi_len = if psi { 2 * packetizer::TS_PACKET } else { 0 };
        let mut data = Vec::with_capacity(psi_len + unit_size(self.pes.len(), start));
        if psi {
            self.write_psi(&mut data);
            self.last_psi = Some(dts);
        }
        let state = &mut self.streams[index];
        packetizer::write_unit(
            &mut data,
            state.pid,
            &mut state.continuity,
            start,
            &self.pes,
        );

        // Chunk timestamps never go backwards, even if streams arrive slightly out of
        // dts order.
        let timestamp = self.last_chunk.map_or(dts, |last| last.max(dts));
        self.last_chunk = Some(timestamp);
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe,
                timestamp,
            },
            Bytes::from(data),
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
