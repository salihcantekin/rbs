//! Micro-benchmarks for `Muxer::write` of the MPEG-TS muxer.
//! Run with `cargo bench -p rbs-mux-mpegts --bench mux`.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rbs_core::media::{BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory};
use rbs_mux_mpegts::MpegTsMuxerFactory;

fn video(size: usize, keyframe: bool, format: BitstreamFormat, index: u64) -> EncodedPacket {
    let mut data = match format {
        BitstreamFormat::AnnexB => vec![0, 0, 0, 1],
        _ => (size as u32 - 4).to_be_bytes().to_vec(),
    };
    data.push(if keyframe { 0x65 } else { 0x41 });
    data.resize(size, 0xab);
    let t = Timestamp::from_nanos(1_000_000_000 + index * 33_333_333);
    EncodedPacket::new(CodecId::H264, format, t, t, keyframe, Bytes::from(data)).unwrap()
}

fn session(format: BitstreamFormat) -> Box<dyn Muxer> {
    let mut video = h264_stream();
    video.bitstream = format;
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![video, aac_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    muxer.begin(&mut Vec::new()).unwrap();
    muxer
}

fn bench_write(c: &mut Criterion) {
    let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
    let cases = [
        // ~6 Mbit/s 1080p30: P frames around 20 KB, keyframes around 150 KB.
        ("h264_p_20k_length_prefixed", video(20_000, false, lp, 1)),
        (
            "h264_p_20k_annexb",
            video(20_000, false, BitstreamFormat::AnnexB, 1),
        ),
        ("h264_idr_150k_length_prefixed", video(150_000, true, lp, 1)),
        (
            "h264_idr_150k_annexb",
            video(150_000, true, BitstreamFormat::AnnexB, 1),
        ),
    ];
    let mut group = c.benchmark_group("mpegts_write");
    for (name, packet) in cases {
        group.throughput(Throughput::Bytes(packet.data().len() as u64));
        group.bench_function(name, |b| {
            let mut muxer = session(packet.bitstream());
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
    let t = Timestamp::from_nanos(1_000_000_000);
    let audio = EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        t,
        t,
        true,
        Bytes::from(vec![0x21; 400]),
    )
    .unwrap();
    group.throughput(Throughput::Bytes(400));
    group.bench_function("aac_400", |b| {
        let mut muxer = session(lp);
        let mut out = Vec::with_capacity(1);
        b.iter(|| {
            out.clear();
            muxer
                .write(StreamIndex(1), black_box(&audio), &mut out)
                .unwrap();
            black_box(&out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_write);
criterion_main!(benches);
