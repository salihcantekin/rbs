//! Checks the bytes the fragmented MP4 muxer writes with a small box reader.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk, StreamIndex,
};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{SYNTHETIC_ASC, SYNTHETIC_AVCC, aac_stream, h264_stream};
use rbs_mux::{CreateError, MuxError, MuxerConfig, MuxerFactory};
use rbs_mux_fmp4::Fmp4MuxerFactory;

const LP4: BitstreamFormat = BitstreamFormat::LengthPrefixed { length_size: 4 };
const MP4: ContainerId = ContainerId::Mp4Fragmented;

/// Container boxes whose payload is more boxes.
const CONTAINERS: [&[u8; 4]; 10] = [
    b"moov", b"trak", b"edts", b"mdia", b"minf", b"dinf", b"stbl", b"mvex", b"moof", b"traf",
];

#[derive(Debug)]
struct Box_ {
    kind: [u8; 4],
    /// Offset of the box start in the buffer it was read from.
    offset: usize,
    payload: Vec<u8>,
    children: Vec<Box_>,
}

impl Box_ {
    fn child(&self, kind: &[u8; 4]) -> &Box_ {
        self.children
            .iter()
            .find(|b| &b.kind == kind)
            .unwrap_or_else(|| panic!("no {} in {}", String::from_utf8_lossy(kind), self.name()))
    }

    fn all<'a>(&'a self, kind: &'a [u8; 4]) -> impl Iterator<Item = &'a Box_> {
        self.children.iter().filter(move |b| &b.kind == kind)
    }

    fn name(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_be_bytes(self.payload[at..at + 4].try_into().unwrap())
    }

    fn u64_at(&self, at: usize) -> u64 {
        u64::from_be_bytes(self.payload[at..at + 8].try_into().unwrap())
    }
}

fn parse(bytes: &[u8], base: usize) -> Vec<Box_> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(
            size >= 8 && pos + size <= bytes.len(),
            "box size {size} at {pos}"
        );
        let kind: [u8; 4] = bytes[pos + 4..pos + 8].try_into().unwrap();
        let payload = bytes[pos + 8..pos + size].to_vec();
        let children = if CONTAINERS.contains(&&kind) {
            parse(&payload, base + pos + 8)
        } else {
            Vec::new()
        };
        out.push(Box_ {
            kind,
            offset: base + pos,
            payload,
            children,
        });
        pos += size;
    }
    out
}

struct Run {
    track: u32,
    base: u64,
    data_offset: usize,
    /// (duration, size, flags, composition offset)
    samples: Vec<(u32, u32, u32, i32)>,
}

/// Reads one `moof` + `mdat` chunk: the track runs and the `mdat` payload.
fn fragment(chunk: &[u8]) -> (u32, Vec<Run>, Vec<u8>) {
    let boxes = parse(chunk, 0);
    assert_eq!(boxes.len(), 2, "a chunk is exactly one moof and one mdat");
    let (moof, mdat) = (&boxes[0], &boxes[1]);
    assert_eq!(&moof.kind, b"moof");
    assert_eq!(&mdat.kind, b"mdat");
    let sequence = moof.child(b"mfhd").u32_at(4);
    let runs = moof
        .all(b"traf")
        .map(|traf| {
            let tfhd = traf.child(b"tfhd");
            assert_eq!(tfhd.u32_at(0), 0x0002_0000, "default-base-is-moof");
            let tfdt = traf.child(b"tfdt");
            assert_eq!(tfdt.payload[0], 1);
            let trun = traf.child(b"trun");
            assert_eq!(trun.u32_at(0), 0x0100_0f01);
            let count = trun.u32_at(4) as usize;
            assert_eq!(trun.payload.len(), 12 + 16 * count);
            let samples = (0..count)
                .map(|i| {
                    let at = 12 + 16 * i;
                    (
                        trun.u32_at(at),
                        trun.u32_at(at + 4),
                        trun.u32_at(at + 8),
                        trun.u32_at(at + 12) as i32,
                    )
                })
                .collect();
            Run {
                track: tfhd.u32_at(4),
                base: tfdt.u64_at(4),
                data_offset: trun.u32_at(8) as usize,
                samples,
            }
        })
        .collect();
    // Data offsets are relative to the moof start (offset 0 of the chunk).
    assert_eq!(mdat.offset, moof.payload.len() + 8);
    (sequence, runs, mdat.payload.clone())
}

