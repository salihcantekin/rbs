//! Limiter (ADR-0130 decision 7): the ceiling holds on hard signals with the guard
//! clamp idle, the delay is the look-ahead, release, bit-exact delay, allocation.

#![allow(clippy::expect_used)]

use rbs_audio_filters::limiter::{LIMITER, Limiter, LimiterDsp};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, conformance, find};
use rbs_core::media::AudioFormat;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;
/// 2 ms at 48 kHz.
const W: usize = 96;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

fn values(pairs: &[(&str, Value)]) -> Values {
    Values::from_pairs(&Limiter as &dyn AudioFilterType, pairs.iter().copied()).expect("valid")
}

fn amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Runs `signal` (both channels the same, the right one negated) through a limiter with
/// a ceiling of `ceiling_db`, in blocks of `block`, and returns the output of both
/// channels and the guard count.
fn run(signal: &[f32], ceiling_db: f32, block: usize) -> (Vec<f32>, Vec<f32>, u64) {
    let v = values(&[("ceiling", Value::Number(ceiling_db))]);
    let mut dsp = LimiterDsp::new(DspSetup::new(AudioFormat::STEREO_48K, block as u32), &v);
    let mut out_l = Vec::with_capacity(signal.len());
    let mut out_r = Vec::with_capacity(signal.len());
    for chunk in signal.chunks(block) {
        let mut l = chunk.to_vec();
        let mut r: Vec<f32> = chunk.iter().map(|x| -x).collect();
        dsp.process(&mut [&mut l[..], &mut r[..]], chunk.len());
        out_l.extend_from_slice(&l);
        out_r.extend_from_slice(&r);
    }
    (out_l, out_r, dsp.guard_hits())
}

fn assert_under(signal: &[f32], ceiling_db: f32, what: &str) {
    for block in [FRAMES, 64, 37] {
        let (l, r, hits) = run(signal, ceiling_db, block);
        let limit = amp(ceiling_db);
        let top = l.iter().chain(&r).fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(
            top <= limit,
            "{what}, block {block}: peak {top} over {limit}"
        );
        assert_eq!(hits, 0, "{what}, block {block}: the guard clamp acted");
    }
}

/// A deterministic noise in -1..1 (xorshift).
fn noise(len: usize, seed: u32) -> Vec<f32> {
    let mut s = seed.max(1);
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

#[test]
fn it_is_in_the_catalogue_conforms_and_reports_its_look_ahead() {
    assert!(find(LIMITER).is_some());
    assert_eq!(conformance::check_type(&Limiter), Vec::<String>::new());
    assert_eq!(Limiter.latency_frames(setup()), W as u32);
    let dsp = Limiter.make(setup(), &values(&[])).expect("valid");
    assert_eq!(dsp.latency_frames(), W as u32);
}

#[test]
fn full_scale_squares_stay_under_the_ceiling() {
    for period in [2usize, 20, 200, 2000] {
        let s: Vec<f32> = (0..20_000)
            .map(|i| if (i / period) % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        for ceiling in [-1.0, -6.0, -30.0] {
            assert_under(&s, ceiling, &format!("square {period}, ceiling {ceiling}"));
        }
    }
}

#[test]
fn rising_impulses_inside_one_window_stay_under_the_ceiling() {
    let mut s = vec![0.0f32; 4000];
    for (k, i) in (1000..1000 + W).step_by(7).enumerate() {
        s[i] = 0.2 + 0.1 * k as f32;
    }
    assert_under(&s, -1.0, "rising impulses");
}

#[test]
fn a_single_impulse_at_every_offset_stays_under_the_ceiling() {
    for offset in 0..=W + 1 {
        let mut s = vec![0.05f32; 1000];
        s[500 + offset] = 4.0;
        assert_under(&s, -1.0, &format!("impulse at {offset}"));
    }
}

#[test]
fn noise_at_plus_12_db_stays_under_the_ceiling() {
    let s: Vec<f32> = noise(48_000, 7).iter().map(|x| x * amp(12.0)).collect();
    assert_under(&s, -1.0, "noise +12 dB");
    assert_under(&s, -20.0, "noise +12 dB, ceiling -20");
}

#[test]
fn quiet_input_is_delayed_by_the_look_ahead_bit_exactly() {
    let s: Vec<f32> = noise(4 * FRAMES, 3).iter().map(|x| x * 0.5).collect();
    for block in [FRAMES, 64, 37] {
        let (l, r, _) = run(&s, -1.0, block);
        assert!(l[..W].iter().all(|&x| x == 0.0));
        assert_eq!(&l[W..], &s[..s.len() - W]);
        let neg: Vec<f32> = s.iter().map(|x| -x).collect();
        assert_eq!(&r[W..], &neg[..s.len() - W]);
    }
}

#[test]
fn the_gain_recovers_within_the_release_time() {
    let v = values(&[("release", Value::Number(10.0))]);
    let mut dsp = Limiter.make(setup(), &v).expect("valid");
    // One loud block, then quiet: the released gain is within 1e-6 of 1 (and snaps to
    // it) after about 12 release time constants, so 200 ms later the quiet signal
    // passes at full level again; halfway through the first 10 ms it is still lowered.
    let mut loud = vec![1.0f32; FRAMES];
    let mut copy = loud.clone();
    dsp.process(&mut [&mut loud[..], &mut copy[..]], FRAMES);
    let mut first = Vec::new();
    let mut last = 0.0;
    for b in 0..20 {
        let mut l = vec![0.1f32; FRAMES];
        let mut r = l.clone();
        dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        if b == 0 {
            first = l.clone();
        }
        last = l[FRAMES - 1];
    }
    assert!(first[W + 1] < 0.1 * 0.95, "{}", first[W + 1]);
    assert_eq!(last, 0.1);
}

#[test]
fn processing_and_set_do_not_allocate() {
    let mut dsp = Limiter.make(setup(), &values(&[])).expect("valid");
    let other = values(&[
        ("ceiling", Value::Number(-6.0)),
        ("release", Value::Number(5.0)),
    ]);
    let mut l = noise(FRAMES, 5);
    let mut r = l.clone();
    let info = allocation_counter::measure(|| {
        for _ in 0..20 {
            dsp.set(&other);
            dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        }
    });
    assert_eq!(info.count_total, 0);
}

#[test]
fn bursts_of_many_levels_stay_under_the_ceiling() {
    // Noise bursts from -6 to +24 dB over the ceiling with gaps shorter and longer than
    // the look-ahead, for several seeds and ceilings.
    for seed in 1..12u32 {
        let base = noise(12_000, seed);
        let s: Vec<f32> = base
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let burst = (i / (31 + seed as usize * 17)) % 4;
                x * amp(-6.0 + 10.0 * burst as f32)
            })
            .collect();
        for ceiling in [-0.1, -3.0, -12.0] {
            assert_under(
                &s,
                ceiling,
                &format!("bursts seed {seed}, ceiling {ceiling}"),
            );
        }
    }
}
