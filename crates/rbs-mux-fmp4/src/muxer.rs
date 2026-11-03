//! The fragmented MP4 muxer session.

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

use crate::boxes;

const VIDEO_TIMESCALE: u32 = 90_000;
/// `trun` flags: data offset, and per-sample duration, size, flags and composition
/// time offset.
const TRUN_FLAGS: u32 = 0x0000_0f01;
/// `tfhd` flag: offsets are relative to the start of the `moof` box.
const DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;
/// Sample flags: a sync sample (depends on nothing), and a non-sync sample.
const SYNC_SAMPLE: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE: u32 = 0x0101_0000;
/// Fixed sizes of the boxes in a fragment (see `Fmp4Muxer::flush`).
const MOOF_FIXED: usize = 8 + 16; // moof header + mfhd
const TRAF_FIXED: usize = 8 + 16 + 20 + 20; // traf + tfhd + tfdt (v1) + trun header
const TRUN_SAMPLE: usize = 16;
const MDAT_HEADER: usize = 8;
/// Largest sample accepted, and the payload bytes after which a fragment ends early.
/// Together they keep every box and data offset far below the 4 GiB a 32-bit box size
/// can describe, and bound the memory a fragment holds.
const MAX_SAMPLE_BYTES: usize = 1 << 30;
const MAX_FRAGMENT_BYTES: usize = 1 << 30;
/// Samples after which a fragment ends early (a stream of equal timestamps would
/// otherwise never end it).
const MAX_FRAGMENT_SAMPLES: usize = 1 << 16;
/// Largest pts - dts accepted: keeps the signed 32-bit composition offset in range at
/// any timescale up to 384 kHz.
const MAX_COMPOSITION_DELAY: Duration = Duration::from_secs(60);

enum Kind {
    /// H.264; `length_size` is `None` for Annex B input, converted to 4-byte lengths.
    Video {
        length_size: Option<u8>,
    },
    Audio,
}

struct Sample {
    /// The payload; empty for Annex B input, which is in `Track::staging`.
    data: Bytes,
    dts: Timestamp,
    pts: Timestamp,
    duration: Option<Duration>,
    keyframe: bool,
    /// Bytes in `mdat` (after conversion for Annex B input).
    size: u32,
}

struct Track {
    id: u32,
    codec: CodecId,
    bitstream: BitstreamFormat,
    kind: Kind,
    timescale: u32,
    /// Sample duration when a packet carries none (frame interval or frame size).
    nominal: Duration,
    samples: Vec<Sample>,
    /// Annex B input converted to 4-byte lengths, the samples of this fragment in
    /// order. Converting at `write` reads the input once; the capacity is reused.
    staging: Vec<u8>,
    /// Bytes of `staging` that belong to samples already in `samples`; bytes after it
    /// are the packet being written.
    committed: usize,
    /// SPS and PPS of the sample entry. In-band parameter sets must be one of these:
    /// a changed set would make the rest of the file undecodable.
    parameter_sets: Vec<Vec<u8>>,
    last_dts: Option<Timestamp>,
}

pub(crate) struct Fmp4Muxer {
    tracks: Vec<Track>,
    has_video: bool,
    header: Vec<u8>,
    fragment: Duration,
    /// Media time 0, in nanoseconds of the master clock: chosen when the first fragment
    /// is emitted (see `Fmp4Muxer::flush`), so the earliest sample starts at 0 and no
    /// edit list is needed. Signed, so an origin before the clock's zero still works.
    origin: Option<i128>,
    /// Earliest dts accepted, in nanoseconds: the first dts minus the interleaving
    /// bound until the origin is chosen, then the origin.
    floor: Option<i128>,
    /// dts of the first sample of the fragment being collected.
    fragment_start: Option<Timestamp>,
    fragment_bytes: usize,
    fragment_samples: usize,
    sequence: u32,
    last_chunk: Option<Timestamp>,
    began: bool,
    finished: bool,
}

/// `nanos` in `timescale` units, rounded to the nearest, like
/// `rbs_core::timing::TimeBase::ticks_in` (a 1024-sample AAC frame at 48 kHz is
/// 21 333 333 ns and must still be 1024 ticks). Positions are rounded and durations
/// are differences of rounded positions, so they add up exactly.
fn ticks(nanos: u128, timescale: u32) -> u64 {
    u64::try_from((nanos * u128::from(timescale) + 500_000_000) / 1_000_000_000).unwrap_or(u64::MAX)
}

