//! Cost per block of the loudness meter (ADR-0092; hot path: audio thread, every
//! 10 ms while loudness is shown).
//!
//! - `48k_stereo_480`: one 10 ms block of the default graph (48 kHz stereo,
//!   480 frames): K-weighting, 4x true peak, and every tenth block the gating and
//!   range histograms. Compare with the audio block budget of 10 ms.
//! - `192k_stereo_1920_sample_peak`: at 192 kHz the true peak is the sample peak, so
//!   this is about the K-weighting and window share alone (per 10 ms).
//! - mono, 5.1 and 96 kHz (2x true peak) for comparison.
//! - `48k_stereo_480_below_max`: the same blocks 6 dB below an earlier peak, the
//!   usual case of programme sound after its loudest moment (the true peak cannot
//!   rise, so its oversampling is skipped; A-02 of the 2026-11 performance audit).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benches fail by panicking.

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio::LoudnessMeter;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::timing::SampleRate;

fn signal(frames: usize, k: usize) -> Vec<f32> {
    (0..frames)
        .map(|f| ((f + k * 37) as f32 * 0.05).sin() * 0.3)
        .collect()
}

fn bench_format(c: &mut Criterion, name: &str, rate: u32, layout: ChannelLayout, frames: usize) {
    bench_signal(c, name, rate, layout, frames, 1.0);
}

/// Runs blocks of `signal` scaled by `level`, after one block at full scale.
fn bench_signal(
    c: &mut Criterion,
    name: &str,
    rate: u32,
    layout: ChannelLayout,
    frames: usize,
    level: f32,
) {
    let format = AudioFormat::new(SampleRate::new(rate).unwrap(), layout).unwrap();
    let mut meter = LoudnessMeter::new(format);
    let loud: Vec<Vec<f32>> = (0..layout.channels()).map(|k| signal(frames, k)).collect();
    let refs: Vec<&[f32]> = loud.iter().map(|c| c.as_slice()).collect();
    meter.process(&refs, frames);
    let chans: Vec<Vec<f32>> = loud
        .iter()
        .map(|c| c.iter().map(|v| v * level).collect())
        .collect();
    let refs: Vec<&[f32]> = chans.iter().map(|c| c.as_slice()).collect();
    // Fill the windows and histograms first, so every block does its full work.
    for _ in 0..400 {
        meter.process(&refs, frames);
    }
    c.bench_function(&format!("loudness_meter_block/{name}"), |b| {
        b.iter(|| {
            meter.process(std::hint::black_box(&refs), frames);
            std::hint::black_box(meter.loudness());
        })
    });
}

fn meter(c: &mut Criterion) {
    bench_format(c, "48k_stereo_480", 48_000, ChannelLayout::Stereo, 480);
    bench_signal(
        c,
        "48k_stereo_480_below_max",
        48_000,
        ChannelLayout::Stereo,
        480,
        0.5,
    );
    bench_format(c, "48k_mono_480", 48_000, ChannelLayout::Mono, 480);
    bench_format(c, "48k_5.1_480", 48_000, ChannelLayout::Surround5_1, 480);
    bench_format(c, "96k_stereo_960", 96_000, ChannelLayout::Stereo, 960);
    bench_format(
        c,
        "192k_stereo_1920_sample_peak",
        192_000,
        ChannelLayout::Stereo,
        1920,
    );
}

criterion_group!(benches, meter);
criterion_main!(benches);
