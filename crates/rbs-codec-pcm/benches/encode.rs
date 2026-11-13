//! Micro-benchmark of PCM `submit` plus `drain` (measure-performance skill).
//!
//! One 10 ms stereo 48 kHz block, which is exactly one packet. There is no `main`
//! counterpart: this crate is new. The run is shortened (0.5 s warm-up, 2 s
//! measurement, 20 samples) so the number is from the tool and the run stays short.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput};
use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_pcm::PcmEncoderFactory;
use rbs_core::media::{AudioBlock, AudioFormat};
use rbs_core::time::Timestamp;

fn bench(c: &mut Criterion, name: &str, bits: &str, bytes: u64) {
    let mut config =
        AudioEncoderConfig::new(rbs_core::media::CodecId::Pcm, AudioFormat::STEREO_48K, 0);
    config
        .adapter_options
        .push(("bit_depth".into(), bits.into()));
    let mut session = PcmEncoderFactory.create(&config).unwrap();
    let left: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin() * 0.2).collect();
    let rate = AudioFormat::STEREO_48K.sample_rate;
    let mut out = Vec::with_capacity(4);
    let mut index = 0u64;
    let mut group = c.benchmark_group("pcm_submit_drain");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function(name, |b| {
        b.iter(|| {
            let ts = Timestamp::from_nanos(1_000_000_000) + rate.duration_of(index * 480);
            index += 1;
            let block = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
            session.submit(&block).unwrap();
            session.drain(&mut out).unwrap();
            black_box(&out);
            out.clear();
        })
    });
    group.finish();
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    // 480 frames, stereo: 16-bit is 1920 bytes, 24-bit 2880, float 3840.
    bench(&mut c, "stereo_48k_10ms_16bit", "16", 1920);
    bench(&mut c, "stereo_48k_10ms_24bit", "24", 2880);
    bench(&mut c, "stereo_48k_10ms_32bit_float", "32", 3840);
    c.final_summary();
}
