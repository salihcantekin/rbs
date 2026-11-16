//! Cost of one RNNoise frame (480 samples, 10 ms at 48 kHz) on speech-like noise
//! (ADR-0130 decision 11: at most 250 µs per frame).
//!
//! Run: `cargo bench -p rbs-denoise`.

#![allow(clippy::expect_used)]

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_denoise::{Denoiser, FRAME_SIZE};

fn frame_bench(c: &mut Criterion) {
    let mut d = Denoiser::new().expect("model");
    d.set_max_reduction_db(30.0);
    // Deterministic noise plus a 200 Hz tone, at speech level in the i16 range.
    let mut seed = 1u32;
    let mut input = [0.0f32; FRAME_SIZE];
    for (i, x) in input.iter_mut().enumerate() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        *x = 3000.0 * noise + 4000.0 * (i as f32 * 200.0 * std::f32::consts::TAU / 48_000.0).sin();
    }
    let mut output = [0.0f32; FRAME_SIZE];
    c.bench_function("rnnoise_frame_480", |b| {
        b.iter(|| d.process_frame(&mut output, std::hint::black_box(&input)))
    });
    // Upstream on the same input, for comparison only.
    let mut up = nnnoiseless::DenoiseState::new();
    c.bench_function("upstream_nnnoiseless_frame_480", |b| {
        b.iter(|| up.process_frame(&mut output, std::hint::black_box(&input)))
    });
}

criterion_group!(benches, frame_bench);
criterion_main!(benches);
