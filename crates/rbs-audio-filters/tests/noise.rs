//! Noise suppression (ADR-0130 decision 5): latency, alignment with RNNoise, reduction,
//! the voice gate, the 48 kHz rule and the real-time contract.

#![allow(clippy::expect_used)]

use rbs_audio_filters::noise::{NOISE_SUPPRESSION, NoiseSuppression};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, find};
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::timing::SampleRate;
use rbs_denoise::{Denoiser, FRAME_SIZE};
use rbs_params::{Value, Values};

fn setup(block: u32) -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, block)
}

fn values(pairs: &[(&str, Value)]) -> Values {
    Values::from_pairs(
        &NoiseSuppression as &dyn AudioFilterType,
        pairs.iter().copied(),
    )
    .expect("valid")
}

/// Fan-like noise (low-passed white noise) at about -30 dBFS, deterministic.
fn fan(samples: usize) -> Vec<f32> {
    let mut seed = 11u32;
    let mut lp = 0.0f32;
    (0..samples)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let white = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            lp = 0.97 * lp + 0.03 * white;
            0.12 * lp
        })
        .collect()
}

/// Runs `input` (mono, copied to both channels) through `dsp` in blocks of `block`.
fn run(dsp: &mut dyn AudioDsp, input: &[f32], block: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(input.len());
    for chunk in input.chunks(block) {
        let mut l = vec![0.0f32; block];
        let mut r = vec![0.0f32; block];
        l[..chunk.len()].copy_from_slice(chunk);
        r[..chunk.len()].copy_from_slice(chunk);
        dsp.process(&mut [&mut l[..], &mut r[..]], block);
        assert_eq!(l, r, "one pass writes the same output to both channels");
        out.extend_from_slice(&l[..chunk.len()]);
    }
    out
}

#[test]
fn it_is_in_the_catalogue() {
    assert!(find(NOISE_SUPPRESSION).is_some());
}

#[test]
fn latency_is_960_minus_gcd() {
    let ty = NoiseSuppression;
    for (block, latency) in [
        (480, 480),
        (960, 480),
        (256, 928),
        (1024, 928),
        (441, 957),
        (1, 959),
    ] {
        assert_eq!(ty.latency_frames(setup(block)), latency, "block {block}");
        let dsp = ty
            .make(setup(block), &Values::defaults(&ty as &dyn AudioFilterType))
            .expect("48 kHz");
        assert_eq!(dsp.latency_frames(), latency, "block {block}");
    }
}

#[test]
fn the_output_is_rnnoise_delayed_by_the_reported_latency() {
    // Reference: RNNoise frame by frame on the same signal (its own delay is one frame).
    let input: Vec<f32> = fan(60 * FRAME_SIZE)
        .iter()
        .enumerate()
        .map(|(i, &x)| x + 0.2 * (i as f32 * 300.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    let mut den = Denoiser::new().expect("model");
    den.set_max_reduction_db(30.0);
    let mut reference = Vec::new();
    let mut frame_out = [0.0f32; FRAME_SIZE];
    for chunk in input.chunks_exact(FRAME_SIZE) {
        let mut f = [0.0f32; FRAME_SIZE];
        for (d, &x) in f.iter_mut().zip(chunk) {
            *d = x * 32768.0;
        }
        den.process_frame(&mut frame_out, &f);
        reference.extend(frame_out.iter().map(|y| y / 32768.0));
    }
    for block in [480usize, 256, 1024] {
        let ty = NoiseSuppression;
        let mut dsp = ty.make(setup(block as u32), &values(&[])).expect("48 kHz");
        let extra = dsp.latency_frames() as usize - FRAME_SIZE;
        let out = run(dsp.as_mut(), &input, block);
        for t in extra..out.len() {
            let want = reference[t - extra];
            assert!(
                (out[t] - want).abs() < 1e-6,
                "block {block}, sample {t}: {} vs {want}",
                out[t]
            );
        }
        assert!(out[..extra].iter().all(|&y| y == 0.0));
    }
}

fn reduction_db(out: &[f32], input: &[f32], skip: usize) -> f64 {
    let e = |s: &[f32]| s[skip..].iter().map(|&x| f64::from(x * x)).sum::<f64>();
    10.0 * (e(out) / e(input)).log10()
}

#[test]
fn fan_noise_is_reduced_by_at_most_max_reduction() {
    let input = fan(300 * FRAME_SIZE);
    for limit in [6.0f32, 30.0] {
        let mut dsp = NoiseSuppression
            .make(
                setup(480),
                &values(&[("max_reduction", Value::Number(limit))]),
            )
            .expect("48 kHz");
        let out = run(dsp.as_mut(), &input, 480);
        let db = reduction_db(&out, &input, 100 * FRAME_SIZE);
        assert!(db > -f64::from(limit) - 1.5, "limit {limit}: {db} dB");
        assert!(db < -f64::from(limit) * 0.6, "limit {limit}: only {db} dB");
    }
}

#[test]
fn the_voice_gate_lowers_noise_further() {
    let input = fan(300 * FRAME_SIZE);
    let reduction = |gate: bool| {
        let mut dsp = NoiseSuppression
            .make(
                setup(480),
                &values(&[
                    ("max_reduction", Value::Number(12.0)),
                    ("voice_gate", Value::Toggle(gate)),
                ]),
            )
            .expect("48 kHz");
        let out = run(dsp.as_mut(), &input, 480);
        reduction_db(&out, &input, 100 * FRAME_SIZE)
    };
    let open = reduction(false);
    let gated = reduction(true);
    assert!(gated < open - 10.0, "gate off {open} dB, on {gated} dB");
}

#[test]
fn other_rates_are_refused_and_have_no_latency() {
    let f = AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).expect("format");
    let s = DspSetup::new(f, 441);
    assert!(NoiseSuppression.make(s, &values(&[])).is_err());
    assert_eq!(NoiseSuppression.latency_frames(s), 0);
}

#[test]
fn processing_set_and_a_mode_switch_do_not_allocate() {
    let mut dsp = NoiseSuppression
        .make(setup(256), &values(&[]))
        .expect("48 kHz");
    let separate = values(&[
        ("mode", Value::Choice(1)),
        ("voice_gate", Value::Toggle(true)),
    ]);
    let mono = values(&[]);
    let input = fan(256);
    let mut l = input.clone();
    let mut r = input.clone();
    let info = allocation_counter::measure(|| {
        for i in 0..40 {
            if i == 10 {
                dsp.set(&separate);
            }
            if i == 30 {
                dsp.set(&mono);
            }
            dsp.process(&mut [&mut l[..], &mut r[..]], 256);
        }
        dsp.reset();
    });
    assert_eq!(info.count_total, 0);
}

#[test]
fn separate_mode_keeps_channels_apart() {
    let mut dsp = NoiseSuppression
        .make(
            setup(480),
            &values(&[
                ("mode", Value::Choice(1)),
                ("max_reduction", Value::Number(6.0)),
            ]),
        )
        .expect("48 kHz");
    let tone: Vec<f32> = (0..30 * FRAME_SIZE)
        .map(|i| 0.3 * (i as f32 * 300.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    let mut energy_r = 0.0f32;
    for chunk in tone.chunks_exact(480) {
        let mut l = chunk.to_vec();
        let mut r = vec![0.0f32; 480];
        dsp.process(&mut [&mut l[..], &mut r[..]], 480);
        energy_r += r.iter().map(|y| y * y).sum::<f32>();
    }
    assert_eq!(energy_r, 0.0, "a silent right channel stays silent");
}
