//! Checks the bytes the Matroska muxer writes with a small EBML reader.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk, StreamIndex,
};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{SYNTHETIC_ASC, SYNTHETIC_AVCC, aac_stream, h264_stream};
use rbs_mux::{CreateError, MuxError, MuxerConfig, MuxerFactory};
use rbs_mux_matroska::MatroskaMuxerFactory;

const LP4: BitstreamFormat = BitstreamFormat::LengthPrefixed { length_size: 4 };
const MKV: ContainerId = ContainerId::Matroska;

const SEGMENT: u64 = 0x1853_8067;
const CLUSTER: u64 = 0x1f43_b675;
/// Master elements whose payload is more elements.
const MASTERS: [u64; 8] = [
    0x1a45_dfa3,
    SEGMENT,
    0x1549_a966,
    0x1654_ae6b,
    0xae,
    0xe0,
    0xe1,
    CLUSTER,
];

#[derive(Debug)]
struct Element {
    id: u64,
    data: Vec<u8>,
    children: Vec<Element>,
}

impl Element {
    fn child(&self, id: u64) -> &Element {
        self.children
            .iter()
            .find(|e| e.id == id)
            .unwrap_or_else(|| panic!("no {id:#x} in {:#x}", self.id))
    }

    fn all(&self, id: u64) -> impl Iterator<Item = &Element> {
        self.children.iter().filter(move |e| e.id == id)
    }

    fn uint(&self) -> u64 {
        self.data.iter().fold(0, |v, b| (v << 8) | u64::from(*b))
    }
}

/// (length, value) of the variable-length integer at `at`; `keep_marker` for ids.
fn vint(bytes: &[u8], at: usize, keep_marker: bool) -> (usize, u64) {
    let first = bytes[at];
    let len = first.leading_zeros() as usize + 1;
    assert!(len <= 8, "bad vint at {at}");
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first) & ((1 << (8 - len)) - 1)
    };
    for b in &bytes[at + 1..at + len] {
        value = (value << 8) | u64::from(*b);
    }
    (len, value)
}

fn parse(bytes: &[u8]) -> Vec<Element> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        let (id_len, id) = vint(bytes, pos, true);
        let (size_len, size) = vint(bytes, pos + id_len, false);
        let start = pos + id_len + size_len;
        let unknown = size == (1 << (7 * size_len)) - 1;
        let end = if unknown {
            bytes.len()
        } else {
            start + size as usize
        };
        assert!(end <= bytes.len(), "element {id:#x} overruns");
        let data = bytes[start..end].to_vec();
        let children = if MASTERS.contains(&id) {
            parse(&data)
        } else {
            Vec::new()
        };
        out.push(Element { id, data, children });
        pos = end;
    }
    out
}

struct SimpleBlock {
    track: u64,
    relative: i16,
    keyframe: bool,
    data: Vec<u8>,
}

