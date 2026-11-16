//! The denoiser against upstream nnnoiseless, and its real-time contract (ADR-0130
//! decision 5).

#![allow(clippy::expect_used)]

use nnnoiseless::DenoiseState;
use rbs_denoise::{Denoiser, FRAME_SIZE};

/// Deterministic test signal in the i16 range: noise plus a tone that comes and goes.
fn signal(frames: usize) -> Vec<f32> {
    let mut seed = 7u32;
    (0..frames * FRAME_SIZE)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            let on = (i / 9600) % 2 == 0;
            let tone = if on {
                6000.0 * (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin()
            } else {
                0.0
            };
            2000.0 * noise + tone
        })
        .collect()
}

fn frame(x: &[f32]) -> [f32; FRAME_SIZE] {
    let mut f = [0.0; FRAME_SIZE];
    f.copy_from_slice(x);
    f
}

#[test]
fn agrees_with_upstream_below_minus_90_dbfs() {
    let input = signal(200);
    let mut ours = Denoiser::new().expect("model");
    let mut theirs = DenoiseState::new();
    let mut worst = 0.0f32;
    let mut vad_worst = 0.0f32;
    let (mut a, mut b) = ([0.0f32; FRAME_SIZE], [0.0f32; FRAME_SIZE]);
    for chunk in input.chunks_exact(FRAME_SIZE) {
        let va = ours.process_frame(&mut a, &frame(chunk));
        let vb = theirs.process_frame(&mut b, chunk);
        for (x, y) in a.iter().zip(&b) {
            worst = worst.max((x - y).abs());
        }
        vad_worst = vad_worst.max((va - vb).abs());
    }
    // -90 dBFS of the i16 full scale.
    assert!(
        worst < 32768.0 * 10f32.powf(-90.0 / 20.0),
        "difference {worst}"
    );
    assert!(vad_worst < 1e-3, "voice probability difference {vad_worst}");
}

#[test]
fn the_output_is_one_frame_late() {
    // A click in frame 3 shows up in the output of frame 4, not 3.
    let mut d = Denoiser::new().expect("model");
    d.set_max_reduction_db(0.0);
    let mut out = [0.0f32; FRAME_SIZE];
    let mut energy = Vec::new();
    for n in 0..8 {
        let mut x = [0.0f32; FRAME_SIZE];
        if n == 3 {
            for (i, s) in x.iter_mut().enumerate() {
                *s = 10_000.0 * (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin();
            }
        }
        d.process_frame(&mut out, &x);
        energy.push(out.iter().map(|y| y * y).sum::<f32>());
    }
    let loudest = energy
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i);
    assert_eq!(loudest, Some(4));
}

#[test]
fn the_reduction_is_limited_by_the_floor() {
    // Steady low-passed noise, like a fan: without a floor it is cut deeply (upstream
    // RNNoise removes about 40 dB of it); with 6 dB it loses about 6. (Flat white noise
    // is a poor case for the model and is not used.)
    let mut seed = 3u32;
    let mut lp = 0.0f32;
    let input: Vec<f32> = (0..300 * FRAME_SIZE)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let white = 1000.0 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
            lp = 0.97 * lp + 0.03 * white;
            4.0 * lp
        })
        .collect();
    let run = |db: f32| {
        let mut d = Denoiser::new().expect("model");
        d.set_max_reduction_db(db);
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        let mut out = [0.0f32; FRAME_SIZE];
        for (n, chunk) in input.chunks_exact(FRAME_SIZE).enumerate() {
            d.process_frame(&mut out, &frame(chunk));
            if n > 100 {
                e_in += chunk.iter().map(|&x| f64::from(x * x)).sum::<f64>();
                e_out += out.iter().map(|&y| f64::from(y * y)).sum::<f64>();
            }
        }
        10.0 * (e_out / e_in).log10()
    };
    let unlimited = run(f32::INFINITY);
    let limited = run(6.0);
    assert!(unlimited < -20.0, "noise reduced by only {unlimited} dB");
    assert!(limited > -7.5 && limited < 0.5, "floor gave {limited} dB");
}

#[test]
fn nan_infinity_and_full_scale_give_finite_output() {
    let mut d = Denoiser::new().expect("model");
    let mut out = [0.0f32; FRAME_SIZE];
    for x in [
        f32::NAN,
        f32::INFINITY,
        -f32::INFINITY,
        32767.0,
        -32768.0,
        1.0e-40,
    ] {
        for _ in 0..4 {
            let p = d.process_frame(&mut out, &[x; FRAME_SIZE]);
            assert!(out.iter().all(|y| y.is_finite()));
            assert!((0.0..=1.0).contains(&p));
        }
    }
}

#[test]
fn reset_gives_the_same_output_as_new() {
    let input = signal(20);
    let mut a = Denoiser::new().expect("model");
    let mut out = [0.0f32; FRAME_SIZE];
    for chunk in input.chunks_exact(FRAME_SIZE) {
        a.process_frame(&mut out, &frame(chunk));
    }
    a.reset();
    let mut b = Denoiser::new().expect("model");
    let (mut oa, mut ob) = ([0.0f32; FRAME_SIZE], [0.0f32; FRAME_SIZE]);
    for chunk in input.chunks_exact(FRAME_SIZE) {
        a.process_frame(&mut oa, &frame(chunk));
        b.process_frame(&mut ob, &frame(chunk));
        assert_eq!(oa, ob);
    }
}

#[test]
fn process_frame_and_reset_do_not_allocate() {
    let input = signal(10);
    let mut d = Denoiser::new().expect("model");
    let mut out = [0.0f32; FRAME_SIZE];
    // Includes the very first call on this thread.
    let counts = allocation_counter::measure(|| {
        for chunk in input.chunks_exact(FRAME_SIZE) {
            d.process_frame(&mut out, &frame(chunk));
        }
        d.reset();
        d.set_max_reduction_db(12.0);
        d.process_frame(&mut out, &frame(&input[..FRAME_SIZE]));
    });
    assert_eq!(counts.count_total, 0);
}
