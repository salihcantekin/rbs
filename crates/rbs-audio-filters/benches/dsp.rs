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

/// The ducker with a loud key (ducking, then steady) and a silent one (idle).
fn ducker_benches(c: &mut Criterion) {
    use rbs_audio_filters::ducker::Ducker;
    let values = Values::defaults(&Ducker as &dyn AudioFilterType);
    let mut l = vec![0.1f32; FRAMES];
    let mut r = vec![0.1f32; FRAMES];
    let loud = vec![0.5f32; FRAMES];
    let quiet = vec![0.0f32; FRAMES];
    for (name, key) in [("ducker_ducking", &loud), ("ducker_idle", &quiet)] {
        let mut d = Ducker.make(setup(), &values).expect("valid");
        c.bench_function(name, |b| {
            b.iter(|| {
                d.process_keyed(
                    &mut [&mut l[..], &mut r[..]],
                    Some(&[&key[..], &key[..]]),
                    FRAMES,
                );
                black_box((&l, &r));
            });
        });
    }
}

/// Noise suppression (ADR-0130 decision 11): one 480-frame stereo block, one RNNoise
/// frame per block in `mono` mode, two in `separate` mode.
fn noise_benches(c: &mut Criterion) {
    use rbs_audio_filters::noise::NoiseSuppression;
    let ty = &NoiseSuppression as &dyn AudioFilterType;
    let mono = Values::defaults(ty);
    let separate = mono.with(ty, [("mode", Value::Choice(1))]).expect("valid");
    bench_one(
        c,
        "noise_suppression_mono",
        &mut [ty.make(setup(), &mono).expect("48 kHz")],
    );
    bench_one(
        c,
        "noise_suppression_separate",
        &mut [ty.make(setup(), &separate).expect("48 kHz")],
    );
}

/// The compressor (ADR-0130 decision 11, budget 5 us per block) while compressing, with
/// each detector, and while idle below the knee.
fn compressor_benches(c: &mut Criterion) {
    use rbs_audio_filters::compressor::Compressor;
    let ty = &Compressor as &dyn AudioFilterType;
    let defaults = Values::defaults(ty);
    let rms = defaults
        .with(
            ty,
            [
                ("threshold", Value::Number(-30.0)),
                ("makeup", Value::Number(6.0)),
            ],
        )
        .expect("valid");
    let peak = rms
        .with(ty, [("detector", Value::Choice(1))])
        .expect("valid");
    let idle = defaults
        .with(ty, [("threshold", Value::Number(0.0))])
        .expect("valid");
    for (name, v) in [
        ("compressor_rms", &rms),
        ("compressor_peak", &peak),
        ("compressor_idle", &idle),
    ] {
        bench_one(c, name, &mut [ty.make(setup(), v).expect("valid")]);
    }
}

/// The limiter (ADR-0130 decision 11, budget 5 us per block) on full-scale noise
/// (limiting all the time) and on a quiet tone (an exact delay).
fn limiter_benches(c: &mut Criterion) {
    use rbs_audio_filters::limiter::Limiter;
    let ty = &Limiter as &dyn AudioFilterType;
    let values = Values::defaults(ty);
    let mut loud = ty.make(setup(), &values).expect("valid");
    let mut s = 1u32;
    let noise: Vec<f32> = (0..FRAMES)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 4.0 - 2.0
        })
        .collect();
    c.bench_function("limiter_limiting", |b| {
        let (mut l, mut r) = (noise.clone(), noise.clone());
        b.iter(|| {
            l.copy_from_slice(&noise);
            r.copy_from_slice(&noise);
            loud.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            black_box((&l, &r));
        });
    });
    bench_one(
        c,
        "limiter_idle",
        &mut [ty.make(setup(), &values).expect("valid")],
    );
}

/// The equaliser (ADR-0130 decision 11, budget 5 us per block): a typical voice setting
/// (low cut and one band), every section on, and a sweep (coefficients every 32 frames).
fn equaliser_benches(c: &mut Criterion) {
    use rbs_audio_filters::equaliser::Equaliser;
    let ty = &Equaliser as &dyn AudioFilterType;
    let defaults = Values::defaults(ty);
    let voice = defaults
        .with(
            ty,
            [
                ("low_cut", Value::Toggle(true)),
                ("band2_gain", Value::Number(3.0)),
            ],
        )
        .expect("valid");
    let all = voice
        .with(
            ty,
            [
                ("low_shelf_gain", Value::Number(-2.0)),
                ("band1_gain", Value::Number(-3.0)),
                ("high_shelf_gain", Value::Number(2.0)),
            ],
        )
        .expect("valid");
    bench_one(
        c,
        "equaliser_voice",
        &mut [ty.make(setup(), &voice).expect("valid")],
    );
    bench_one(
        c,
        "equaliser_all",
        &mut [ty.make(setup(), &all).expect("valid")],
    );
    // A tone, then 0.6 s of digital silence: the section states decay towards
    // denormals (A-04 of the 2026-11 performance audit). One iteration is 61 blocks.
    let mut decaying = ty.make(setup(), &all).expect("valid");
    let (tone, _) = signal();
    c.bench_function("equaliser_tone_then_silence_61_blocks", |b| {
        let (mut l, mut r) = (tone.clone(), tone.clone());
        b.iter(|| {
            l.copy_from_slice(&tone);
            r.copy_from_slice(&tone);
            decaying.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            for _ in 0..60 {
                l.fill(0.0);
                r.fill(0.0);
                decaying.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            }
            black_box((&l, &r));
        });
    });
    let other = all
        .with(ty, [("band1_freq", Value::Number(2000.0))])
        .expect("valid");
    let mut sweeping = ty.make(setup(), &all).expect("valid");
    let (l0, _) = signal();
    let mut flip = false;
    c.bench_function("equaliser_sweeping", |b| {
        let (mut l, mut r) = (l0.clone(), l0.clone());
        b.iter(|| {
            flip = !flip;
            sweeping.set(if flip { &other } else { &all });
            l.copy_from_slice(&l0);
            r.copy_from_slice(&l0);
            sweeping.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            black_box((&l, &r));
        });
    });
}

criterion_group!(
    benches,
    equaliser_benches,
    limiter_benches,
    dsp_benches,
    ducker_benches,
    noise_benches,
    compressor_benches
);
criterion_main!(benches);
