//! Conformance of the loudness meter with the minimum-requirements test signals of
//! EBU Tech 3341 (2023, table 1) and EBU Tech 3342 (2023, table 1), generated here
//! from their definitions (ADR-0092 decision 4). The programme-based cases (3341 cases
//! 7 and 8, 3342 cases 5 and 6) need the EBU's recordings and are not run.
//!
//! Tolerances are the documents': M, S and I within ±0.1 LU, LRA within ±1 LU, true
//! peak within +0.2/-0.4 dB. Signals are fed in graph-sized blocks (480 frames, and
//! 512 and 1 where the block size could matter), at 48 kHz unless the test says
//! otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_audio::{Loudness, LoudnessMeter};
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::timing::SampleRate;

const RATE: u32 = 48_000;

fn format(rate: u32, layout: ChannelLayout) -> AudioFormat {
    AudioFormat::new(SampleRate::new(rate).unwrap(), layout).unwrap()
}

fn amp(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
}

/// A phase-continuous sine of `freq` Hz through segments of (seconds, peak dBFS);
/// a level of `None` is digital silence.
fn tones(rate: u32, freq: f64, segments: &[(f64, Option<f64>)]) -> Vec<f32> {
    let mut out = Vec::new();
    let mut n = 0u64;
    for &(secs, level) in segments {
        let frames = (secs * f64::from(rate)).round() as u64;
        for _ in 0..frames {
            let v = match level {
                Some(db) => {
                    amp(db) * (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(rate)).sin()
                }
                None => 0.0,
            };
            out.push(v as f32);
            n += 1;
        }
    }
    out
}

/// Feeds `signal` to every channel of `meter` in blocks of `block` frames and returns
/// the values after each block.
fn run_each(
    meter: &mut LoudnessMeter,
    signals: &[&[f32]],
    block: usize,
    mut each: impl FnMut(usize, Loudness),
) -> Loudness {
    let len = signals[0].len();
    let mut at = 0;
    while at < len {
        let n = block.min(len - at);
        let chans: Vec<&[f32]> = signals.iter().map(|s| &s[at..at + n]).collect();
        meter.process(&chans, n);
        at += n;
        each(at, meter.loudness());
    }
    meter.loudness()
}

fn stereo(signal: &[f32], block: usize) -> Loudness {
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    run_each(&mut m, &[signal, signal], block, |_, _| {})
}

#[track_caller]
fn near(value: f32, expected: f64, tol: f64) {
    assert!(
        (f64::from(value) - expected).abs() <= tol,
        "{value} is not within {tol} of {expected}"
    );
}

#[track_caller]
fn true_peak_ok(value: f32, expected: f64) {
    let d = f64::from(value) - expected;
    assert!(
        (-0.4..=0.2).contains(&d),
        "true peak {value} dBTP is outside {expected} +0.2/-0.4"
    );
}

// --- EBU Tech 3341, table 1 ---

#[test]
fn tech3341_case_1_reads_minus_23() {
    for block in [480, 512, 1] {
        let s = tones(RATE, 1000.0, &[(20.0, Some(-23.0))]);
        let l = stereo(&s, block);
        near(l.momentary, -23.0, 0.1);
        near(l.short_term, -23.0, 0.1);
        near(l.integrated, -23.0, 0.1);
    }
}

#[test]
fn tech3341_case_2_reads_minus_33() {
    let s = tones(RATE, 1000.0, &[(20.0, Some(-33.0))]);
    let l = stereo(&s, 480);
    near(l.momentary, -33.0, 0.1);
    near(l.short_term, -33.0, 0.1);
    near(l.integrated, -33.0, 0.1);
}

#[test]
fn tech3341_case_3_gates_the_quiet_tones() {
    let s = tones(
        RATE,
        1000.0,
        &[
            (10.0, Some(-36.0)),
            (60.0, Some(-23.0)),
            (10.0, Some(-36.0)),
        ],
    );
    near(stereo(&s, 480).integrated, -23.0, 0.1);
}

#[test]
fn tech3341_case_4_gates_absolutely_and_relatively() {
    let s = tones(
        RATE,
        1000.0,
        &[
            (10.0, Some(-72.0)),
            (10.0, Some(-36.0)),
            (60.0, Some(-23.0)),
            (10.0, Some(-36.0)),
            (10.0, Some(-72.0)),
        ],
    );
    near(stereo(&s, 512).integrated, -23.0, 0.1);
}

