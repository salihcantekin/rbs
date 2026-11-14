//! The fragmented MP4 muxer session.

use std::time::Duration;

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk,
    StreamDescriptor, StreamIndex, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;
use rbs_media_format::av1::Av1Config;
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::hvcc::Hvcc;
use rbs_media_format::nal::{self, h264, hevc};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

use crate::boxes;

const VIDEO_TIMESCALE: u32 = 90_000;
/// Movie header timescale. A 32-bit duration at this rate covers about 49 days.
const MOVIE_TIMESCALE: u32 = 1000;
/// `trun` flags: data offset, and per-sample duration, size, flags and composition
/// time offset.
const TRUN_FLAGS: u32 = 0x0000_0f01;
/// `trun` flags of a PCM track: data offset only; every sample takes the defaults of
/// its `tfhd`.
const TRUN_PCM_FLAGS: u32 = 0x0000_0001;
/// `tfhd` flag: offsets are relative to the start of the `moof` box.
const DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;
/// `tfhd` flags of a PCM track: default sample duration, size and flags present.
const TFHD_PCM_DEFAULTS: u32 = 0x0000_0038;
/// Sample flags: a sync sample (depends on nothing), and a non-sync sample.
const SYNC_SAMPLE: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE: u32 = 0x0101_0000;
/// Fixed sizes of the boxes in a fragment (see `Fmp4Muxer::flush`).
const MOOF_FIXED: usize = 8 + 16; // moof header + mfhd
const TRAF_FIXED: usize = 8 + 16 + 20 + 20; // traf + tfhd + tfdt (v1) + trun header
const TRUN_SAMPLE: usize = 16;
/// A PCM `traf`: its `tfhd` carries the three defaults and its `trun` no sample rows.
const TRAF_PCM: usize = TRAF_FIXED + 12;
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
    /// H.264 or HEVC. `length_size` is `None` for Annex B input, converted to 4-byte lengths.
    Video {
        length_size: Option<u8>,
    },
    /// AV1 temporal unit, copied as OBUs.
    Obu,
    Audio,
    /// Interleaved little-endian PCM (MOV only). Each packet is a run of frames of
    /// `frame_bytes` (channels x bytes per sample); in the file every frame is one
    /// sample of duration 1 at the sample rate, so `trun` carries no per-sample rows.
    Pcm {
        frame_bytes: u32,
    },
}

impl Kind {
    fn is_video(&self) -> bool {
        matches!(self, Kind::Video { .. } | Kind::Obu)
    }