/// The bytes of every sample of `run`, read through its data offset.
fn sample_bytes<'a>(chunk: &'a [u8], run: &Run) -> Vec<&'a [u8]> {
    let mut at = run.data_offset;
    run.samples
        .iter()
        .map(|&(_, size, _, _)| {
            let s = &chunk[at..at + size as usize];
            at += size as usize;
            s
        })
        .collect()
}

fn at(ms: u64) -> Timestamp {
    Timestamp::from_nanos(7_000_000_000 + ms * 1_000_000)
}

fn video(
    pts: u64,
    dts: u64,
    keyframe: bool,
    data: Vec<u8>,
    format: BitstreamFormat,
) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::H264,
        format,
        at(pts),
        at(dts),
        keyframe,
        Bytes::from(data),
    )
    .unwrap()
}

fn audio(ms: u64) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        at(ms),
        at(ms),
        true,
        Bytes::from_static(&[1, 2, 3]),
    )
    .unwrap()
}

fn is_key(chunk: &MuxChunk) -> bool {
    matches!(chunk.kind, ChunkKind::Media { keyframe: true, .. })
}

#[test]
fn header_is_ftyp_and_moov_with_sample_entries() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, ChunkKind::Header);
    let boxes = parse(&chunks[0].data, 0);
    assert_eq!(
        boxes.iter().map(Box_::name).collect::<Vec<_>>(),
        ["ftyp", "moov"]
    );
    assert_eq!(&boxes[0].payload[..4], b"iso6");
    assert!(
        !boxes[0].payload.windows(4).any(|w| w == b"cmfc"),
        "not a CMAF track file"
    );
    let moov = &boxes[1];
    assert_eq!(moov.child(b"mvhd").u32_at(96), 3, "next track id");
    let traks: Vec<&Box_> = moov.all(b"trak").collect();
    assert_eq!(traks.len(), 2);
    let trex: Vec<u32> = moov
        .child(b"mvex")
        .all(b"trex")
        .map(|t| t.u32_at(4))
        .collect();
    assert_eq!(trex, [1, 2]);

    // No edit lists: media time 0 is the file's earliest sample. (FFmpeg reads an AAC
    // track's edit media time as encoder priming and skips that much audio.)
    assert!(traks.iter().all(|t| t.all(b"edts").next().is_none()));

    let mdia = traks[0].child(b"mdia");
    assert_eq!(mdia.child(b"mdhd").u32_at(12), 90_000, "video timescale");
    assert_eq!(&mdia.child(b"hdlr").payload[8..12], b"vide");
    let stsd = &mdia.child(b"minf").child(b"stbl").child(b"stsd").payload;
    assert_eq!(&stsd[12..16], b"avc1");
    let avc1 = &stsd[8..];
    assert_eq!(u16::from_be_bytes([avc1[32], avc1[33]]), 320);
    assert_eq!(u16::from_be_bytes([avc1[34], avc1[35]]), 240);
    let avcc = parse(&avc1[8 + 78..], 0);
    assert_eq!(&avcc[0].kind, b"avcC");
    assert_eq!(avcc[0].payload, SYNTHETIC_AVCC);

    let mdia = traks[1].child(b"mdia");
    assert_eq!(mdia.child(b"mdhd").u32_at(12), 48_000, "audio timescale");
    assert_eq!(&mdia.child(b"hdlr").payload[8..12], b"soun");
    let stsd = &mdia.child(b"minf").child(b"stbl").child(b"stsd").payload;
    assert_eq!(&stsd[12..16], b"mp4a");
    let mp4a = &stsd[8..];
    assert_eq!(u16::from_be_bytes([mp4a[24], mp4a[25]]), 2, "channels");
    let esds = parse(&mp4a[8 + 28..], 0);
    assert_eq!(&esds[0].kind, b"esds");
    let tail = &esds[0].payload[esds[0].payload.len() - 3 - SYNTHETIC_ASC.len()..];
    assert_eq!(&tail[..SYNTHETIC_ASC.len()], SYNTHETIC_ASC);
    assert_eq!(&tail[SYNTHETIC_ASC.len()..], &[6, 1, 2]);
}