#[test]
fn tech3341_case_5_averages_power() {
    let s = tones(
        RATE,
        1000.0,
        &[
            (20.0, Some(-26.0)),
            (20.1, Some(-20.0)),
            (20.0, Some(-26.0)),
        ],
    );
    near(stereo(&s, 480).integrated, -23.0, 0.1);
}

#[test]
fn tech3341_case_6_weights_the_surrounds() {
    // 5.0 in a 5.1 layout: L, R at -28, C at -24, LFE silent, Ls, Rs at -30.
    let lr = tones(RATE, 1000.0, &[(20.0, Some(-28.0))]);
    let c = tones(RATE, 1000.0, &[(20.0, Some(-24.0))]);
    let lfe = vec![0.0f32; lr.len()];
    let s = tones(RATE, 1000.0, &[(20.0, Some(-30.0))]);
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Surround5_1));
    let l = run_each(&mut m, &[&lr, &lr, &c, &lfe, &s, &s], 480, |_, _| {});
    near(l.integrated, -23.0, 0.1);
}

#[test]
fn tech3341_case_9_short_term_is_constant_after_3_s() {
    let mut seg = Vec::new();
    for _ in 0..5 {
        seg.push((1.34, Some(-20.0)));
        seg.push((1.66, Some(-30.0)));
    }
    let s = tones(RATE, 1000.0, &seg);
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    let mut checked = 0;
    run_each(&mut m, &[&s, &s], 480, |at, l| {
        if at >= 3 * RATE as usize {
            near(l.short_term, -23.0, 0.1);
            checked += 1;
        }
    });
    assert!(checked > 1000);
}

#[test]
fn tech3341_case_11_live_max_short_term_steps_one_lu() {
    // (i * 0.15 s silence; 3 s at -38 + i dBFS; 3 - i * 0.15 s silence), i = 0..19.
    // A live meter's maximum short-term reading after each tone is -38 + i.
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    for i in 0..20 {
        let t = f64::from(i);
        let s = tones(
            RATE,
            1000.0,
            &[
                (t * 0.15, None),
                (3.0, Some(-38.0 + t)),
                (3.0 - t * 0.15, None),
            ],
        );
        // The maximum of each tone: a reset between tones, which also clears the
        // window (the silence before each tone is part of the signal).
        m.reset();
        let l = run_each(&mut m, &[&s, &s], 480, |_, _| {});
        near(l.max_short_term, -38.0 + t, 0.1);
    }
}

#[test]
fn tech3341_case_12_momentary_is_constant_after_1_s() {
    let mut seg = Vec::new();
    for _ in 0..25 {
        seg.push((0.18, Some(-20.0)));
        seg.push((0.22, Some(-30.0)));
    }
    let s = tones(RATE, 1000.0, &seg);
    for block in [480, 512] {
        let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
        let mut checked = 0;
        run_each(&mut m, &[&s, &s], block, |at, l| {
            // A value is published every 100 ms; it is checked once it covers 1 s.
            if at >= RATE as usize + 4800 {
                near(l.momentary, -23.0, 0.1);
                checked += 1;
            }
        });
        assert!(checked > 100);
    }
}

#[test]
fn tech3341_case_14_live_max_momentary_steps_one_lu() {
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    for i in 0..20 {
        let t = f64::from(i);
        let s = tones(
            RATE,
            1000.0,
            &[
                (t * 0.02, None),
                (0.4, Some(-38.0 + t)),
                (0.4 - t * 0.02, None),
            ],
        );
        m.reset();
        let l = run_each(&mut m, &[&s, &s], 480, |_, _| {});
        // Windows move in 10 ms steps, so a 400 ms window holds the whole tone.
        near(l.max_momentary, -38.0 + t, 0.1);
    }
}

/// A stereo sine of `freq` with `amplitude` and starting `phase` degrees, with 10 ms
/// raised-cosine fades, at `rate`.
fn tp_tone(rate: u32, freq: f64, amplitude: f64, phase: f64) -> Vec<f32> {
    let n = rate as usize; // 1 s
    let fade = (rate / 100) as usize;
    (0..n)
        .map(|i| {
            let w = if i < fade {
                0.5 - 0.5 * (std::f64::consts::PI * i as f64 / fade as f64).cos()
            } else if i >= n - fade {
                0.5 - 0.5 * (std::f64::consts::PI * (n - 1 - i) as f64 / fade as f64).cos()
            } else {
                1.0
            };
            let x =
                2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate) + phase.to_radians();
            (w * amplitude * x.sin()) as f32
        })
        .collect()
}

