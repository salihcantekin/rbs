//! Cost per block of the sync probe (ADR-0126; hot path: audio thread, every 10 ms,
//! only while a sync offset measurement runs).
//!
//! - `48k_stereo_480`: one 10 ms block of the default graph, ten 1 ms sub-blocks
//!   logged. Compare with the audio block budget of 10 ms.
//! - `48k_5.1_480` and `96k_stereo_960` for comparison.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benches fail by panicking.

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio::SubBlockEnergy;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::timing::SampleRate;

fn bench_format(c: &mut Criterion, name: &str, rate: u32, layout: ChannelLayout, frames: usize) {
    let format = AudioFormat::new(SampleRate::new(rate).unwrap(), layout).unwrap();
    let mut acc = SubBlockEnergy::new(format);
    let chans: Vec<Vec<f32>> = (0..layout.channels())
        .map(|k| {
            (0..frames)
                .map(|f| ((f + k * 37) as f32 * 0.05).sin() * 0.3)
                .collect()
        })
        .collect();
    let refs: Vec<&[f32]> = chans.iter().map(|c| c.as_slice()).collect();
    let block_ns = 1_000_000_000 * frames as u64 / u64::from(rate);
    let mut t = 0u64;
    c.bench_function(&format!("sync_probe_block/{name}"), |b| {
        b.iter(|| {
            let mut sum = 0.0f32;
            acc.add(std::hint::black_box(&refs), frames, t, |_, e| sum += e);
            t += block_ns;
            std::hint::black_box(sum);
        })
    });
}

fn probe(c: &mut Criterion) {
    bench_format(c, "48k_stereo_480", 48_000, ChannelLayout::Stereo, 480);
    bench_format(c, "48k_5.1_480", 48_000, ChannelLayout::Surround5_1, 480);
    bench_format(c, "96k_stereo_960", 96_000, ChannelLayout::Stereo, 960);
}

criterion_group!(benches, probe);
criterion_main!(benches);