#[test]
fn fragments_start_at_keyframes_with_times_and_offsets() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let key = video(100, 0, true, vec![0, 0, 0, 2, 0x65, 9], LP4);
    muxer.write(StreamIndex(0), &key, &mut chunks).unwrap();
    muxer
        .write(StreamIndex(1), &audio(21), &mut chunks)
        .unwrap();
    let p = video(33, 33, false, vec![0, 0, 0, 2, 0x41, 8], LP4);
    muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    assert_eq!(
        chunks.len(),
        1,
        "samples are collected until the fragment ends"
    );
    let next_key = video(166, 66, true, vec![0, 0, 0, 2, 0x65, 7], LP4);
    muxer.write(StreamIndex(0), &next_key, &mut chunks).unwrap();
    assert_eq!(chunks.len(), 2, "a video keyframe ends the fragment");
    muxer.finish(&mut chunks).unwrap();
    assert_eq!(
        chunks.len(),
        3,
        "finish flushes the last fragment, no trailer"
    );
    assert!(chunks[1..].iter().all(is_key));
    assert_eq!(
        chunks[1].kind,
        ChunkKind::Media {
            keyframe: true,
            timestamp: at(0)
        }
    );

    let (sequence, runs, _) = fragment(&chunks[1].data);
    assert_eq!(sequence, 1);
    assert_eq!(runs.len(), 2);
    let (v, a) = (&runs[0], &runs[1]);
    assert_eq!((v.track, a.track), (1, 2));
    // The first fragment ended at 66 ms, less than the interleaving bound (1 s) after
    // the first dts: media time 0 is 1 s before that newest dts, so a packet arriving
    // within the bound still fits.
    assert_eq!(v.base, 934 * 90);
    assert_eq!(a.base, 955 * 48);
    assert_eq!(
        v.samples,
        [
            (33 * 90, 6, 0x0200_0000, 100 * 90),
            (33 * 90, 6, 0x0101_0000, 0),
        ],
        "durations from the next dts, then the last one up to the next fragment"
    );
    assert_eq!(a.samples, [(1024, 3, 0x0200_0000, 0)]);
    let video_bytes = sample_bytes(&chunks[1].data, v);
    assert_eq!(
        video_bytes,
        [&[0, 0, 0, 2, 0x65, 9][..], &[0, 0, 0, 2, 0x41, 8]]
    );
    assert_eq!(sample_bytes(&chunks[1].data, a), [&[1, 2, 3][..]]);

    let (sequence, runs, mdat) = fragment(&chunks[2].data);
    assert_eq!(sequence, 2);
    assert_eq!(runs.len(), 1, "a track without samples has no traf");
    assert_eq!(runs[0].base, (934 + 66) * 90);
    assert_eq!(runs[0].samples, [(3000, 6, 0x0200_0000, 100 * 90)]);
    assert_eq!(mdat, [0, 0, 0, 2, 0x65, 7]);
}

#[test]
fn media_time_starts_at_the_earliest_sample_when_the_first_fragment_is_full() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Audio first, at 10 ms; video from 30 ms, one keyframe (a 2 s GOP).
    let mut ms_audio = 10;
    for i in 0..40u64 {
        let ms = 30 + i * 1000 / 30;
        while ms_audio <= ms {
            muxer
                .write(StreamIndex(1), &audio(ms_audio), &mut chunks)
                .unwrap();
            ms_audio += 21;
        }
        let data = vec![0, 0, 0, 2, if i == 0 { 0x65 } else { 0x41 }, i as u8];
        muxer
            .write(
                StreamIndex(0),
                &video(ms, ms, i == 0, data, LP4),
                &mut chunks,
            )
            .unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let (_, runs, _) = fragment(&chunks[1].data);
    let v = runs.iter().find(|r| r.track == 1).unwrap();
    let a = runs.iter().find(|r| r.track == 2).unwrap();
    assert_eq!(a.base, 0, "the earliest sample is at media time 0");
    assert_eq!(v.base, 20 * 90, "video starts 20 ms after audio");
}

#[test]
fn packets_before_the_start_are_refused_once_the_first_fragment_is_out() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for ms in (100..1200).step_by(21) {
        muxer
            .write(StreamIndex(1), &audio(ms), &mut chunks)
            .unwrap();
    }
    assert_eq!(chunks.len(), 2, "the first fragment is out");
    let (_, runs, _) = fragment(&chunks[1].data);
    assert_eq!(runs[0].base, 0);
    let early = video(50, 50, true, vec![0, 0, 0, 2, 0x65, 1], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &early, &mut chunks),
        Err(MuxError::InvalidPacket(_))
    ));
    let on_time = video(100, 100, true, vec![0, 0, 0, 2, 0x65, 1], LP4);
    muxer.write(StreamIndex(0), &on_time, &mut chunks).unwrap();
}

