//! Micro-benchmarks for `Muxer::write` of the Matroska muxer.
//! Run with `cargo bench -p rbs-mux-matroska --bench mux`.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rbs_core::media::{BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory};
use rbs_mux_matroska::MatroskaMuxerFactory;

/// Bytes that look like entropy-coded slice data: pseudo-random, with emulation
/// prevention (no `00 00 0x` for x <= 3), so start-code searches see realistic zeros.
fn slice_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_u32;
    let mut out: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    for i in 2..out.len() {
        if out[i - 2] == 0 && out[i - 1] == 0 && out[i] <= 3 {
            out[i] = 3;
        }
    }
    out
}

fn video(size: usize, keyframe: bool, format: BitstreamFormat, index: u64) -> EncodedPacket {
    let mut data = match format {
        BitstreamFormat::AnnexB => vec![0, 0, 0, 1],
        _ => (size as u32 - 4).to_be_bytes().to_vec(),
    };
    data.push(if keyframe { 0x65 } else { 0x41 });
    data.extend(slice_bytes(size - data.len()));
    let t = Timestamp::from_nanos(1_000_000_000 + index * 33_333_333);
    EncodedPacket::new(CodecId::H264, format, t, t, keyframe, Bytes::from(data)).unwrap()
}

fn session(format: BitstreamFormat) -> Box<dyn Muxer> {
    let mut video = h264_stream();
    video.bitstream = format;
    let config = MuxerConfig::new(ContainerId::Matroska, vec![video, aac_stream()]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    muxer.begin(&mut Vec::new()).unwrap();
    muxer
}

fn gop(format: BitstreamFormat) -> Vec<(StreamIndex, EncodedPacket)> {
    let mut packets: Vec<(StreamIndex, EncodedPacket)> = (0..30u64)
        .map(|i| {
            let size = if i == 0 { 150_000 } else { 20_000 };
            (StreamIndex(0), video(size, i == 0, format, i))
        })
        .collect();
    for i in 0..47u64 {
        let t = Timestamp::from_nanos(1_000_000_000 + i * 21_333_333);
        let a = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(vec![0x21; 400]),
        );
        packets.push((StreamIndex(1), a.unwrap()));
    }
    packets.sort_by_key(|(s, p)| (p.dts(), *s));
    packets
}

fn bench_write(c: &mut Criterion) {
    let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
    let mut group = c.benchmark_group("matroska_write");
    // A keyframe ends the cluster before it, so each iteration writes one cluster.
    for (name, format) in [
        ("h264_idr_150k_length_prefixed", lp),
        ("h264_idr_150k_annexb", BitstreamFormat::AnnexB),
    ] {
        let packet = video(150_000, true, format, 1);
        group.throughput(Throughput::Bytes(packet.data().len() as u64));
        group.bench_function(name, |b| {
            let mut muxer = session(format);
            let mut out = Vec::with_capacity(1);
            b.iter(|| {
                out.clear();
                // Same dts each time is allowed (only decreasing dts is refused).
                muxer
                    .write(StreamIndex(0), black_box(&packet), &mut out)
                    .unwrap();
                black_box(&out);
            });
        });
    }
    // One second of ~6 Mbit/s 1080p30 with AAC: a 150 KB keyframe, 29 P frames of
    // 20 KB and 47 AAC frames, written and finished as one cluster.
    for (name, format) in [
        ("gop_1s_length_prefixed", lp),
        ("gop_1s_annexb", BitstreamFormat::AnnexB),
    ] {
        let packets = gop(format);
        let bytes: usize = packets.iter().map(|(_, p)| p.data().len()).sum();
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_function(name, |b| {
            b.iter_batched(
                || (session(format), Vec::with_capacity(2)),
                |(mut muxer, mut out)| {
                    for (stream, packet) in &packets {
                        muxer.write(*stream, black_box(packet), &mut out).unwrap();
                    }
                    muxer.finish(&mut out).unwrap();
                    black_box(out)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_write);
criterion_main!(benches);