fn nanos(t: Timestamp) -> i128 {
    i128::from(t.as_nanos())
}

/// Whether an Annex B unit goes into an `avc1` sample: parameter sets live in the
/// sample entry and access unit delimiters are not needed.
fn keep_unit(unit: &[u8]) -> bool {
    !unit.is_empty() && !is_parameter_set(unit) && nal::h264_type(unit) != Some(h264::AUD)
}

fn is_parameter_set(unit: &[u8]) -> bool {
    matches!(nal::h264_type(unit), Some(h264::SPS | h264::PPS))
}

/// Refuses an in-band SPS or PPS that differs from the sample entry's.
fn check_parameter_set(track: &Track, unit: &[u8]) -> Result<(), MuxError> {
    if is_parameter_set(unit) && !track.parameter_sets.iter().any(|p| p == unit) {
        return Err(MuxError::InvalidPacket(
            "H.264 parameter sets changed; fragmented MP4 needs a new output".into(),
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
                    "Annex B H.264 in MP4 needs SPS and PPS in the extradata".into(),
                ));
            }
            Ok((avcc, None))
        }
        other => Err(CreateError::InvalidConfig(format!(
            "H.264 in {other:?} framing"
        ))),
    }
}

/// `avc1` sample entry with its `avcC` box.
fn avc1_entry(out: &mut Vec<u8>, width: u16, height: u16, avcc: &[u8]) {
    let entry = boxes::open(out, b"avc1");
    boxes::zeros(out, 6);
    boxes::u16(out, 1); // data_reference_index
    boxes::zeros(out, 16); // pre_defined, reserved
    boxes::u16(out, width);
    boxes::u16(out, height);
    boxes::u32(out, 0x0048_0000); // 72 dpi
    boxes::u32(out, 0x0048_0000);
    boxes::u32(out, 0);
    boxes::u16(out, 1); // frame_count
    boxes::zeros(out, 32); // compressorname
    boxes::u16(out, 0x0018); // depth
    boxes::u16(out, 0xffff); // pre_defined = -1
    let config = boxes::open(out, b"avcC");
    out.extend_from_slice(avcc);
    boxes::close(out, config);
    boxes::close(out, entry);
}

/// `mp4a` sample entry with its `esds` box.
fn mp4a_entry(out: &mut Vec<u8>, channels: u16, rate: u32, asc: &[u8]) {
    let entry = boxes::open(out, b"mp4a");
    boxes::zeros(out, 6);
    boxes::u16(out, 1);
    boxes::zeros(out, 8);
    boxes::u16(out, channels);
    boxes::u16(out, 16); // samplesize
    boxes::u32(out, 0); // pre_defined, reserved
    // 16.16 fixed point; rates above 65535 Hz do not fit, and decoders take the rate
    // from the AudioSpecificConfig.
    boxes::u32(out, if rate <= 0xffff { rate << 16 } else { 0 });
    let esds = boxes::open_full(out, b"esds", 0, 0);
    // ES_Descriptor > DecoderConfigDescriptor > DecoderSpecificInfo, SLConfigDescriptor.
    // Lengths fit one byte: the AudioSpecificConfig is a few bytes (checked at create).
    let asc_len = asc.len() as u8;
    let config_len = 13 + 2 + asc_len;
    out.extend_from_slice(&[0x03, 3 + 2 + config_len + 3, 0, 1, 0]);
    out.extend_from_slice(&[0x04, config_len, 0x40, 0x15, 0, 0, 0]);
    boxes::u32(out, 0); // maxBitrate (unknown)
    boxes::u32(out, 0); // avgBitrate (variable)
    out.extend_from_slice(&[0x05, asc_len]);
    out.extend_from_slice(asc);
    out.extend_from_slice(&[0x06, 1, 0x02]);
    boxes::close(out, esds);
    boxes::close(out, entry);
}