#[test]
fn tech3341_cases_15_to_19_true_peak_of_sines() {
    let fs = f64::from(RATE);
    let cases = [
        (fs / 4.0, 0.5, 0.0, -6.0),
        (fs / 4.0, 0.5, 45.0, -6.0),
        (fs / 6.0, 0.5, 60.0, -6.0),
        (fs / 8.0, 0.5, 67.5, -6.0),
        (fs / 4.0, 1.41, 45.0, 3.0),
    ];
    for (k, (freq, a, phase, expected)) in cases.into_iter().enumerate() {
        let s = tp_tone(RATE, freq, a, phase);
        let l = stereo(&s, 480);
        assert!(
            (-0.4..=0.2).contains(&(f64::from(l.true_peak) - expected)),
            "case {}: {} dBTP, expected {expected}",
            15 + k,
            l.true_peak
        );
    }
}

/// Test signals 20 to 23: a sine at fs/6, 0.5 FFS, holding one period of a sine at fs/4,
/// 1.0 FFS, continuous in phase, synthesized at 4 fs, low-pass filtered and taken
/// down to fs at `offset` (0 to 3).
fn tp_burst(offset: usize) -> Vec<f32> {
    let up = 4;
    let rate = f64::from(RATE) * up as f64;
    // At 4 fs, a period of fs/6 is 24 samples and one of fs/4 is 16.
    let mut s = Vec::new();
    let half = (RATE as usize / 2) * up;
    let low = |i: usize| {
        0.5 * (2.0 * std::f64::consts::PI * (f64::from(RATE) / 6.0) * i as f64 / rate).sin()
    };
    let mut i = 0;
    while s.len() < half {
        s.push(low(i));
        i = (i + 1) % 24;
    }
    // Starts at a rising zero crossing, as `low` does after whole periods.
    let high = |j: usize| (2.0 * std::f64::consts::PI * j as f64 / 16.0).sin();
    s.extend((0..16).map(high));
    for k in 0..half {
        s.push(low(k % 24));
    }
    // Fades of 10 ms at 4 fs.
    let fade = 4 * RATE as usize / 100;
    let n = s.len();
    for k in 0..fade {
        let w = 0.5 - 0.5 * (std::f64::consts::PI * k as f64 / fade as f64).cos();
        s[k] *= w;
        s[n - 1 - k] *= w;
    }
    // Anti-alias low pass at 0.45 fs (of the target rate): Blackman-windowed sinc.
    let taps = 255;
    let fc = 0.45 / up as f64;
    let c = (taps - 1) as f64 / 2.0;
    let h: Vec<f64> = (0..taps)
        .map(|k| {
            let t = k as f64 - c;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (taps - 1) as f64).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / (taps - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let gain: f64 = h.iter().sum();
    let mut out = Vec::new();
    let mut k = offset;
    while k < n {
        let mut acc = 0.0;
        for (j, hj) in h.iter().enumerate() {
            let idx = k as isize + j as isize - c as isize;
            if idx >= 0 && (idx as usize) < n {
                acc += hj * s[idx as usize];
            }
        }
        out.push((acc / gain) as f32);
        k += up;
    }
    out
}

#[test]
fn tech3341_cases_20_to_23_true_peak_between_samples() {
    for offset in 0..4 {
        let s = tp_burst(offset);
        let l = stereo(&s, 480);
        true_peak_ok(l.true_peak, 0.0);
        if offset == 2 {
            // Halfway between the 4 fs samples of the burst's peak, the samples alone
            // under-read by more than the tolerance; the meter does not.
            let sample_peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(20.0 * sample_peak.log10() < -0.4, "{sample_peak}");
        }
    }
}

// --- EBU Tech 3342, table 1 ---

fn lra(levels: &[f64]) -> f32 {
    let seg: Vec<_> = levels.iter().map(|&l| (20.0, Some(l))).collect();
    let s = tones(RATE, 1000.0, &seg);
    stereo(&s, 480).range
}

#[test]
fn tech3342_case_1_reads_10_lu() {
    near(lra(&[-20.0, -30.0]), 10.0, 1.0);
}

#[test]
fn tech3342_case_2_reads_5_lu() {
    near(lra(&[-20.0, -15.0]), 5.0, 1.0);
}

#[test]
fn tech3342_case_3_reads_20_lu() {
    near(lra(&[-40.0, -20.0]), 20.0, 1.0);
}

#[test]
fn tech3342_case_4_reads_15_lu() {
    near(lra(&[-50.0, -35.0, -20.0, -35.0, -50.0]), 15.0, 1.0);
}

// --- Other rates, mono, silence, reset ---

#[test]
fn case_1_reads_the_same_at_44_1_and_96_khz() {
    for rate in [44_100, 96_000] {
        let s = tones(rate, 1000.0, &[(20.0, Some(-23.0))]);
        let mut m = LoudnessMeter::new(format(rate, ChannelLayout::Stereo));
        let l = run_each(&mut m, &[&s, &s], 480, |_, _| {});
        near(l.integrated, -23.0, 0.1);
        near(l.momentary, -23.0, 0.1);
    }
}

#[test]
fn true_peak_holds_at_44_1_and_96_khz() {
    // The same audio frequencies as cases 16 and 18 at 48 kHz (12 and 6 kHz): 96 kHz
    // oversamples by 2, to the same interpolated rate as 48 kHz by 4.
    for (rate, freq) in [(44_100, 11_025.0), (96_000, 12_000.0), (96_000, 6_000.0)] {
        let s = tp_tone(rate, freq, 0.5, 45.0);
        let mut m = LoudnessMeter::new(format(rate, ChannelLayout::Stereo));
        let l = run_each(&mut m, &[&s, &s], 480, |_, _| {});
        true_peak_ok(l.true_peak, -6.0);
    }
}

#[test]
fn a_mono_graph_measures_one_channel_with_weight_one() {
    // The same sine on one channel reads 3 dB below the stereo pair (BS.1770 sums
    // the channels' power; a mono graph is one loudspeaker).
    let s = tones(RATE, 1000.0, &[(10.0, Some(-23.0))]);
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Mono));
    let l = run_each(&mut m, &[&s], 480, |_, _| {});
    near(l.integrated, -26.01, 0.1);
}