#[test]
fn long_gops_are_split_at_the_fragment_duration() {
    let config = MuxerConfig::new(MP4, vec![h264_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for i in 0..75u64 {
        let ms = i * 1000 / 30;
        let data = vec![0, 0, 0, 2, if i == 0 { 0x65 } else { 0x41 }, i as u8];
        muxer
            .write(
                StreamIndex(0),
                &video(ms, ms, i == 0, data, LP4),
                &mut chunks,
            )
            .unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let keys: Vec<bool> = chunks[1..].iter().map(is_key).collect();
    assert_eq!(keys, [true, false, false]);
    let counts: Vec<usize> = chunks[1..]
        .iter()
        .map(|c| fragment(&c.data).1[0].samples.len())
        .collect();
    assert_eq!(counts, [30, 30, 15]);
    // Non-sync samples of a non-keyframe fragment are marked as such.
    let (_, runs, _) = fragment(&chunks[2].data);
    assert!(runs[0].samples.iter().all(|s| s.2 == 0x0101_0000));
}

#[test]
fn streams_arriving_out_of_order_keep_their_timestamps() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Audio first; then video whose dts is 66 ms earlier (B-frame reorder delay).
    muxer
        .write(StreamIndex(1), &audio(100), &mut chunks)
        .unwrap();
    for (pts, dts) in [(100, 34), (200, 67), (133, 100)] {
        let p = video(pts, dts, dts == 34, vec![0, 0, 0, 2, 0x41, 1], LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 2, "one fragment: the keyframe arrived first");
    // Fragment timestamp is the earliest dts in it.
    assert_eq!(
        chunks[1].kind,
        ChunkKind::Media {
            keyframe: true,
            timestamp: at(34)
        }
    );
    let (_, runs, _) = fragment(&chunks[1].data);
    let v = runs.iter().find(|r| r.track == 1).unwrap();
    // The only fragment spans less than 1 s: media time 0 is 1 s before the newest dts
    // (100 ms).
    assert_eq!(v.base, 934 * 90);
    let cts: Vec<i32> = v.samples.iter().map(|s| s.3).collect();
    assert_eq!(cts, [66 * 90, 133 * 90, 33 * 90]);
    let a = runs.iter().find(|r| r.track == 2).unwrap();
    assert_eq!(a.base, 1000 * 48);

    // Earlier than the origin is refused, not clamped.
    let mut fresh = Fmp4MuxerFactory.create(&config).unwrap();
    fresh.begin(&mut Vec::new()).unwrap();
    fresh
        .write(StreamIndex(1), &audio(2000), &mut Vec::new())
        .unwrap();
    let way_early = video(0, 0, false, vec![0, 0, 0, 2, 0x41, 1], LP4);
    assert!(matches!(
        fresh.write(StreamIndex(0), &way_early, &mut Vec::new()),
        Err(MuxError::InvalidPacket(_))
    ));
}

#[test]
fn annexb_input_is_converted_and_parameter_sets_dropped() {
    let mut stream = h264_stream();
    stream.bitstream = BitstreamFormat::AnnexB;
    let config = MuxerConfig::new(MP4, vec![stream]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let au = vec![
        0, 0, 1, 0x09, 0xf0, // AUD
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, // SPS
        0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, // PPS (both as in the avcC)
        0, 0, 0, 1, 0x06, 5, // SEI
        0, 0, 1, 0x65, 1, 2, // IDR
    ];
    muxer
        .write(
            StreamIndex(0),
            &video(0, 0, true, au, BitstreamFormat::AnnexB),
            &mut chunks,
        )
        .unwrap();
    muxer.finish(&mut chunks).unwrap();
    let (_, runs, mdat) = fragment(&chunks[1].data);
    assert_eq!(mdat, [0, 0, 0, 2, 0x06, 5, 0, 0, 0, 3, 0x65, 1, 2]);
    assert_eq!(runs[0].samples[0].1 as usize, mdat.len());
    // The sample entry still carries the parameter sets.
    let header = parse(&chunks[0].data, 0);
    let stsd = &header[1]
        .child(b"trak")
        .child(b"mdia")
        .child(b"minf")
        .child(b"stbl")
        .child(b"stsd")
        .payload;
    assert!(
        stsd.windows(SYNTHETIC_AVCC.len())
            .any(|w| w == SYNTHETIC_AVCC)
    );
}

#[test]
fn audio_only_output_has_keyframe_fragments() {
    let config = MuxerConfig::new(MP4, vec![aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for i in 0..100 {
        muxer
            .write(StreamIndex(0), &audio(i * 21), &mut chunks)
            .unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    assert!(chunks.len() > 2, "fragments by duration");
    assert!(chunks[1..].iter().all(is_key));
}

#[test]
fn rejects_bad_configs_and_packets() {
    let f = Fmp4MuxerFactory;
    let config = |streams| MuxerConfig::new(MP4, streams);
    assert!(matches!(
        f.create(&MuxerConfig::new(ContainerId::Flv, vec![h264_stream()])),
        Err(CreateError::Unsupported { .. })
    ));
    assert!(matches!(
        f.create(&config(vec![])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut wrong_length = h264_stream();
    wrong_length.bitstream = BitstreamFormat::LengthPrefixed { length_size: 2 };
    assert!(matches!(
        f.create(&config(vec![wrong_length])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut annexb_without_sets = h264_stream();
    annexb_without_sets.bitstream = BitstreamFormat::AnnexB;
    annexb_without_sets.extradata = Bytes::new();
    assert!(matches!(
        f.create(&config(vec![annexb_without_sets])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut bad_asc = aac_stream();
    bad_asc.extradata = Bytes::new();
    assert!(matches!(
        f.create(&config(vec![bad_asc])),
        Err(CreateError::InvalidConfig(_))
    ));

    let mut muxer = f
        .create(&config(vec![h264_stream(), aac_stream()]))
        .unwrap();
    let mut out = Vec::new();
    let packet = video(0, 0, true, vec![0, 0, 0, 1, 0x65], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &packet, &mut out),
        Err(MuxError::InvalidState(_))
    ));
    muxer.begin(&mut out).unwrap();
    assert!(matches!(
        muxer.begin(&mut out),
        Err(MuxError::InvalidState(_))
    ));
    assert!(matches!(
        muxer.write(StreamIndex(2), &packet, &mut out),
        Err(MuxError::UnknownStream(2))
    ));
    assert!(matches!(
        muxer.write(StreamIndex(1), &packet, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    let truncated = video(0, 0, true, vec![0, 0, 0, 9, 0x65], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &truncated, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
    let earlier = EncodedPacket::new(
        CodecId::H264,
        LP4,
        at(0) - std::time::Duration::from_millis(1),
        at(0) - std::time::Duration::from_millis(1),
        false,
        Bytes::from_static(&[0, 0, 0, 1, 0x41]),
    )
    .unwrap();
    let err = muxer.write(StreamIndex(0), &earlier, &mut out).unwrap_err();
    assert!(matches!(err, MuxError::NonMonotonic { stream: 0 }) && err.is_recoverable());
    let empty = video(5, 5, false, vec![], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &empty, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    muxer.finish(&mut out).unwrap();
    assert!(matches!(
        muxer.write(StreamIndex(0), &packet, &mut out),
        Err(MuxError::InvalidState(_))
    ));
    assert!(matches!(
        muxer.finish(&mut out),
        Err(MuxError::InvalidState(_))
    ));
}

#[test]
fn changed_parameter_sets_and_bad_annexb_leave_no_output() {
    for format in [LP4, BitstreamFormat::AnnexB] {
        let mut stream = h264_stream();
        stream.bitstream = format;
        let config = MuxerConfig::new(MP4, vec![stream]);
        let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
        let mut chunks = Vec::new();
        muxer.begin(&mut chunks).unwrap();
        let (key, other_sps, empty_units) = if format == LP4 {
            (
                vec![0, 0, 0, 2, 0x65, 1],
                vec![0, 0, 0, 4, 0x67, 0x42, 0, 0x1e, 0, 0, 0, 2, 0x65, 1],
                None,
            )
        } else {
            (
                vec![0, 0, 1, 0x65, 1],
                vec![0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 1, 0x65, 1],
                Some(vec![0, 0, 1, 0, 0, 1, 0x09, 0x10]),
            )
        };
        muxer
            .write(
                StreamIndex(0),
                &video(0, 0, true, key.clone(), format),
                &mut chunks,
            )
            .unwrap();
        // Both would end the fragment (keyframes); the error must come first.
        let changed = video(33, 33, true, other_sps, format);
        assert!(matches!(
            muxer.write(StreamIndex(0), &changed, &mut chunks),
            Err(MuxError::InvalidPacket(_))
        ));
        if let Some(units) = empty_units {
            let empty = video(33, 33, true, units, format);
            assert!(matches!(
                muxer.write(StreamIndex(0), &empty, &mut chunks),
                Err(MuxError::InvalidPacket(_))
            ));
        }
        assert_eq!(chunks.len(), 1, "no fragment was emitted by a failed write");
        muxer
            .write(
                StreamIndex(0),
                &video(66, 66, true, key, format),
                &mut chunks,
            )
            .unwrap();
        muxer.finish(&mut chunks).unwrap();
        let samples: Vec<usize> = chunks[1..]
            .iter()
            .map(|c| fragment(&c.data).1[0].samples.len())
            .collect();
        assert_eq!(samples, [1, 1], "only the good packets, one per fragment");
        let (_, _, mdat) = fragment(&chunks[2].data);
        assert_eq!(
            mdat,
            [0, 0, 0, 2, 0x65, 1],
            "the pending packet survived the flush"
        );
    }
}

#[test]
fn a_second_video_stream_is_refused() {
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream(), h264_stream()]);
    assert!(matches!(
        Fmp4MuxerFactory.create(&config),
        Err(CreateError::Unsupported { .. })
    ));
}

/// Real clocks are not whole milliseconds: 30 fps frames and 1024-sample AAC frames
/// fall between nanoseconds. Durations must still be constant and fragments must
/// continue exactly where the previous one ended.
#[test]
fn rounded_positions_give_exact_durations_and_continuous_fragments() {
    use rbs_core::timing::{Cadence, FrameRate, SampleRate};
    let config = MuxerConfig::new(MP4, vec![h264_stream(), aac_stream()]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let origin = Timestamp::from_nanos(3_000_000_007);
    let frames = Cadence::frames(origin, FrameRate::FPS_30);
    let mut packets = Vec::new();
    for i in 0..90u64 {
        let dts = frames.time_of(i);
        // B-frame style: pts two frames after dts.
        let pts = frames.time_of(i + 2);
        let key = i % 30 == 0;
        let data = Bytes::from(vec![0, 0, 0, 2, if key { 0x65 } else { 0x41 }, i as u8]);
        let p = EncodedPacket::new(CodecId::H264, LP4, pts, dts, key, data).unwrap();
        packets.push((StreamIndex(0), p));
    }
    for i in 0..140u64 {
        let t = origin + SampleRate::HZ_48000.duration_of(i * 1024);
        let p = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from_static(&[1, 2, 3]),
        )
        .unwrap();
        packets.push((StreamIndex(1), p));
    }
    packets.sort_by_key(|(s, p)| (p.dts(), *s));
    for (s, p) in &packets {
        muxer.write(*s, p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();

    let mut next_base = [None, None];
    let mut durations = [Vec::new(), Vec::new()];
    for c in &chunks[1..] {
        for run in fragment(&c.data).1 {
            let t = run.track as usize - 1;
            if let Some(expected) = next_base[t] {
                assert_eq!(run.base, expected, "track {} continues exactly", run.track);
            }
            next_base[t] = Some(run.base + run.samples.iter().map(|s| u64::from(s.0)).sum::<u64>());
            durations[t].extend(run.samples.iter().map(|s| s.0));
            if t == 0 {
                assert!(run.samples.iter().all(|s| s.3 == 6000), "two frames of CTS");
            }
        }
    }
    // 30 fps at 90 kHz is exactly 3000 ticks; AAC frames are exactly 1024.
    assert!(
        durations[0].iter().all(|&d| d == 3000),
        "{:?}",
        durations[0]
    );
    assert!(
        durations[1].iter().all(|&d| d == 1024),
        "{:?}",
        durations[1]
    );
    assert_eq!(durations[0].len(), 90);
    assert_eq!(durations[1].len(), 140);
}
