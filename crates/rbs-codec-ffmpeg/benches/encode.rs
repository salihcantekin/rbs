//! Micro-benchmark of the AAC encode path (measure-performance skill, ADR-0009):
//! `submit` plus `drain` of one 10 ms stereo 48 kHz block, which encodes a packet
//! every 2.13 blocks. `fake` measures the session alone (staging, timeline) on every platform; `ffmpeg` adds libavcodec's encoder on Linux.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benchmarks fail by panicking.

use criterion::Criterion;
use rbs_codec::{AudioEncoder, AudioEncoderConfig};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

fn bench(c: &mut Criterion, name: &str, s: &mut dyn AudioEncoder) {
    let left: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
    let rate = AudioFormat::STEREO_48K.sample_rate;
    let mut out = Vec::with_capacity(4096);
    let mut index = 0u64;
    c.bench_function(name, |b| {
        b.iter(|| {
            let ts = Timestamp::from_nanos(1_000_000_000) + rate.duration_of(index * 480);
            index += 1;
            let block = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
            s.submit(&block).unwrap();
            s.drain(&mut out).unwrap();
            out.clear();
        })
    });
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    let config = AudioEncoderConfig::new(CodecId::Aac, AudioFormat::STEREO_48K, 128_000);
    let mut fake = rbs_codec_ffmpeg::testing::FakeAacFactory::new()
        .create_session(&config)
        .unwrap();
    bench(&mut c, "submit_drain/fake_stereo_48k_10ms", &mut fake);
    #[cfg(target_os = "linux")]
    {
        use rbs_core::registry::AdapterFactory;
        let factory = rbs_codec_ffmpeg::FfmpegAacFactory::new();
        if factory.probe().unwrap().is_empty() {
            eprintln!("no FFmpeg AAC encoder here: mf bench skipped");
        } else {
            let mut mf = factory.create_session(&config).unwrap();
            bench(&mut c, "submit_drain/ffmpeg_aac_stereo_48k_10ms", &mut mf);
        }
    }
    c.final_summary();
}
