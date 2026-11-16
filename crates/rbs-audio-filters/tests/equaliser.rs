//! Equaliser (ADR-0130 decision 8): bit-exact defaults, magnitudes at the band centres,
//! smooth sweeps, stability at the extremes, allocation.

#![allow(clippy::expect_used)]

use rbs_audio_filters::equaliser::{Coefs, EQUALISER, Equaliser, EqualiserDsp, Shape};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, conformance, find};
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::timing::SampleRate;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;
const RATE: f32 = 48_000.0;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

fn values(pairs: &[(&str, Value)]) -> Values {
    Values::from_pairs(&Equaliser as &dyn AudioFilterType, pairs.iter().copied()).expect("valid")
}

fn sine(freq: f32, len: usize, start: usize) -> Vec<f32> {
    (start..start + len)
        .map(|i| 0.25 * (i as f32 * freq * std::f32::consts::TAU / RATE).sin())
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// The gain in dB of `dsp` on a steady sine of `freq`, after it settles.
fn measured_db(dsp: &mut dyn AudioDsp, freq: f32) -> f32 {
    let blocks = 40;
    let mut last_in = Vec::new();
    let mut last_out = Vec::new();
    for b in 0..blocks {
        let input = sine(freq, FRAMES, b * FRAMES);
        let mut l = input.clone();
        let mut r = input.clone();
        dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        if b >= blocks - 10 {
            last_in.extend_from_slice(&input);
            last_out.extend_from_slice(&l);
        }
    }
    20.0 * (rms(&last_out) / rms(&last_in)).log10()
}

#[test]
fn it_is_in_the_catalogue_and_conforms() {
    assert!(find(EQUALISER).is_some());
    assert_eq!(conformance::check_type(&Equaliser), Vec::<String>::new());
    assert_eq!(Equaliser.latency_frames(setup()), 0);
    assert_eq!(Equaliser.fields().len(), 12);
}

#[test]
fn the_defaults_and_zero_gains_are_bit_exact() {
    let input: Vec<f32> = (0..FRAMES)
        .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
        .collect();
    for v in [
        values(&[]),
        values(&[
            ("band1_freq", Value::Number(1000.0)),
            ("band2_q", Value::Number(5.0)),
        ]),
    ] {
        let mut dsp = Equaliser.make(setup(), &v).expect("valid");
        for _ in 0..3 {
            let (mut l, mut r) = (input.clone(), input.clone());
            dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            assert_eq!(l, input);
            assert_eq!(r, input);
        }
    }
}

/// Values, a test frequency and the gain expected there in dB.
type Case = (&'static [(&'static str, Value)], f32, f32);

#[test]
fn the_magnitude_at_each_centre_matches_the_formulas() {
    // A peak gives its gain at its centre, a slope-1 shelf half its gain at its
    // frequency, the low cut -3 dB at its frequency.
    let cases: [Case; 6] = [
        (
            &[
                ("band1_gain", Value::Number(9.0)),
                ("band1_freq", Value::Number(1000.0)),
            ],
            1000.0,
            9.0,
        ),
        (
            &[
                ("band2_gain", Value::Number(-12.0)),
                ("band2_q", Value::Number(4.0)),
            ],
            3000.0,
            -12.0,
        ),
        (&[("low_shelf_gain", Value::Number(6.0))], 120.0, 3.0),
        // Deep in the shelf, the RBJ formula's own value.
        (
            &[("low_shelf_gain", Value::Number(6.0))],
            25.0,
            Coefs::rbj(Shape::LowShelf, RATE, 120.0, 6.0, 1.0).magnitude_db(RATE, 25.0),
        ),
        (&[("high_shelf_gain", Value::Number(-8.0))], 8000.0, -4.0),
        (&[("low_cut", Value::Toggle(true))], 80.0, -3.01),
    ];
    for (pairs, freq, want) in cases {
        let v = values(pairs);
        let mut dsp = EqualiserDsp::new(setup(), &v);
        let got = measured_db(&mut dsp, freq);
        assert!(
            (got - want).abs() < 0.15,
            "{pairs:?} at {freq} Hz: {got} dB, want {want}"
        );
    }
    // The coefficients themselves match the RBJ magnitudes exactly at the centres.
    let c = Coefs::rbj(Shape::Peak, RATE, 1000.0, 9.0, 1.0);
    assert!((c.magnitude_db(RATE, 1000.0) - 9.0).abs() < 0.01);
    let c = Coefs::rbj(
        Shape::LowCut,
        RATE,
        80.0,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
    );
    assert!((c.magnitude_db(RATE, 80.0) + 3.0103).abs() < 0.01);
    let c = Coefs::rbj(Shape::HighShelf, RATE, 8000.0, 12.0, 1.0);
    assert!((c.magnitude_db(RATE, 20_000.0) - 12.0).abs() < 0.5);
}