#[test]
fn silence_reads_nothing_and_leaves_the_integrated_value() {
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    let quiet = vec![0.0f32; 5 * RATE as usize];
    let l = run_each(&mut m, &[&quiet, &quiet], 480, |_, _| {});
    assert_eq!(l.integrated, f32::NEG_INFINITY);
    assert_eq!(l.momentary, f32::NEG_INFINITY);
    assert!(l.momentary < -70.0);
    let tone = tones(RATE, 1000.0, &[(20.0, Some(-23.0))]);
    run_each(&mut m, &[&tone, &tone], 480, |_, _| {});
    let l = run_each(&mut m, &[&quiet, &quiet], 480, |_, _| {});
    // Silence is gated out: the programme still reads -23 (the blocks that straddle
    // the tone's start and end count, as on any meter).
    near(l.integrated, -23.0, 0.1);
    assert_eq!(l.momentary, f32::NEG_INFINITY);
}

#[test]
fn reset_forgets_everything_heard_before() {
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    let loud = tones(RATE, 1000.0, &[(5.0, Some(-13.0))]);
    run_each(&mut m, &[&loud, &loud], 480, |_, _| {});
    m.reset();
    assert_eq!(m.loudness(), Loudness::NONE);
    let quiet = tones(RATE, 1000.0, &[(10.0, Some(-33.0))]);
    let l = run_each(&mut m, &[&quiet, &quiet], 480, |_, _| {});
    near(l.integrated, -33.0, 0.1);
    near(l.max_short_term, -33.0, 0.1);
    true_peak_ok(l.true_peak, -33.0);
    m.clear();
    assert_eq!(m.loudness(), Loudness::NONE);
}

#[test]
fn processing_does_not_allocate() {
    let mut m = LoudnessMeter::new(format(RATE, ChannelLayout::Stereo));
    let s = tones(RATE, 1000.0, &[(4.0, Some(-23.0))]);
    let blocks: Vec<&[f32]> = s.chunks(480).collect();
    let counts = allocation_counter::measure(|| {
        for b in &blocks {
            m.process(&[b, b], b.len());
        }
    });
    assert_eq!(counts.count_total, 0);
    near(m.loudness().momentary, -23.0, 0.1);
    let counts = allocation_counter::measure(|| m.reset());
    assert_eq!(counts.count_total, 0);
}
