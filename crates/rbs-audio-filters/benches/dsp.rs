//! Block cost of the audio filters at 48 kHz stereo, 480 frames (ADR-0082, P15).
//!
//! Run: `cargo bench -p rbs-audio-filters`.

#![allow(clippy::expect_used)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_filters::gain::Gain;
use rbs_audio_filters::gate::NoiseGate;
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup};
use rbs_core::media::AudioFormat;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

/// A 220 Hz tone at -20 dBFS that the gate opens on, plus a noisy tail, deterministic.
fn signal() -> (Vec<f32>, Vec<f32>) {
    let l: Vec<f32> = (0..FRAMES)
        .map(|i| 0.1 * (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    (l.clone(), l)
}

fn bench_one(c: &mut Criterion, name: &str, dsps: &mut [Box<dyn AudioDsp>]) {
    let (l0, r0) = signal();
    c.bench_function(name, |b| {
        let (mut l, mut r) = (l0.clone(), r0.clone());
        b.iter(|| {
            l.copy_from_slice(&l0);
            r.copy_from_slice(&r0);
            for d in dsps.iter_mut() {
                d.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            }
            black_box((&l, &r));
        });
    });
}

fn dsp_benches(c: &mut Criterion) {
    let gain = Values::defaults(&Gain as &dyn AudioFilterType);
    let moving = gain
        .with(
            &Gain as &dyn AudioFilterType,
            [("gain", Value::Number(6.0))],
        )
        .expect("valid");
    let gate = Values::defaults(&NoiseGate as &dyn AudioFilterType);
    let make = |t: &dyn AudioFilterType, v: &Values| t.make(setup(), v).expect("valid");

    bench_one(c, "gain_steady", &mut [make(&Gain, &moving)]);
    bench_one(c, "gate_open", &mut [make(&NoiseGate, &gate)]);
    let mut chain = vec![
        make(&Gain, &moving),
        make(&NoiseGate, &gate),
        make(&Gain, &gain),
        make(&NoiseGate, &gate),
    ];
    bench_one(c, "chain_of_four", &mut chain);
    // A closed gate stays in the cheap state; a transitioning one pays the exp per sample.
    let mut closing = make(&NoiseGate, &gate);
    let mut l = vec![0.0f32; FRAMES];
    let mut r = vec![0.0f32; FRAMES];
    c.bench_function("gate_closed_silence", |b| {
        b.iter(|| {
            closing.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            black_box((&l, &r));
        });
    });
}

criterion_group!(benches, dsp_benches);
criterion_main!(benches);
