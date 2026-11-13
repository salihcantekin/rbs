//! Micro-benchmark of `FlacMuxer::write` (measure-performance skill).
//!
//! One already-encoded frame is cloned into a media chunk. The header is not
//! patched. There is no `main` counterpart.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamDescriptor,
    StreamIndex, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_mux::{MuxerConfig, MuxerFactory};
use rbs_mux_flac::FlacMuxerFactory;

fn main() {
    let mut c = Criterion::default().configure_from_args();
    let stream = StreamDescriptor::new(
        CodecId::Flac,
        BitstreamFormat::Raw,
        Bytes::from(vec![16_u8]),
        StreamParams::audio(AudioFormat::STEREO_48K, 4096),
    );
    let config = MuxerConfig::new(ContainerId::Flac, vec![stream]);
    let mut muxer = FlacMuxerFactory.create(&config).unwrap();
    muxer.begin(&mut Vec::new()).unwrap();
    let t = Timestamp::from_nanos(1_000_000_000);
    let packet = EncodedPacket::new(
        CodecId::Flac,
        BitstreamFormat::Raw,
        t,
        t,
        true,
        Bytes::from(vec![0_u8; 2048]),
    )
    .unwrap();
    let mut group = c.benchmark_group("flac_write");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);
    group.throughput(Throughput::Bytes(2048));
    let mut out = Vec::with_capacity(2);
    group.bench_function("frame_2048", |b| {
        b.iter(|| {
            muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
            black_box(&out);
            out.clear();
        })
    });
    group.finish();
    c.final_summary();
}
