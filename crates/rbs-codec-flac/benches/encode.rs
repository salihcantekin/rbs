//! Micro-benchmark of one FLAC frame (measure-performance skill).
//!
//! `submit` of 4096 stereo frames encodes one frame. The graph sends 480-frame
//! blocks, so this cost is paid about once per 85 ms at 48 kHz, not every 10 ms.
//! There is no `main` counterpart: this crate is new. 0.5 s warm-up, 2 s
//! measurement, 20 samples.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput};
use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_flac::FlacEncoderFactory;
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

fn main() {
    let mut c = Criterion::default().configure_from_args();
    let config = AudioEncoderConfig::new(CodecId::Flac, AudioFormat::STEREO_48K, 0);
    let mut session = FlacEncoderFactory.create(&config).unwrap();
    let left: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.01).sin() * 0.2).collect();
    let mut out = Vec::with_capacity(4);
    let ts = Timestamp::from_nanos(1_000_000_000);
    let mut group = c.benchmark_group("flac_submit");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);
    // One 16-bit stereo frame, uncompressed, as the throughput unit.
    group.throughput(Throughput::Bytes(4096 * 2 * 2));
    group.bench_function("stereo_48k_4096_16bit", |b| {
        b.iter(|| {
            let block = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
            session.submit(&block).unwrap();
            session.drain(&mut out).unwrap();
            black_box(&out);
            out.clear();
        })
    });
    group.finish();
    c.final_summary();
}
