//! Compressor (ADR-0130 decision 6): static curve, timing, bit-exact cases, the key,
//! allocation.

#![allow(clippy::expect_used)]

use rbs_audio_filters::compressor::{COMPRESSOR, Compressor, CompressorDsp};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, conformance, find};
use rbs_core::media::AudioFormat;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

fn values(pairs: &[(&str, Value)]) -> Values {
    Values::from_pairs(&Compressor as &dyn AudioFilterType, pairs.iter().copied()).expect("valid")
}

fn amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Runs `blocks` blocks of a square wave of `level_db` (constant magnitude, so RMS and
/// peak agree) and returns the output magnitude of the last sample in dB.
fn settle(dsp: &mut dyn AudioDsp, level_db: f32, blocks: usize) -> f32 {
    let a = amp(level_db);
    let mut last = 0.0;
    for _ in 0..blocks {
        let mut l: Vec<f32> = (0..FRAMES)
            .map(|i| if i % 2 == 0 { a } else { -a })
            .collect();
        let mut r = l.clone();
        dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        last = l[FRAMES - 1].abs();
    }
    20.0 * last.log10()
}

#[test]
fn it_is_in_the_catalogue_and_conforms() {
    assert!(find(COMPRESSOR).is_some());
    assert_eq!(conformance::check_type(&Compressor), Vec::<String>::new());
    assert_eq!(Compressor.latency_frames(setup()), 0);
}

#[test]
fn the_static_curve_matches_threshold_ratio_and_knee() {
    let v = values(&[
        ("threshold", Value::Number(-20.0)),
        ("ratio", Value::Number(4.0)),
        ("knee", Value::Number(6.0)),
        ("makeup", Value::Number(3.0)),
    ]);
    for (level, expect_gr) in [
        (-40.0f32, 0.0f32),
        (-23.0, 0.0),
        // Inside the knee: (1/4 - 1) * (x - T + W/2)^2 / (2W).
        (-20.0, -0.75 * 9.0 / 12.0),
        (-18.0, -0.75 * 25.0 / 12.0),
        // Above it: (1/4 - 1) * (x - T).
        (-10.0, -7.5),
        (0.0, -15.0),
    ] {
        let mut dsp = CompressorDsp::new(setup(), &v);
        assert!((dsp.static_gain_db(level) - expect_gr).abs() < 1e-4);
        let out = settle(&mut dsp, level, 100);
        let want = level + expect_gr + 3.0;
        assert!(
            (out - want).abs() < 0.1,
            "level {level}: {out} dB, want {want}"
        );
    }
}

#[test]
fn attack_and_release_follow_their_times() {
    let v = values(&[
        ("threshold", Value::Number(-30.0)),
        ("ratio", Value::Number(20.0)),
        ("knee", Value::Number(0.0)),
        ("detector", Value::Choice(1)),
        ("attack", Value::Number(10.0)),
        ("release", Value::Number(100.0)),
    ]);
    let mut dsp = CompressorDsp::new(setup(), &v);
    settle(&mut dsp, -60.0, 10);
    // Peak detector, step to -10 dB: the target is about -19 dB; after one time
    // constant (10 ms, one block) about 63 % of it, after five nearly all.
    settle(&mut dsp, -10.0, 1);
    let after_one = dsp.reduction_db();
    let target = dsp.static_gain_db(-10.0);
    assert!(
        (after_one / target - 0.632).abs() < 0.05,
        "{after_one} of {target}"
    );
    settle(&mut dsp, -10.0, 4);
    assert!((dsp.reduction_db() / target) > 0.99);
    // Release: 100 ms back toward 0 from the settled value.
    let start = dsp.reduction_db();
    settle(&mut dsp, -60.0, 10);
    assert!((dsp.reduction_db() / start - 0.368).abs() < 0.05);
}

#[test]
fn below_threshold_and_dry_mix_are_bit_exact() {
    let signal: Vec<f32> = (0..FRAMES)
        .map(|i| 0.01 * (i as f32 * 0.05).sin())
        .collect();
    for v in [
        values(&[]),
        values(&[
            ("mix", Value::Number(0.0)),
            ("threshold", Value::Number(-60.0)),
        ]),
    ] {
        let mut dsp = CompressorDsp::new(setup(), &v);
        for _ in 0..5 {
            let mut l = signal.clone();
            let mut r = signal.clone();
            dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            assert_eq!(l, signal);
            assert_eq!(r, signal);
        }
    }
}

#[test]
fn a_key_source_drives_the_reduction() {
    let keyed = values(&[
        ("key_source", Value::Source(Some(7))),
        ("threshold", Value::Number(-30.0)),
    ]);
    let loud = vec![0.5f32; FRAMES];
    let quiet = vec![0.001f32; FRAMES];
    // Loud own signal, key missing: silence, no reduction (the ducker's rule).
    let mut dsp = CompressorDsp::new(setup(), &keyed);
    for _ in 0..20 {
        let (mut l, mut r) = (loud.clone(), loud.clone());
        dsp.process_keyed(&mut [&mut l[..], &mut r[..]], None, FRAMES);
        assert_eq!(l, loud);
    }
    // Quiet own signal, loud key: reduced.
    for _ in 0..20 {
        let (mut l, mut r) = (quiet.clone(), quiet.clone());
        dsp.process_keyed(
            &mut [&mut l[..], &mut r[..]],
            Some(&[&loud[..], &loud[..]]),
            FRAMES,
        );
    }
    assert!(dsp.reduction_db() < -10.0);
    // Without a key source the own signal is the detector.
    let mut own = CompressorDsp::new(setup(), &values(&[("threshold", Value::Number(-30.0))]));
    for _ in 0..20 {
        let (mut l, mut r) = (loud.clone(), loud.clone());
        own.process_keyed(&mut [&mut l[..], &mut r[..]], None, FRAMES);
    }
    assert!(own.reduction_db() < -10.0);
}

#[test]
fn processing_and_set_do_not_allocate() {
    let mut dsp = Compressor.make(setup(), &values(&[])).expect("valid");
    let other = values(&[
        ("ratio", Value::Number(8.0)),
        ("detector", Value::Choice(1)),
    ]);
    let mut l = vec![0.4f32; FRAMES];
    let mut r = vec![0.4f32; FRAMES];
    let info = allocation_counter::measure(|| {
        for _ in 0..20 {
            dsp.set(&other);
            dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        }
    });
    assert_eq!(info.count_total, 0);
}

#[test]
fn a_block_longer_than_the_setup_gives_the_same_output() {
    // `work` is sized for the setup's block; a longer block is processed in parts.
    let v = values(&[
        ("threshold", Value::Number(-30.0)),
        ("attack", Value::Number(1.0)),
    ]);
    let mut small = CompressorDsp::new(DspSetup::new(AudioFormat::STEREO_48K, 128), &v);
    let mut full = CompressorDsp::new(setup(), &v);
    let signal: Vec<f32> = (0..FRAMES)
        .map(|i| 0.5 * (i as f32 * 0.05).sin() * (i as f32 / FRAMES as f32))
        .collect();
    for _ in 0..3 {
        let (mut a, mut b) = (signal.clone(), signal.clone());
        small.process(&mut [&mut a[..]], FRAMES);
        full.process(&mut [&mut b[..]], FRAMES);
        assert_eq!(a, b);
    }
}

#[test]
fn a_quiet_stretch_lets_the_reduction_recover() {
    // The idle shortcut must not freeze a reduction that is still releasing.
    let v = values(&[
        ("threshold", Value::Number(-30.0)),
        ("release", Value::Number(10.0)),
    ]);
    let mut dsp = CompressorDsp::new(setup(), &v);
    settle(&mut dsp, -6.0, 20);
    assert!(dsp.reduction_db() < -10.0);
    settle(&mut dsp, -80.0, 50);
    assert_eq!(dsp.reduction_db(), 0.0);
}