    /// Bytes of this track's `traf` for `samples` packets.
    fn traf_size(&self, samples: usize) -> usize {
        match self {
            Kind::Pcm { .. } => TRAF_PCM,
            _ => TRAF_FIXED + TRUN_SAMPLE * samples,
        }
    }
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
    /// File offset of this track's 4-byte `tkhd` duration, patched at `finish`.
    duration_at: usize,
    /// Latest presentation end seen, in master-clock nanoseconds.
    present_end: Option<i128>,
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
    /// File offset of the 4-byte `mvhd` duration, patched at `finish`.
    movie_duration_at: usize,
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
fn keep_unit(codec: CodecId, unit: &[u8]) -> bool {
    !unit.is_empty() && !is_parameter_set(codec, unit) && !is_aud(codec, unit)
}

fn is_aud(codec: CodecId, unit: &[u8]) -> bool {
    match codec {
        CodecId::Hevc => nal::hevc_type(unit) == Some(hevc::AUD),
        _ => nal::h264_type(unit) == Some(h264::AUD),
    }
}

fn is_parameter_set(codec: CodecId, unit: &[u8]) -> bool {
    match codec {
        CodecId::Hevc => {
            matches!(
                nal::hevc_type(unit),
                Some(hevc::VPS | hevc::SPS | hevc::PPS)
            )
        }
        _ => matches!(nal::h264_type(unit), Some(h264::SPS | h264::PPS)),
    }
}

/// Refuses an in-band parameter set that differs from the sample entry's.
fn check_parameter_set(track: &Track, unit: &[u8]) -> Result<(), MuxError> {
    if is_parameter_set(track.codec, unit) && !track.parameter_sets.iter().any(|p| p == unit) {
        return Err(MuxError::InvalidPacket(
            "parameter sets changed; a fragmented file needs a new output".into(),
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

/// Length-prefixed HEVC whose extradata is an hvcC record.
fn hvcc_for(stream: &StreamDescriptor) -> Result<(Vec<u8>, Option<u8>), CreateError> {
    let invalid = |e| CreateError::InvalidConfig(format!("HEVC extradata: {e}"));
    match stream.bitstream {
        BitstreamFormat::LengthPrefixed { length_size } => {
            if !matches!(length_size, 1 | 2 | 4) {
                return Err(CreateError::InvalidConfig(format!(
                    "NAL length size {length_size}"
                )));
            }
            let hvcc = Hvcc::parse(stream.extradata.as_ref()).map_err(invalid)?;
            if hvcc.length_size != length_size {
                return Err(CreateError::InvalidConfig(
                    "hvcC must match the packets' NAL length size".into(),
                ));
            }
            Ok((stream.extradata.to_vec(), Some(length_size)))
        }
        other => Err(CreateError::Unsupported {
            what: format!("HEVC in {other:?} framing"),
        }),
    }
}

/// `avc1` sample entry with its `avcC` box.
fn avc1_entry(out: &mut Vec<u8>, width: u16, height: u16, avcc: &[u8]) {
    visual_entry(out, b"avc1", b"avcC", width, height, avcc);
}

/// `hvc1` sample entry with its `hvcC` box. Parameter sets live in the entry.
fn hvc1_entry(out: &mut Vec<u8>, width: u16, height: u16, hvcc: &[u8]) {
    visual_entry(out, b"hvc1", b"hvcC", width, height, hvcc);
}

fn visual_entry(
    out: &mut Vec<u8>,
    sample: &[u8; 4],
    config_box: &[u8; 4],
    width: u16,
    height: u16,
    config: &[u8],
) {
    let entry = boxes::open(out, sample);
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
    let record = config;
    let box_at = boxes::open(out, config_box);
    out.extend_from_slice(record);
    boxes::close(out, box_at);
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

/// QuickTime sound sample description version 2 (`lpcm`), the form Apple documents
/// for linear PCM at any rate and width: interleaved, packed, little-endian; signed
/// integer at 16 or 24 bits, float at 32 bits.
fn lpcm_entry(out: &mut Vec<u8>, channels: u16, rate: u32, bits: u16) {
    /// `formatSpecificFlags`: float, signed integer, packed (Core Audio's LPCM flags).
    const IS_FLOAT: u32 = 0x1;
    const IS_SIGNED_INTEGER: u32 = 0x4;
    const IS_PACKED: u32 = 0x8;
    let entry = boxes::open(out, b"lpcm");
    boxes::zeros(out, 6);
    boxes::u16(out, 1); // data_reference_index
    boxes::u16(out, 2); // version
    boxes::u16(out, 0); // revision level
    boxes::u32(out, 0); // vendor
    boxes::u16(out, 3); // always 3
    boxes::u16(out, 16); // always 16
    boxes::u16(out, 0xfffe); // always -2
    boxes::u16(out, 0); // always 0
    boxes::u32(out, 0x0001_0000); // always 65536
    boxes::u32(out, 72); // sizeOfStructOnly
    out.extend_from_slice(&f64::from(rate).to_bits().to_be_bytes());
    boxes::u32(out, u32::from(channels));
    boxes::u32(out, 0x7f00_0000); // always 0x7F000000
    boxes::u32(out, u32::from(bits)); // constBitsPerChannel
    let kind = if bits == 32 {
        IS_FLOAT
    } else {
        IS_SIGNED_INTEGER
    };
    boxes::u32(out, kind | IS_PACKED);
    boxes::u32(out, u32::from(channels) * u32::from(bits / 8)); // constBytesPerAudioPacket
    boxes::u32(out, 1); // constLPCMFramesPerAudioPacket
    boxes::close(out, entry);
}

/// PCM bit depth from the stream's extradata: one byte, 16, 24 or 32 (float); empty
/// means 16 (the convention of `rbs-codec-pcm` and `rbs-mux-wav`).
fn pcm_bits(extradata: &[u8]) -> Result<u16, CreateError> {
    match extradata {
        [] => Ok(16),
        [bits @ (16 | 24 | 32)] => Ok(u16::from(*bits)),
        _ => Err(CreateError::InvalidConfig(
            "pcm extradata must be one byte: 16, 24 or 32".into(),
        )),
    }
}

/// Writes one `trak` and returns the file-relative offset of its `tkhd` duration
/// within `out` (the caller adds where `out` itself is placed).
fn trak(
    out: &mut Vec<u8>,
    container: ContainerId,
    track: &Track,
    entry: &dyn Fn(&mut Vec<u8>),
    width: u16,
    height: u16,
) -> usize {
    let qt = container == ContainerId::Mov;
    let video = track.kind.is_video();
    let trak = boxes::open(out, b"trak");
    let tkhd = boxes::open_full(out, b"tkhd", 0, 3); // enabled, in movie
    boxes::zeros(out, 8); // creation, modification
    boxes::u32(out, track.id);
    boxes::u32(out, 0); // reserved
    let duration_at = out.len();
    boxes::u32(out, 0); // duration, patched at finish
    boxes::zeros(out, 8); // reserved
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
    // Left at 0. A 32-bit field at 90 kHz overflows past about 13 hours, which would
    // make a player that trusts it stop early. Fragments carry each sample's duration.
    boxes::u32(out, 0);
    boxes::u16(out, 0x55c4); // language "und"
    boxes::u16(out, 0);
    boxes::close(out, mdhd);
    let hdlr = boxes::open_full(out, b"hdlr", 0, 0);
    // QuickTime names the component type (`mhlr`, media handler) where ISO BMFF has
    // pre_defined 0, and writes the name as a Pascal string.
    out.extend_from_slice(if qt { b"mhlr" } else { &[0; 4] });
    out.extend_from_slice(if video { b"vide" } else { b"soun" });
    boxes::zeros(out, 12);
    out.extend_from_slice(if qt { b"\x03RBS" } else { b"RBS\0" });
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
    // Media in this file: QuickTime's self-reference is an `alis` entry with flag 1,
    // ISO BMFF's a `url ` entry with flag 1.
    let url = boxes::open_full(out, if qt { b"alis" } else { b"url " }, 0, 1);
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
    duration_at
}

impl Fmp4Muxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.is_empty() {
            return Err(CreateError::InvalidConfig("no streams".into()));
        }
        let container = config.container;
        let qt = container == ContainerId::Mov;
        // Names this file in error text.
        let name = if qt { "MOV" } else { "fragmented MP4" };
        let mut header = Vec::new();
        let ftyp = boxes::open(&mut header, b"ftyp");
        if qt {
            // QuickTime: major and only compatible brand `qt  `, minor version as
            // QuickTime writes it (a BCD date, 2005-03).
            header.extend_from_slice(b"qt  ");
            boxes::u32(&mut header, 0x2005_0300);
            header.extend_from_slice(b"qt  ");
        } else {
            header.extend_from_slice(b"iso6");
            boxes::u32(&mut header, 0);
            // Not `cmfc`: a CMAF track file has one track, and these fragments carry all.
            let av1 = config.streams.iter().any(|s| s.codec == CodecId::Av1);
            for brand in [b"iso6", b"isom", b"mp41"] {
                header.extend_from_slice(brand);
            }
            if av1 {
                header.extend_from_slice(b"av01");
            }
        }
        boxes::close(&mut header, ftyp);

        let mut tracks = Vec::with_capacity(config.streams.len());
        let mut traks = Vec::new();
        for (i, stream) in config.streams.iter().enumerate() {
            let id = i as u32 + 1;
            match (stream.codec, stream.params) {
                (
                    codec @ (CodecId::H264 | CodecId::Hevc),
                    StreamParams::Video {
                        format, frame_rate, ..
                    },
                ) => {
                    if tracks.iter().any(|t: &Track| t.kind.is_video()) {
                        // A keyframe chunk must be an entry point for every stream; with
                        // two video tracks whose GOPs differ, fragments rarely are.
                        return Err(CreateError::Unsupported {
                            what: format!("more than one video stream in {name}"),
                        });
                    }
                    let (config, length_size, parameter_sets, hevc) = if codec == CodecId::Hevc {
                        let (hvcc, length_size) = hvcc_for(stream)?;
                        let parameter_sets = Hvcc::parse(&hvcc)
                            .map(|c| c.parameter_sets.iter().map(|p| p.to_vec()).collect())
                            .map_err(|e| {
                                CreateError::InvalidConfig(format!("HEVC extradata: {e}"))
                            })?;
                        (hvcc, length_size, parameter_sets, true)
                    } else {
                        let (avcc, length_size) = avcc_for(stream)?;
                        let parameter_sets = AvcConfig::parse(&avcc)
                            .map(|c| c.sps.iter().chain(&c.pps).map(|p| p.to_vec()).collect())
                            .map_err(|e| {
                                CreateError::InvalidConfig(format!("H.264 extradata: {e}"))
                            })?;
                        (avcc, length_size, parameter_sets, false)
                    };
                    let (Ok(width), Ok(height)) = (
                        u16::try_from(format.size.width),
                        u16::try_from(format.size.height),
                    ) else {
                        return Err(CreateError::InvalidConfig("picture size".into()));
                    };
                    let mut track = Track {
                        id,
                        codec,
                        bitstream: stream.bitstream,
                        kind: Kind::Video { length_size },
                        timescale: VIDEO_TIMESCALE,
                        nominal: frame_rate.frame_duration(),
                        samples: Vec::new(),
                        staging: Vec::new(),
                        committed: 0,
                        parameter_sets,
                        last_dts: None,
                        duration_at: 0,
                        present_end: None,
                    };
                    track.duration_at = trak(
                        &mut traks,
                        container,
                        &track,
                        &|out| {
                            if hevc {
                                hvc1_entry(out, width, height, &config);
                            } else {
                                avc1_entry(out, width, height, &config);
                            }
                        },
                        width,
                        height,
                    );
                    tracks.push(track);
                }
                (CodecId::Av1, _) if qt => {
                    return Err(CreateError::Unsupported {
                        what: "av1 in MOV".into(),
                    });
                }
                (
                    CodecId::Av1,
                    StreamParams::Video {
                        format, frame_rate, ..
                    },
                ) => {
                    if tracks.iter().any(|t: &Track| t.kind.is_video()) {
                        return Err(CreateError::Unsupported {
                            what: format!("more than one video stream in {name}"),
                        });
                    }
                    if stream.bitstream != BitstreamFormat::Obu {
                        return Err(CreateError::InvalidConfig(format!(
                            "AV1 in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    let parsed = Av1Config::from_extradata(&stream.extradata)
                        .map_err(|e| CreateError::InvalidConfig(format!("AV1 extradata: {e}")))?;
                    let mut config_record = Vec::new();
                    parsed.build(&mut config_record);
                    let (Ok(width), Ok(height)) = (
                        u16::try_from(format.size.width),
                        u16::try_from(format.size.height),
                    ) else {
                        return Err(CreateError::InvalidConfig("picture size".into()));
                    };
                    let mut track = Track {
                        id,
                        codec: CodecId::Av1,
                        bitstream: BitstreamFormat::Obu,
                        kind: Kind::Obu,
                        timescale: VIDEO_TIMESCALE,
                        nominal: frame_rate.frame_duration(),
                        samples: Vec::new(),
                        staging: Vec::new(),
                        committed: 0,
                        parameter_sets: Vec::new(),
                        last_dts: None,
                        duration_at: 0,
                        present_end: None,
                    };
                    track.duration_at = trak(
                        &mut traks,
                        container,
                        &track,
                        &|out| visual_entry(out, b"av01", b"av1C", width, height, &config_record),
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
                    let mut track = Track {
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
                        duration_at: 0,
                        present_end: None,
                    };
                    let asc = stream.extradata.clone();
                    track.duration_at = trak(
                        &mut traks,
                        container,
                        &track,
                        &|out| mp4a_entry(out, channels, rate, &asc),
                        0,
                        0,
                    );
                    tracks.push(track);
                }
                (CodecId::Pcm, StreamParams::Audio { format, .. }) if qt => {
                    if stream.bitstream != BitstreamFormat::Raw {
                        return Err(CreateError::InvalidConfig(format!(
                            "PCM in {:?} framing",
                            stream.bitstream
                        )));
                    }
                    let bits = pcm_bits(&stream.extradata)?;
                    let rate = format.sample_rate.hz();
                    let channels = format.channels.channels() as u16;
                    let frame_bytes = u32::from(channels) * u32::from(bits / 8);
                    let mut track = Track {
                        id,
                        codec: CodecId::Pcm,
                        bitstream: BitstreamFormat::Raw,
                        kind: Kind::Pcm { frame_bytes },
                        timescale: rate,
                        // Unused: a PCM packet's length is its frame count.
                        nominal: Duration::ZERO,
                        samples: Vec::new(),
                        staging: Vec::new(),
                        committed: 0,
                        parameter_sets: Vec::new(),
                        last_dts: None,
                        duration_at: 0,
                        present_end: None,
                    };
                    track.duration_at = trak(
                        &mut traks,
                        container,
                        &track,
                        &|out| lpcm_entry(out, channels, rate, bits),
                        0,
                        0,
                    );
                    tracks.push(track);
                }
                (CodecId::Pcm, _) if qt => {
                    return Err(CreateError::InvalidConfig(format!(
                        "pcm stream with {:?} parameters",
                        stream.params
                    )));
                }
                (CodecId::H264 | CodecId::Hevc | CodecId::Av1 | CodecId::Aac, _) => {
                    return Err(CreateError::InvalidConfig(format!(
                        "{} stream with {:?} parameters",
                        stream.codec, stream.params
                    )));
                }
                (other, _) => {
                    return Err(CreateError::Unsupported {
                        what: format!("{other} in {name}"),
                    });
                }
            }
        }

        let moov = boxes::open(&mut header, b"moov");
        let mvhd = boxes::open_full(&mut header, b"mvhd", 0, 0);
        boxes::zeros(&mut header, 8);
        boxes::u32(&mut header, MOVIE_TIMESCALE);
        let movie_duration_at = header.len();
        boxes::u32(&mut header, 0); // duration, patched at finish
        boxes::u32(&mut header, 0x0001_0000); // rate
        boxes::u16(&mut header, 0x0100); // volume
        boxes::zeros(&mut header, 10);
        boxes::matrix(&mut header);
        boxes::zeros(&mut header, 24);
        boxes::u32(&mut header, tracks.len() as u32 + 1);
        boxes::close(&mut header, mvhd);
        let trak_at = header.len();
        header.extend_from_slice(&traks);
        for track in &mut tracks {
            track.duration_at += trak_at;
        }
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
            has_video: tracks.iter().any(|t| t.kind.is_video()),
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
            movie_duration_at,
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
                .map(|t| t.kind.traf_size(t.samples.len()))
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
            let base = self.media_time(track.samples[0].dts, track.timescale);
            if let Kind::Pcm { frame_bytes } = track.kind {
                // One sample per PCM frame, all with the defaults: duration 1 tick at
                // the sample rate, `frame_bytes` long, sync. The packets of a fragment
                // are laid end to end from the first one's time.
                let tfhd = boxes::open_full(
                    &mut chunk,
                    b"tfhd",
                    0,
                    DEFAULT_BASE_IS_MOOF | TFHD_PCM_DEFAULTS,
                );
                boxes::u32(&mut chunk, track.id);
                boxes::u32(&mut chunk, 1);
                boxes::u32(&mut chunk, frame_bytes);
                boxes::u32(&mut chunk, SYNC_SAMPLE);
                boxes::close(&mut chunk, tfhd);
                let tfdt = boxes::open_full(&mut chunk, b"tfdt", 1, 0);
                boxes::u64(&mut chunk, base);
                boxes::close(&mut chunk, tfdt);
                let bytes: usize = track.samples.iter().map(|s| s.size as usize).sum();
                let trun = boxes::open_full(&mut chunk, b"trun", 0, TRUN_PCM_FLAGS);
                // Fragment payload is below MAX_FRAGMENT_BYTES, so both fit 32 bits.
                boxes::u32(&mut chunk, (bytes / frame_bytes as usize) as u32);
                boxes::u32(&mut chunk, data_offset as u32);
                boxes::close(&mut chunk, trun);
                boxes::close(&mut chunk, traf);
                data_offset += bytes;
                continue;
            }
            let tfhd = boxes::open_full(&mut chunk, b"tfhd", 0, DEFAULT_BASE_IS_MOOF);
            boxes::u32(&mut chunk, track.id);
            boxes::close(&mut chunk, tfhd);
            let tfdt = boxes::open_full(&mut chunk, b"tfdt", 1, 0);
            boxes::u64(&mut chunk, base);
            boxes::close(&mut chunk, tfdt);
            let trun = boxes::open_full(&mut chunk, b"trun", 1, TRUN_FLAGS);
            boxes::u32(&mut chunk, track.samples.len() as u32);
            // Below 4 GiB: see MAX_SAMPLE_BYTES and MAX_FRAGMENT_BYTES.
            boxes::u32(&mut chunk, data_offset as u32);
            for (i, s) in track.samples.iter().enumerate() {
                let dts = self.media_time(s.dts, track.timescale);
                let end_ts = sample_end(&track.samples, i, following, track.nominal);
                let end = self.media_time(end_ts, track.timescale);
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
        note_present_ends(&mut self.tracks, next);

        // An entry point for every stream: audio samples all are, and there is at most
        // one video track (checked at create), whose first sample must be a keyframe.
        let keyframe = if self.has_video {
            self.tracks
                .iter()
                .find(|t| t.kind.is_video())
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
                    if keep_unit(track.codec, unit) {
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
            Kind::Obu => data.len(),
            Kind::Audio => data.len(),
            Kind::Pcm { frame_bytes } => {
                if !data.len().is_multiple_of(frame_bytes as usize) {
                    return Err(MuxError::InvalidPacket(
                        "PCM packet is not a whole number of frames".into(),
                    ));
                }
                data.len()
            }
        };
        if size > MAX_SAMPLE_BYTES {
            return Err(MuxError::Overflow("sample size"));
        }
        let keyframe = packet.keyframe();
        let is_video = track.kind.is_video();
        // A PCM packet lasts exactly its frames; the samples are laid end to end.
        let duration = match track.kind {
            Kind::Pcm { frame_bytes } => Some(Duration::from_nanos(
                u64::try_from(
                    u128::from((size / frame_bytes as usize) as u64) * 1_000_000_000
                        / u128::from(track.timescale),
                )
                .unwrap_or(u64::MAX),
            )),
            _ => packet.duration(),
        };
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
            duration,
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
        // Seekable outputs get the length (players and file browsers show it). Others
        // keep a valid live file whose duration fields stay 0.
        let Some(origin) = self.origin else {
            return Ok(());
        };
        let mut movie_end: Option<i128> = None;
        for track in &self.tracks {
            let Some(end) = track.present_end else {
                continue;
            };
            movie_end = Some(movie_end.map_or(end, |m| m.max(end)));
            out.push(duration_patch(
                track.duration_at,
                movie_duration(end, origin),
            ));
        }
        if let Some(end) = movie_end {
            out.push(duration_patch(
                self.movie_duration_at,
                movie_duration(end, origin),
            ));
        }
        Ok(())
    }
}

/// Decode end of sample `i`: the next sample's dts, the packet that closed the
/// fragment, or this sample's own duration.
fn sample_end(
    samples: &[Sample],
    i: usize,
    following: Option<Timestamp>,
    nominal: Duration,
) -> Timestamp {
    match samples.get(i + 1) {
        Some(next) => next.dts,
        None => {
            following.unwrap_or_else(|| samples[i].dts + samples[i].duration.unwrap_or(nominal))
        }
    }
}

/// One max per sample, on timestamps `flush` already holds. No allocation.
fn note_present_ends(tracks: &mut [Track], next: Option<(usize, Timestamp)>) {
    for (index, track) in tracks.iter_mut().enumerate() {
        let following = next.and_then(|(i, dts)| (i == index).then_some(dts));
        let mut shown = track.present_end;
        for (i, s) in track.samples.iter().enumerate() {
            let end_ts = sample_end(&track.samples, i, following, track.nominal);
            let delta = nanos(end_ts).saturating_sub(nanos(s.dts));
            let at = nanos(s.pts).saturating_add(delta);
            shown = Some(shown.map_or(at, |e| e.max(at)));
        }
        track.present_end = shown;
    }
}

/// `end - origin` in the movie timescale, saturating at the 32-bit field.
fn movie_duration(end: i128, origin: i128) -> u32 {
    let since = end - origin;
    if since <= 0 {
        return 0;
    }
    let ticks = ticks(since as u128, MOVIE_TIMESCALE);
    u32::try_from(ticks).unwrap_or(u32::MAX)
}

fn duration_patch(offset: usize, ticks: u32) -> MuxChunk {
    MuxChunk::new(
        ChunkKind::Patch {
            offset: offset as u64,
        },
        Bytes::copy_from_slice(&ticks.to_be_bytes()),
    )
}