/// Reads one Cluster chunk: (cluster timestamp, blocks).
fn cluster(chunk: &[u8]) -> (u64, Vec<SimpleBlock>) {
    let elements = parse(chunk);
    assert_eq!(elements.len(), 1, "a chunk is exactly one Cluster");
    let c = &elements[0];
    assert_eq!(c.id, CLUSTER);
    assert_eq!(c.children[0].id, 0xe7, "Timestamp first");
    let blocks = c
        .all(0xa3)
        .map(|b| {
            let (len, track) = vint(&b.data, 0, false);
            SimpleBlock {
                track,
                relative: i16::from_be_bytes([b.data[len], b.data[len + 1]]),
                keyframe: b.data[len + 2] & 0x80 != 0,
                data: b.data[len + 3..].to_vec(),
            }
        })
        .collect();
    (c.children[0].uint(), blocks)
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
fn header_has_ebml_info_and_tracks_in_a_live_segment() {
    let config = MuxerConfig::new(MKV, vec![h264_stream(), aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, ChunkKind::Header);
    let bytes = &chunks[0].data;
    let top = parse(bytes);
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].child(0x4282).data, b"matroska");
    assert_eq!(top[1].id, SEGMENT);
    let segment_size = &bytes[bytes.len() - top[1].data.len() - 8..][..8];
    assert_eq!(
        segment_size,
        [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        "unknown size"
    );
    let info = top[1].child(0x1549_a966);
    assert_eq!(info.child(0x2a_d7b1).uint(), 1_000_000, "milliseconds");
    let entries: Vec<&Element> = top[1].child(0x1654_ae6b).all(0xae).collect();
    assert_eq!(entries.len(), 2);
    let v = entries[0];
    assert_eq!(v.child(0xd7).uint(), 1);
    assert_eq!(v.child(0x83).uint(), 1, "video");
    assert_eq!(v.child(0x86).data, b"V_MPEG4/ISO/AVC");
    assert_eq!(v.child(0x63a2).data, SYNTHETIC_AVCC);
    assert_eq!(v.child(0x23_e383).uint(), 33_333_333, "30 fps");
    assert_eq!(v.child(0xe0).child(0xb0).uint(), 320);
    assert_eq!(v.child(0xe0).child(0xba).uint(), 240);
    let a = entries[1];
    assert_eq!(a.child(0xd7).uint(), 2);
    assert_eq!(a.child(0x83).uint(), 2, "audio");
    assert_eq!(a.child(0x86).data, b"A_AAC");
    assert_eq!(a.child(0x63a2).data, SYNTHETIC_ASC);
    let rate = f64::from_bits(a.child(0xe1).child(0xb5).uint());
    assert_eq!(rate, 48_000.0);
    assert_eq!(a.child(0xe1).child(0x9f).uint(), 2);
}

#[test]
fn clusters_start_at_keyframes_in_decode_order() {
    let config = MuxerConfig::new(MKV, vec![h264_stream(), aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Audio at 21 ms arrives after video at 33 ms: blocks are sorted by dts.
    let key = video(0, 0, true, vec![0, 0, 0, 2, 0x65, 9], LP4);
    muxer.write(StreamIndex(0), &key, &mut chunks).unwrap();
    let p = video(33, 33, false, vec![0, 0, 0, 2, 0x41, 8], LP4);
    muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    muxer
        .write(StreamIndex(1), &audio(21), &mut chunks)
        .unwrap();
    assert_eq!(
        chunks.len(),
        1,
        "blocks are collected until the cluster ends"
    );
    let next_key = video(66, 66, true, vec![0, 0, 0, 2, 0x65, 7], LP4);
    muxer.write(StreamIndex(0), &next_key, &mut chunks).unwrap();
    assert_eq!(chunks.len(), 2, "a video keyframe ends the cluster");
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    assert_eq!(chunks.len(), 3, "finish emits the last cluster, no trailer");
    assert!(chunks[1..].iter().all(is_key));
    assert_eq!(
        chunks[1].kind,
        ChunkKind::Media {
            keyframe: true,
            timestamp: at(0)
        }
    );

    // Time 0 is one interleaving bound (1 s) before the first dts.
    let (base, blocks) = cluster(&chunks[1].data);
    assert_eq!(base, 1000);
    let order: Vec<(u64, i16, bool)> = blocks
        .iter()
        .map(|b| (b.track, b.relative, b.keyframe))
        .collect();
    assert_eq!(order, [(1, 0, true), (2, 21, true), (1, 33, false)]);
    assert_eq!(blocks[0].data, [0, 0, 0, 2, 0x65, 9]);
    assert_eq!(blocks[1].data, [1, 2, 3]);
    let (base, blocks) = cluster(&chunks[2].data);
    assert_eq!(base, 1066);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].data, [0, 0, 0, 2, 0x65, 7]);
}

#[test]
fn block_timestamps_are_presentation_times() {
    let config = MuxerConfig::new(MKV, vec![h264_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // I P B B in decode order: pts 66, 166, 100, 133 with dts 0, 33, 66, 100.
    for (pts, dts, key) in [
        (66, 0, true),
        (166, 33, false),
        (100, 66, false),
        (133, 100, false),
    ] {
        let t = if key { 0x65 } else { 0x41 };
        let p = video(pts, dts, key, vec![0, 0, 0, 2, t, pts as u8], LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    let (base, blocks) = cluster(&chunks[1].data);
    assert_eq!(base, 1066, "the earliest pts");
    let rel: Vec<i16> = blocks.iter().map(|b| b.relative).collect();
    assert_eq!(rel, [0, 100, 34, 67], "decode order, presentation times");
}

#[test]
fn long_gops_are_split_at_the_cluster_duration() {
    let config = MuxerConfig::new(MKV, vec![h264_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
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
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    let keys: Vec<bool> = chunks[1..].iter().map(is_key).collect();
    assert_eq!(keys, [true, false, false]);
    let counts: Vec<usize> = chunks[1..]
        .iter()
        .map(|c| cluster(&c.data).1.len())
        .collect();
    assert_eq!(counts, [30, 30, 15]);
    assert!(cluster(&chunks[2].data).1.iter().all(|b| !b.keyframe));
}

#[test]
fn streams_arriving_out_of_order_keep_their_timestamps() {
    let config = MuxerConfig::new(MKV, vec![h264_stream(), aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Audio first; then video whose dts is 66 ms earlier.
    muxer
        .write(StreamIndex(1), &audio(100), &mut chunks)
        .unwrap();
    for (pts, dts) in [(34, 34), (67, 67)] {
        let p = video(pts, dts, dts == 34, vec![0, 0, 0, 2, 0x41, 1], LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    assert_eq!(
        chunks[1].kind,
        ChunkKind::Media {
            keyframe: true,
            timestamp: at(34)
        }
    );
    // Origin: the first dts (audio at 100 ms) minus 1 s.
    let (base, blocks) = cluster(&chunks[1].data);
    assert_eq!(base, 934);
    let order: Vec<(u64, i16)> = blocks.iter().map(|b| (b.track, b.relative)).collect();
    assert_eq!(order, [(1, 0), (1, 33), (2, 66)]);

    let mut fresh = MatroskaMuxerFactory.create(&config).unwrap();
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
fn annexb_input_is_converted_and_bad_input_leaves_no_output() {
    let mut stream = h264_stream();
    stream.bitstream = BitstreamFormat::AnnexB;
    let config = MuxerConfig::new(MKV, vec![stream]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let au = vec![
        0, 0, 1, 0x09, 0xf0, // AUD
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, // SPS
        0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, // PPS (both as in the avcC)
        0, 0, 0, 1, 0x06, 5, // SEI
        0, 0, 1, 0x65, 1, 2, // IDR
    ];
    let annexb = BitstreamFormat::AnnexB;
    muxer
        .write(StreamIndex(0), &video(0, 0, true, au, annexb), &mut chunks)
        .unwrap();
    // A changed SPS and an access unit of empty units: both keyframes that would end
    // the cluster, both refused before anything is emitted.
    let changed = video(
        33,
        33,
        true,
        vec![0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 1, 0x65, 1],
        annexb,
    );
    let empty = video(33, 33, true, vec![0, 0, 1, 0, 0, 1, 0x09, 0x10], annexb);
    for bad in [changed, empty] {
        assert!(matches!(
            muxer.write(StreamIndex(0), &bad, &mut chunks),
            Err(MuxError::InvalidPacket(_))
        ));
    }
    assert_eq!(chunks.len(), 1);
    let next = video(66, 66, true, vec![0, 0, 1, 0x65, 3], annexb);
    muxer.write(StreamIndex(0), &next, &mut chunks).unwrap();
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    let blocks: Vec<Vec<u8>> = chunks[1..]
        .iter()
        .flat_map(|c| cluster(&c.data).1)
        .map(|b| b.data)
        .collect();
    assert_eq!(
        blocks,
        [
            vec![0, 0, 0, 2, 0x06, 5, 0, 0, 0, 3, 0x65, 1, 2],
            vec![0, 0, 0, 2, 0x65, 3],
        ],
        "AUD and parameter sets dropped; the pending unit survived the flush"
    );
}

#[test]
fn audio_only_output_has_keyframe_clusters() {
    let config = MuxerConfig::new(MKV, vec![aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for i in 0..100 {
        muxer
            .write(StreamIndex(0), &audio(i * 21), &mut chunks)
            .unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    assert!(chunks.len() > 2, "clusters by duration");
    assert!(chunks[1..].iter().all(is_key));
    assert!(
        chunks[1..]
            .iter()
            .flat_map(|c| cluster(&c.data).1)
            .all(|b| b.keyframe)
    );
}

#[test]
fn rejects_bad_configs_and_packets() {
    let f = MatroskaMuxerFactory;
    let config = |streams| MuxerConfig::new(MKV, streams);
    assert!(matches!(
        f.create(&MuxerConfig::new(ContainerId::Flv, vec![h264_stream()])),
        Err(CreateError::Unsupported { .. })
    ));
    assert!(matches!(
        f.create(&config(vec![])),
        Err(CreateError::InvalidConfig(_))
    ));
    assert!(matches!(
        f.create(&config(vec![h264_stream(), h264_stream()])),
        Err(CreateError::Unsupported { .. })
    ));
    let mut wrong_length = h264_stream();
    wrong_length.bitstream = BitstreamFormat::LengthPrefixed { length_size: 2 };
    assert!(matches!(
        f.create(&config(vec![wrong_length])),
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
    let changed_sps = video(0, 0, true, vec![0, 0, 0, 2, 0x67, 1, 0, 0, 0, 1, 0x65], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &changed_sps, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    let late_pts = video(20_000, 0, true, vec![0, 0, 0, 1, 0x65], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &late_pts, &mut out),
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

/// Every block timestamp of every cluster, absolute (cluster + relative), in ms.
fn block_times(chunks: &[MuxChunk]) -> Vec<(u64, i64)> {
    chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Media { .. }))
        .flat_map(|c| {
            let (base, blocks) = cluster(&c.data);
            blocks
                .into_iter()
                .map(move |b| (b.track, base as i64 + i64::from(b.relative)))
        })
        .collect()
}

#[test]
fn a_lagging_stream_never_overflows_block_timestamps() {
    let config = MuxerConfig::new(MKV, vec![h264_stream(), aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // 40 s of video in one GOP, then audio far behind: refused (interleaving bound).
    for i in 0..1200u64 {
        let ms = i * 1000 / 30;
        let t = if i == 0 { 0x65 } else { 0x41 };
        let p = video(ms, ms, i == 0, vec![0, 0, 0, 2, t, 1], LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    assert!(matches!(
        muxer.write(StreamIndex(1), &audio(0), &mut chunks),
        Err(MuxError::InvalidPacket(_))
    ));
    // Late within the bound: kept, and its cluster's timestamps stay exact.
    muxer
        .write(StreamIndex(1), &audio(39_500), &mut chunks)
        .unwrap();
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    let times = block_times(&chunks);
    let video_times: Vec<i64> = times.iter().filter(|t| t.0 == 1).map(|t| t.1).collect();
    let expected: Vec<i64> = (0..1200i64).map(|i| 1000 + (i * 1000 / 30)).collect();
    assert_eq!(video_times, expected, "every video block at its own time");
    assert!(times.contains(&(2, 1000 + 39_500)));
}

#[test]
fn long_fragment_durations_are_capped_for_16_bit_block_times() {
    let mut config = MuxerConfig::new(MKV, vec![h264_stream()]);
    config.fragment_duration = Some(std::time::Duration::from_secs(60));
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // One 50 s GOP with B-frame style pts (+10 s is the composition bound).
    for i in 0..1500u64 {
        let ms = i * 1000 / 30;
        let t = if i == 0 { 0x65 } else { 0x41 };
        let p = video(ms + 10_000, ms, i == 0, vec![0, 0, 0, 2, t, 1], LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    assert!(chunks.len() > 3, "a header and clusters of at most 20 s");
    // Time 0 is the capped lead (20 s) before the first dts, and pts are exact.
    let times: Vec<i64> = block_times(&chunks).iter().map(|t| t.1).collect();
    let expected: Vec<i64> = (0..1500i64)
        .map(|i| 20_000 + 10_000 + i * 1000 / 30)
        .collect();
    assert_eq!(times, expected);
}

#[test]
fn clusters_end_at_the_block_limit() {
    let config = MuxerConfig::new(MKV, vec![aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Equal timestamps never end a cluster by time.
    for _ in 0..(1 << 16) + 10 {
        muxer.write(StreamIndex(0), &audio(0), &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks.retain(MuxChunk::is_sequential); // Clusters only, no patches.
    let counts: Vec<usize> = chunks[1..]
        .iter()
        .map(|c| cluster(&c.data).1.len())
        .collect();
    assert_eq!(counts, [1 << 16, 10]);
}

#[test]
fn length_prefixed_packets_without_slices_are_refused() {
    let config = MuxerConfig::new(MKV, vec![h264_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut out = Vec::new();
    muxer.begin(&mut out).unwrap();
    let aud_only = video(0, 0, true, vec![0, 0, 0, 2, 0x09, 0xf0], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &aud_only, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
}

/// Applies the patch chunks to the sequential bytes, as a seekable file would.
fn file_bytes(chunks: &[MuxChunk]) -> Vec<u8> {
    let mut bytes: Vec<u8> = chunks
        .iter()
        .filter(|c| c.is_sequential())
        .flat_map(|c| c.data.iter().copied())
        .collect();
    for c in chunks {
        if let ChunkKind::Patch { offset } = c.kind {
            let at = offset as usize;
            bytes[at..at + c.data.len()].copy_from_slice(&c.data);
        }
    }
    bytes
}

#[test]
fn finish_patches_the_duration_and_segment_size() {
    let config = MuxerConfig::new(MKV, vec![h264_stream(), aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    // Unpatched (live) header: a Void reserves the Duration's 11 bytes.
    let live = parse(&chunks[0].data);
    let info = live[1].child(0x1549_a966);
    assert_eq!(info.child(0xec).data.len(), 9);
    assert!(info.all(0x4489).next().is_none());

    for (i, ms) in [0, 33, 66, 100].into_iter().enumerate() {
        let data = if i == 0 {
            vec![0, 0, 0, 2, 0x65, 9]
        } else {
            vec![0, 0, 0, 2, 0x41, 8]
        };
        let packet = video(ms, ms, i == 0, data, LP4);
        muxer.write(StreamIndex(0), &packet, &mut chunks).unwrap();
        muxer
            .write(StreamIndex(1), &audio(ms), &mut chunks)
            .unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let patches: Vec<&MuxChunk> = chunks.iter().filter(|c| !c.is_sequential()).collect();
    assert_eq!(patches.len(), 2, "duration and segment size");
    assert!(
        chunks.last().is_some_and(|c| !c.is_sequential()),
        "patches come last"
    );

    let bytes = file_bytes(&chunks);
    let top = parse(&bytes);
    let segment = &top[1];
    assert_eq!(top.len(), 2, "the known Segment size covers every cluster");
    let size_at = bytes.len() - segment.data.len() - 8;
    assert_eq!(bytes[size_at], 0x01, "8-byte size");
    assert_eq!(segment.all(CLUSTER).count(), 1);
    let info = segment.child(0x1549_a966);
    assert!(
        info.all(0xec).next().is_none(),
        "the Void became the Duration"
    );
    let duration = f64::from_bits(info.child(0x4489).uint());
    // Time 0 is 1 s before the first dts; the last video frame (pts 100 ms) lasts one
    // frame at 30 fps, longer than the last AAC frame (1024 samples at 48 kHz).
    let expected = 1000.0 + 100.0 + 1000.0 / 30.0;
    assert!((duration - expected).abs() < 0.001, "{duration} ms");
}

#[test]
fn an_empty_recording_gets_no_patches() {
    let config = MuxerConfig::new(MKV, vec![aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    muxer.finish(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 1);
}