#[test]
fn a_frequency_sweep_during_a_sine_makes_no_step() {
    let v0 = values(&[
        ("band1_gain", Value::Number(12.0)),
        ("band1_freq", Value::Number(200.0)),
    ]);
    let v1 = values(&[
        ("band1_gain", Value::Number(12.0)),
        ("band1_freq", Value::Number(5000.0)),
    ]);
    let v2 = values(&[
        ("band1_gain", Value::Number(-18.0)),
        ("band1_freq", Value::Number(5000.0)),
    ]);
    let mut dsp = EqualiserDsp::new(setup(), &v0);
    let freq = 1000.0;
    // The largest sample-to-sample change of the sine through +12 dB is bounded by
    // 2 * pi * f / rate * amplitude * gain; a step would exceed it by far.
    let bound = std::f32::consts::TAU * freq / RATE * 0.25 * 10f32.powf(12.0 / 20.0) * 1.5;
    let mut prev = 0.0f32;
    for b in 0..60 {
        if b == 10 {
            dsp.set(&v1);
        }
        if b == 30 {
            dsp.set(&v2);
        }
        let mut l = sine(freq, FRAMES, b * FRAMES);
        let mut r = l.clone();
        dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        for &x in &l {
            if b > 0 {
                assert!(
                    (x - prev).abs() <= bound,
                    "block {b}: step {}",
                    (x - prev).abs()
                );
            }
            prev = x;
        }
    }
}

#[test]
fn the_extremes_are_stable() {
    let v = values(&[
        ("low_cut", Value::Toggle(true)),
        ("low_cut_freq", Value::Number(400.0)),
        ("low_shelf_freq", Value::Number(20.0)),
        ("low_shelf_gain", Value::Number(18.0)),
        ("band1_freq", Value::Number(16_000.0)),
        ("band1_gain", Value::Number(18.0)),
        ("band1_q", Value::Number(10.0)),
        ("band2_freq", Value::Number(40.0)),
        ("band2_gain", Value::Number(-18.0)),
        ("band2_q", Value::Number(0.1)),
        ("high_shelf_freq", Value::Number(20_000.0)),
        ("high_shelf_gain", Value::Number(18.0)),
    ]);
    for rate_setup in [
        setup(),
        DspSetup::new(
            AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).expect("format"),
            256,
        ),
    ] {
        let mut dsp = Equaliser.make(rate_setup, &v).expect("valid");
        let frames = rate_setup.block_frames as usize;
        let mut s = 1u32;
        for _ in 0..400 {
            let mut l: Vec<f32> = (0..frames)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    (s as f32 / u32::MAX as f32) * 2.0 - 1.0
                })
                .collect();
            let mut r = l.clone();
            dsp.process(&mut [&mut l[..], &mut r[..]], frames);
            assert!(l.iter().all(|x| x.is_finite() && x.abs() < 100.0));
        }
    }
}

#[test]
fn a_gain_returning_to_zero_becomes_bit_exact_again() {
    let mut dsp = EqualiserDsp::new(setup(), &values(&[("band1_gain", Value::Number(6.0))]));
    assert!(dsp.is_active(2));
    dsp.set(&values(&[]));
    let input = sine(440.0, FRAMES, 0);
    for _ in 0..20 {
        let (mut l, mut r) = (input.clone(), input.clone());
        dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
    }
    assert!(!dsp.is_active(2));
    let (mut l, mut r) = (input.clone(), input.clone());
    dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
    assert_eq!(l, input);
}

#[test]
fn processing_and_set_do_not_allocate() {
    let mut dsp = Equaliser.make(setup(), &values(&[])).expect("valid");
    let a = values(&[
        ("band1_gain", Value::Number(6.0)),
        ("low_cut", Value::Toggle(true)),
    ]);
    let b = values(&[
        ("band2_gain", Value::Number(-6.0)),
        ("band1_freq", Value::Number(900.0)),
    ]);
    let mut l = sine(300.0, FRAMES, 0);
    let mut r = l.clone();
    let info = allocation_counter::measure(|| {
        for i in 0..20 {
            dsp.set(if i % 2 == 0 { &a } else { &b });
            dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        }
    });
    assert_eq!(info.count_total, 0);
}
