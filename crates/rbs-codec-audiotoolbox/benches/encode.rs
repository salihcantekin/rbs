//! Micro-benchmark of the AAC encode path (measure-performance skill, ADR-0009):
//! `submit` plus `drain` of one 10 ms stereo 48 kHz block, which encodes a packet
//! every 2.13 blocks. On other platforms it does nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benchmarks fail by panicking.

#[cfg(target_os = "macos")]
mod bench {
    use criterion::Criterion;
    use rbs_codec::{AudioEncoder, AudioEncoderConfig};
    use rbs_codec_audiotoolbox::AacFactory;
    use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
    use rbs_core::time::Timestamp;

    pub fn main() {
        let mut c = Criterion::default().configure_from_args();
        let mut s = AacFactory::new()
            .create_session(&AudioEncoderConfig::new(
                CodecId::Aac,
                AudioFormat::STEREO_48K,
                128_000,
            ))
            .unwrap();
        let left: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
        let rate = AudioFormat::STEREO_48K.sample_rate;
        let mut out = Vec::with_capacity(4096);
        let mut index = 0u64;
        c.bench_function("submit_drain/aac_stereo_48k_10ms", |b| {
            b.iter(|| {
                let ts = Timestamp::from_nanos(1_000_000_000) + rate.duration_of(index * 480);
                index += 1;
                let block =
                    AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
                s.submit(&block).unwrap();
                s.drain(&mut out).unwrap();
                out.clear();
            })
        });
        c.final_summary();
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    bench::main();
}
