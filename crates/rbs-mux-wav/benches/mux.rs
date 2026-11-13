//! Micro-benchmark of `WavMuxer::write` (measure-performance skill).
//!
//! One 10 ms stereo 16-bit packet (1920 bytes). The session patches sizes on the
//! first packet and then about every 1 MiB, so a long run includes those patches.
//! A second function writes one packet on a fresh session, which is the first-packet
//! path (header already written, one size patch). There is no `main` counterpart.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamDescriptor,
    StreamIndex, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory};
use rbs_mux_wav::WavMuxerFactory;

fn stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        Bytes::from(vec![16_u8]),
        StreamParams::audio(AudioFormat::STEREO_48K, 480),
    )
}

fn packet() -> EncodedPacket {
    let t = Timestamp::from_nanos(1_000_000_000);
    EncodedPacket::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        t,
        t,
        true,
        Bytes::from(vec![0_u8; 1920]),
    )
    .unwrap()
}

fn opened() -> Box<dyn Muxer> {
    let config = MuxerConfig::new(ContainerId::Wav, vec![stream()]);
    let mut muxer = WavMuxerFactory.create(&config).unwrap();
    muxer.begin(&mut Vec::new()).unwrap();
    muxer
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    let packet = packet();
    let mut group = c.benchmark_group("wav_write");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);
    group.throughput(Throughput::Bytes(1920));

    group.bench_function("steady_1920", |b| {
        let mut muxer = opened();
        let mut out = Vec::with_capacity(4);
        // The first packet patches sizes. The timed calls are the ones after it.
        muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
        out.clear();
        b.iter(|| {
            muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
            black_box(&out);
            out.clear();
        })
    });

    group.bench_function("first_packet_1920", |b| {
        b.iter(|| {
            let mut muxer = opened();
            let mut out = Vec::with_capacity(4);
            muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
            black_box(&out);
        })
    });
    group.finish();
    c.final_summary();
}