fn trak(out: &mut Vec<u8>, track: &Track, entry: &dyn Fn(&mut Vec<u8>), width: u16, height: u16) {
    let video = matches!(track.kind, Kind::Video { .. });
    let trak = boxes::open(out, b"trak");
    let tkhd = boxes::open_full(out, b"tkhd", 0, 3); // enabled, in movie
    boxes::zeros(out, 8); // creation, modification
    boxes::u32(out, track.id);
    boxes::zeros(out, 4 + 4 + 8); // reserved, duration, reserved
    boxes::u16(out, 0); // layer
    boxes::u16(out, 0); // alternate_group
    boxes::u16(out, if video { 0 } else { 0x0100 }); // volume
    boxes::u16(out, 0);
    boxes::matrix(out);
    boxes::u32(out, u32::from(width) << 16);
    boxes::u32(out, u32::from(height) << 16);
    boxes::close(out, tkhd);

    // No edit list: media time 0 is the earliest sample of the file, so presentation
    // time equals media time. (FFmpeg reads a positive edit media time on an AAC track
    // as encoder priming and skips that much audio.)

    let mdia = boxes::open(out, b"mdia");
    let mdhd = boxes::open_full(out, b"mdhd", 0, 0);
    boxes::zeros(out, 8);
    boxes::u32(out, track.timescale);
    boxes::u32(out, 0); // duration: fragmented
    boxes::u16(out, 0x55c4); // language "und"
    boxes::u16(out, 0);
    boxes::close(out, mdhd);
    let hdlr = boxes::open_full(out, b"hdlr", 0, 0);
    boxes::u32(out, 0);
    out.extend_from_slice(if video { b"vide" } else { b"soun" });
    boxes::zeros(out, 12);
    out.extend_from_slice(b"RBS\0");
    boxes::close(out, hdlr);

    let minf = boxes::open(out, b"minf");
    if video {
        let vmhd = boxes::open_full(out, b"vmhd", 0, 1);
        boxes::zeros(out, 8);
        boxes::close(out, vmhd);
    } else {
        let smhd = boxes::open_full(out, b"smhd", 0, 0);
        boxes::zeros(out, 4);
        boxes::close(out, smhd);
    }
    let dinf = boxes::open(out, b"dinf");
    let dref = boxes::open_full(out, b"dref", 0, 0);
    boxes::u32(out, 1);
    let url = boxes::open_full(out, b"url ", 0, 1); // media in this file
    boxes::close(out, url);
    boxes::close(out, dref);
    boxes::close(out, dinf);

    let stbl = boxes::open(out, b"stbl");
    let stsd = boxes::open_full(out, b"stsd", 0, 0);
    boxes::u32(out, 1);
    entry(out);
    boxes::close(out, stsd);
    for kind in [b"stts", b"stsc", b"stco"] {
        let b = boxes::open_full(out, kind, 0, 0);
        boxes::u32(out, 0);
        boxes::close(out, b);
    }
    let stsz = boxes::open_full(out, b"stsz", 0, 0);
    boxes::u32(out, 0);
    boxes::u32(out, 0);
    boxes::close(out, stsz);
    boxes::close(out, stbl);
    boxes::close(out, minf);
    boxes::close(out, mdia);
    boxes::close(out, trak);
}

impl Fmp4Muxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        let mut header = Vec::new();
        let ftyp = boxes::open(&mut header, b"ftyp");
        header.extend_from_slice(b"iso6");
        boxes::u32(&mut header, 0);
        // Not `cmfc`: a CMAF track file has one track, and these fragments carry all.
        for brand in [b"iso6", b"isom", b"mp41"] {
            header.extend_from_slice(brand);
        }
        boxes::close(&mut header, ftyp);

        let mut tracks = Vec::with_capacity(config.streams.len());
        let mut traks = Vec::new();
        for (i, stream) in config.streams.iter().enumerate() {
            let id = i as u32 + 1;
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
                        // two video tracks whose GOPs differ, fragments rarely are.
                        return Err(CreateError::Unsupported {
                            what: "more than one video stream in fragmented MP4".into(),
                        });
                    }
                    let (avcc, length_size) = avcc_for(stream)?;
                    let parameter_sets = AvcConfig::parse(&avcc)
                        .map(|c| c.sps.iter().chain(&c.pps).map(|p| p.to_vec()).collect())
                        .map_err(|e| CreateError::InvalidConfig(format!("H.264 extradata: {e}")))?;
                    let (Ok(width), Ok(height)) = (
                        u16::try_from(format.size.width),
                        u16::try_from(format.size.height),
                    ) else {
                        return Err(CreateError::InvalidConfig("picture size".into()));
                    };
                    let track = Track {
                        id,
                        codec: CodecId::H264,
                        bitstream: stream.bitstream,
                        kind: Kind::Video { length_size },
                        timescale: VIDEO_TIMESCALE,
                        nominal: frame_rate.frame_duration(),
                        samples: Vec::new(),
                        staging: Vec::new(),
                        committed: 0,
                        parameter_sets,
                        last_dts: None,
                    };
                    trak(
                        &mut traks,
                        &track,
                        &|out| avc1_entry(out, width, height, &avcc),
                        width,
                        height,
                    );
                    tracks.push(track);
                }
                (
                    CodecId::Aac,
                    StreamParams::Audio {
                        format, frame_size, ..
                    },
                ) => {
                    if stream.bitstream != BitstreamFormat::Raw {
                        return Err(CreateError::InvalidConfig(format!(
                            "AAC in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    AudioSpecificConfig::parse(&stream.extradata)
                        .map_err(|e| CreateError::InvalidConfig(format!("AAC extradata: {e}")))?;
                    if stream.extradata.len() > 64 {
                        return Err(CreateError::InvalidConfig("AAC extradata too long".into()));
                    }
                    let rate = format.sample_rate.hz();
                    let channels = format.channels.channels() as u16;
                    let track = Track {
                        id,
                        codec: CodecId::Aac,
                        bitstream: BitstreamFormat::Raw,
                        kind: Kind::Audio,
                        timescale: rate,
                        nominal: format.sample_rate.duration_of(u64::from(frame_size)),
                        samples: Vec::new(),
                        staging: Vec::new(),
                        committed: 0,
                        parameter_sets: Vec::new(),
                        last_dts: None,
                    };
                    let asc = stream.extradata.clone();
                    trak(
                        &mut traks,
                        &track,
                        &|out| mp4a_entry(out, channels, rate, &asc),
                        0,
                        0,
                    );
                    tracks.push(track);
                }
                (CodecId::H264 | CodecId::Aac, _) => {
                    return Err(CreateError::InvalidConfig(format!(
                        "{} stream with {:?} parameters",
                        stream.codec, stream.params
                    )));
                }
                (other, _) => {
                    return Err(CreateError::Unsupported {
                        what: format!("{other} in fragmented MP4"),
                    });
                }
            }
        }

        let moov = boxes::open(&mut header, b"moov");
        let mvhd = boxes::open_full(&mut header, b"mvhd", 0, 0);
        boxes::zeros(&mut header, 8);
        boxes::u32(&mut header, 1000);
        boxes::u32(&mut header, 0);
        boxes::u32(&mut header, 0x0001_0000); // rate
        boxes::u16(&mut header, 0x0100); // volume
        boxes::zeros(&mut header, 10);
        boxes::matrix(&mut header);
        boxes::zeros(&mut header, 24);
        boxes::u32(&mut header, tracks.len() as u32 + 1);
        boxes::close(&mut header, mvhd);
        header.extend_from_slice(&traks);
        let mvex = boxes::open(&mut header, b"mvex");
        for track in &tracks {
            let trex = boxes::open_full(&mut header, b"trex", 0, 0);
            boxes::u32(&mut header, track.id);
            boxes::u32(&mut header, 1);
            boxes::zeros(&mut header, 12);
            boxes::close(&mut header, trex);
        }
        boxes::close(&mut header, mvex);
        boxes::close(&mut header, moov);

        Ok(Self {
            has_video: tracks.iter().any(|t| matches!(t.kind, Kind::Video { .. })),
            tracks,
            header,
            fragment: config.max_buffered(),
            origin: None,
            floor: None,
            fragment_start: None,
            fragment_bytes: 0,
            fragment_samples: 0,
            sequence: 1,
            last_chunk: None,
            began: false,
            finished: false,
        })
    }

    /// Media time of `t` in `timescale` units from the origin, rounded to the nearest.
    /// `write` refuses timestamps before the origin.
    fn media_time(&self, t: Timestamp, timescale: u32) -> u64 {
        let since = nanos(t) - self.origin.unwrap_or_else(|| nanos(t));
        ticks(u128::try_from(since).unwrap_or(0), timescale)
    }

    /// Emits the collected samples as one `moof` + `mdat` chunk. `next` is the track
    /// and dts of the packet that ends the fragment: the last sample of that track lasts
    /// until it.
    fn flush(&mut self, next: Option<(usize, Timestamp)>, out: &mut Vec<MuxChunk>) {
        let Some(start) = self.fragment_start.take() else {
            return;
        };
        if self.origin.is_none() {
            // The first fragment: media time 0 is its earliest sample, unless that is
            // less than the interleaving bound before the newest packet so far (a first
            // fragment shorter than the bound, at a short GOP). Then the origin is that
            // bound before the newest packet, so a packet still arriving within the
            // bound keeps its timestamp; the file then starts a little after 0.
            let newest = self
                .tracks
                .iter()
                .filter_map(|t| t.last_dts)
                .chain(next.map(|(_, dts)| dts))
                .max()
                .unwrap_or(start);
            let bound = i128::try_from(self.fragment.as_nanos()).unwrap_or(i128::MAX);
            // The packet that ends the fragment goes into the next one: it must not
            // start before the origin either.
            let earliest = next.map_or(start, |(_, dts)| start.min(dts));
            let origin = nanos(earliest).min(nanos(newest).saturating_sub(bound));
            self.origin = Some(origin);
            self.floor = Some(origin);
        }
        let active = || self.tracks.iter().filter(|t| !t.samples.is_empty());
        let moof_size = MOOF_FIXED
            + active()
                .map(|t| TRAF_FIXED + TRUN_SAMPLE * t.samples.len())
                .sum::<usize>();
        let mdat_size = MDAT_HEADER
            + active()
                .flat_map(|t| t.samples.iter().map(|s| s.size as usize))
                .sum::<usize>();
        let mut chunk = Vec::with_capacity(moof_size + mdat_size);

        let moof = boxes::open(&mut chunk, b"moof");
        let mfhd = boxes::open_full(&mut chunk, b"mfhd", 0, 0);
        boxes::u32(&mut chunk, self.sequence);
        boxes::close(&mut chunk, mfhd);
        let mut data_offset = moof_size + MDAT_HEADER;
        for (index, track) in self.tracks.iter().enumerate() {
            if track.samples.is_empty() {
                continue;
            }
            let following = next.and_then(|(i, dts)| (i == index).then_some(dts));
            let traf = boxes::open(&mut chunk, b"traf");
            let tfhd = boxes::open_full(&mut chunk, b"tfhd", 0, DEFAULT_BASE_IS_MOOF);
            boxes::u32(&mut chunk, track.id);
            boxes::close(&mut chunk, tfhd);
            let tfdt = boxes::open_full(&mut chunk, b"tfdt", 1, 0);
            let base = self.media_time(track.samples[0].dts, track.timescale);
            boxes::u64(&mut chunk, base);
            boxes::close(&mut chunk, tfdt);
            let trun = boxes::open_full(&mut chunk, b"trun", 1, TRUN_FLAGS);
            boxes::u32(&mut chunk, track.samples.len() as u32);
            // Below 4 GiB: see MAX_SAMPLE_BYTES and MAX_FRAGMENT_BYTES.
            boxes::u32(&mut chunk, data_offset as u32);
            for (i, s) in track.samples.iter().enumerate() {
                let dts = self.media_time(s.dts, track.timescale);
                let end = match track.samples.get(i + 1) {
                    Some(next) => self.media_time(next.dts, track.timescale),
                    None => match following {
                        Some(dts) => self.media_time(dts, track.timescale),
                        None => self.media_time(
                            s.dts + s.duration.unwrap_or(track.nominal),
                            track.timescale,
                        ),
                    },
                };
                // pts >= dts (packet invariant) and pts - dts <= MAX_COMPOSITION_DELAY
                // (checked in `write`), so the offset is non-negative and fits an i32.
                let pts = self.media_time(s.pts, track.timescale);
                let cts = u32::try_from(pts.saturating_sub(dts)).unwrap_or(u32::MAX);
                // dts is non-decreasing in a track; a fragment spans at most
                // `fragment_duration`, so durations fit 32 bits at these timescales.
                let duration = u32::try_from(end.saturating_sub(dts)).unwrap_or(u32::MAX);
                boxes::u32(&mut chunk, duration);
                boxes::u32(&mut chunk, s.size);
                let sync = s.keyframe || matches!(track.kind, Kind::Audio);
                boxes::u32(&mut chunk, if sync { SYNC_SAMPLE } else { NON_SYNC_SAMPLE });
                boxes::u32(&mut chunk, cts);
                data_offset += s.size as usize;
            }
            boxes::close(&mut chunk, trun);
            boxes::close(&mut chunk, traf);
        }
        boxes::close(&mut chunk, moof);

        let mdat = boxes::open(&mut chunk, b"mdat");
        for track in active() {
            for s in &track.samples {
                chunk.extend_from_slice(&s.data);
            }
            chunk.extend_from_slice(&track.staging[..track.committed]);
        }
        boxes::close(&mut chunk, mdat);
        debug_assert_eq!(chunk.len(), chunk.capacity());

        // An entry point for every stream: audio samples all are, and there is at most
        // one video track (checked at create), whose first sample must be a keyframe.
        let keyframe = if self.has_video {
            self.tracks
                .iter()
                .find(|t| matches!(t.kind, Kind::Video { .. }))
                .and_then(|t| t.samples.first())
                .is_some_and(|s| s.keyframe)
        } else {
            true
        };
        for track in &mut self.tracks {
            track.samples.clear();
            // Keep the packet being written (after `committed`) for the next fragment.
            track.staging.copy_within(track.committed.., 0);
            track
                .staging
                .truncate(track.staging.len() - track.committed);
            track.committed = 0;
        }
        self.fragment_bytes = 0;
        self.fragment_samples = 0;
        self.sequence = self.sequence.wrapping_add(1);
        // Chunk timestamps never go back: a fragment can start earlier than the one
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

impl Muxer for Fmp4Muxer {
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
        let bound = i128::try_from(self.fragment.as_nanos()).unwrap_or(i128::MAX);
        let floor = self.floor.unwrap_or(nanos(dts).saturating_sub(bound));
        if nanos(dts) < floor {
            return Err(MuxError::InvalidPacket(
                "dts more than the interleaving bound before the start".into(),
            ));
        }
        if packet.pts().saturating_since(dts) > MAX_COMPOSITION_DELAY {
            return Err(MuxError::InvalidPacket(
                "pts more than a minute after dts".into(),
            ));
        }

        // Validate and size the packet before anything changes or is emitted, so an
        // error leaves no output and no state behind.
        let track = &mut self.tracks[index];
        let mut stored = packet.data().clone();
        let size = match track.kind {
            Kind::Video {
                length_size: Some(length_size),
            } => {
                for unit in nal::length_prefixed_units(data, length_size) {
                    let unit = unit.map_err(|_| {
                        MuxError::InvalidPacket("truncated length-prefixed NAL unit".into())
                    })?;
                    check_parameter_set(track, unit)?;
                }
                data.len()
            }
            Kind::Video { length_size: None } => {
                // Converted after the committed samples; a flush below keeps it.
                let staged = track.staging.len();
                let mut result = Ok(());
                for unit in nal::annexb_units(data) {
                    if let Err(e) = check_parameter_set(track, unit) {
                        result = Err(e);
                        break;
                    }
                    if keep_unit(unit) {
                        boxes::u32(&mut track.staging, unit.len() as u32);
                        track.staging.extend_from_slice(unit);
                    }
                    if track.staging.len() - staged > MAX_SAMPLE_BYTES {
                        result = Err(MuxError::Overflow("sample size"));
                        break;
                    }
                }
                let converted = track.staging.len() - staged;
                if result.is_ok() && converted == 0 {
                    result = Err(MuxError::InvalidPacket("packet without media".into()));
                }
                if let Err(e) = result {
                    track.staging.truncate(staged);
                    return Err(e);
                }
                stored = Bytes::new();
                converted
            }
            Kind::Audio => data.len(),
        };
        if size > MAX_SAMPLE_BYTES {
            return Err(MuxError::Overflow("sample size"));
        }
        let keyframe = packet.keyframe();
        let is_video = matches!(track.kind, Kind::Video { .. });
        let track_has_samples = !track.samples.is_empty();

        // A fragment ends before a video keyframe (so fragments start at entry points),
        // once it spans the fragment duration (the interleaving bound), or before it
        // would exceed its byte or sample bound. A keyframe joins a fragment its track
        // has no samples in yet, so the fragment starts at it. Very short GOPs therefore
        // give short fragments (one per GOP); there is no minimum length.
        if let Some(start) = self.fragment_start {
            if (is_video && keyframe && track_has_samples)
                || dts.saturating_since(start) >= self.fragment
                || self.fragment_bytes + size > MAX_FRAGMENT_BYTES
                || self.fragment_samples >= MAX_FRAGMENT_SAMPLES
            {
                self.flush(Some((index, dts)), out);
            }
        }
        if self.floor.is_none() {
            self.floor = Some(floor);
        }
        let track = &mut self.tracks[index];
        track.committed = track.staging.len();
        track.last_dts = Some(dts);
        track.samples.push(Sample {
            data: stored,
            dts,
            pts: packet.pts(),
            duration: packet.duration(),
            keyframe,
            // At most MAX_SAMPLE_BYTES.
            size: size as u32,
        });
        self.fragment_bytes += size;
        self.fragment_samples += 1;
        let start = self.fragment_start.get_or_insert(dts);
        *start = (*start).min(dts);
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("finish outside begin..finish"));
        }
        self.flush(None, out);
        self.finished = true;
        Ok(())
    }
}
