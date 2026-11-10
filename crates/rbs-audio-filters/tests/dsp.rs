//! DSP behaviour of Gain and Noise gate, driven by blocks (no sleeping, no clock).

#![allow(clippy::expect_used)]

use rbs_audio_filters::gain::Gain;
use rbs_audio_filters::gate::{GateDsp, NoiseGate};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, catalogue, conformance};
use rbs_core::media::AudioFormat;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;
const RATE: f32 = 48_000.0;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

fn gate_values(pairs: &[(&'static str, f32)]) -> Values {
    let ty: &dyn AudioFilterType = &NoiseGate;
    Values::from_pairs(ty, pairs.iter().map(|&(k, v)| (k, Value::Number(v)))).expect("valid")
}

fn gain_values(db: f32) -> Values {
    Values::from_pairs(&Gain as &dyn AudioFilterType, [("gain", Value::Number(db))]).expect("valid")
}

/// One block of a sine of `amp` at `hz`, starting at sample `start`.
fn sine(amp: f32, hz: f32, start: usize) -> Vec<f32> {
    (0..FRAMES)
        .map(|i| amp * ((start + i) as f32 * hz * std::f32::consts::TAU / RATE).sin())
        .collect()
}

fn run(dsp: &mut dyn AudioDsp, input: &[f32]) -> Vec<f32> {
    let mut l = input.to_vec();
    let mut r = input.to_vec();
    dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
    assert_eq!(l, r, "channels are processed alike");
    l
}

fn gate(pairs: &[(&'static str, f32)]) -> GateDsp {
    GateDsp::new(setup(), &gate_values(pairs))
}

/// Blocks of constant level `amp`.
fn feed(g: &mut GateDsp, amp: f32, blocks: usize) {
    for _ in 0..blocks {
        run(g, &vec![amp; FRAMES]);
    }
}

#[test]
fn the_catalogue_passes_conformance() {
    for ty in catalogue() {
        assert_eq!(
            conformance::check_type(*ty),
            Vec::<String>::new(),
            "{}",
            ty.name()
        );
    }
}

#[test]
fn gain_at_zero_db_is_bit_exact() {
    let mut d = Gain.make(setup(), &gain_values(0.0)).expect("valid");
    let input = sine(0.7, 440.0, 0);
    assert_eq!(run(d.as_mut(), &input), input);
}

#[test]
fn gain_follows_its_value_with_a_ten_ms_ramp_and_no_step() {
    let mut d = Gain.make(setup(), &gain_values(0.0)).expect("valid");
    d.set(&gain_values(6.0));
    let target = 10.0f32.powf(6.0 / 20.0);
    let out = run(d.as_mut(), &vec![1.0; FRAMES]);
    // 10 ms is 480 samples: the block ends at the target and rises monotonically.
    assert!(
        (out[FRAMES - 1] - target).abs() < 1e-4,
        "{}",
        out[FRAMES - 1]
    );
    let step = (target - 1.0) / FRAMES as f32;
    let mut prev = 1.0;
    for &y in &out {
        assert!(
            y >= prev && y - prev <= step * 1.01 + 1e-6,
            "step {}",
            y - prev
        );
        prev = y;
    }
    // Afterwards it stays on the target.
    assert!((run(d.as_mut(), &vec![1.0; FRAMES])[0] - target).abs() < 1e-5);
}

#[test]
fn gain_reverses_from_where_it_is() {
    let mut d = Gain.make(setup(), &gain_values(0.0)).expect("valid");
    d.set(&gain_values(6.0));
    // A shorter block is a legal use: `frames` may be less than the setup's.
    let mut l = vec![1.0f32; FRAMES];
    let mut r = vec![1.0f32; FRAMES];
    d.process(&mut [&mut l[..], &mut r[..]], FRAMES / 2);
    assert!(
        l[FRAMES / 2 - 1] > 1.2 && l[FRAMES / 2 - 1] < 1.9,
        "mid-ramp"
    );
    d.set(&gain_values(-6.0));
    let out = run(d.as_mut(), &vec![1.0; FRAMES]);
    for w in out.windows(2) {
        assert!((w[1] - w[0]).abs() < 0.01, "jump {}", w[1] - w[0]);
    }
}

#[test]
fn the_gate_starts_closed_and_attenuates_quiet_input() {
    let mut g = gate(&[]);
    let out = run(&mut g, &vec![0.001; FRAMES]); // -60 dB, under the open threshold
    assert!(!g.is_open());
    assert!(
        out.iter().all(|y| y.abs() < 0.001 * 0.001),
        "closed gate at -80 dB"
    );
}

#[test]
fn the_gate_opens_at_the_open_threshold_and_reaches_unity_after_the_attack() {
    let mut g = gate(&[("attack", 5.0)]);
    feed(&mut g, 0.1, 1); // -20 dB, above -40
    assert!(g.is_open());
    // 5 ms attack from -80 dB: done within one 10 ms block.
    assert_eq!(g.gain_db(), 0.0);
}

#[test]
fn the_gate_holds_then_closes_after_the_release() {
    let mut g = gate(&[("hold", 100.0), ("release", 150.0)]);
    feed(&mut g, 0.1, 2);
    assert!(g.is_open());
    // The 40 ms detector keeps the level above the close threshold for a while, then
    // hold (100 ms) runs; the gate must still be open 60 ms after the input stops.
    feed(&mut g, 0.0, 6);
    assert!(g.is_open(), "open during hold");
    feed(&mut g, 0.0, 30);
    assert!(!g.is_open(), "closed after hold");
    // 150 ms release from 0 dB to -80 dB: fully down well within 500 ms.
    feed(&mut g, 0.0, 20);
    assert_eq!(g.gain_db(), -80.0);
}

#[test]
fn a_level_between_the_thresholds_keeps_the_gate_open_and_resets_hold() {
    // open -40 dB (0.01), close -46 dB (0.005); 0.007 is between.
    let mut g = gate(&[("hold", 50.0)]);
    feed(&mut g, 0.1, 2);
    assert!(g.is_open());
    feed(&mut g, 0.007, 100); // one second between the thresholds
    assert!(g.is_open());
    // The hold timer was reset all along: it runs only once the level drops below.
    feed(&mut g, 0.0, 6);
    assert!(g.is_open(), "hold starts after the level leaves the band");
}

#[test]
fn a_closed_gate_does_not_open_between_the_thresholds() {
    let mut g = gate(&[]);
    feed(&mut g, 0.007, 20);
    assert!(!g.is_open());
}

#[test]
fn a_close_threshold_above_the_open_threshold_is_clamped() {
    let mut g = gate(&[
        ("open_threshold", -40.0),
        ("close_threshold", -10.0),
        ("hold", 0.0),
    ]);
    feed(&mut g, 0.02, 2); // -34 dB: above open, below the unclamped close
    assert!(g.is_open(), "clamped to the open threshold");
}

#[test]
fn hold_zero_does_not_chatter_on_low_tones_above_the_threshold() {
    for hz in [50.0f32, 100.0] {
        let mut g = gate(&[("hold", 0.0)]);
        let mut starts = 0;
        let mut was_open = false;
        for b in 0..60 {
            run(&mut g, &sine(0.1, hz, b * FRAMES));
            if g.is_open() && !was_open {
                starts += 1;
            }
            was_open = g.is_open();
        }
        assert!(g.is_open());
        assert_eq!(starts, 1, "opened once at {hz} Hz");
    }
}

#[test]
fn changing_release_during_a_release_does_not_jump() {
    let mut g = gate(&[("hold", 0.0), ("release", 300.0)]);
    feed(&mut g, 0.1, 2);
    feed(&mut g, 0.0, 15);
    let before = g.gain_db();
    assert!(before < 0.0 && before > -80.0, "mid-release at {before}");
    g.set(&gate_values(&[("hold", 0.0), ("release", 20.0)]));
    assert_eq!(g.gain_db(), before, "set does not move the gain");
    let out = run(&mut g, &vec![0.0; FRAMES]);
    assert!(out.iter().all(|y| *y == 0.0));
    assert!(g.gain_db() < before);
}

#[test]
fn the_gate_reverses_from_the_current_gain_when_the_level_returns() {
    let mut g = gate(&[("hold", 0.0), ("release", 300.0), ("attack", 50.0)]);
    feed(&mut g, 0.1, 2);
    feed(&mut g, 0.0, 14);
    let mid = g.gain_db();
    assert!(mid < 0.0 && mid > -80.0);
    feed(&mut g, 0.1, 1);
    assert!(g.gain_db() > mid, "rises again from {mid}");
    assert!(g.gain_db() < 0.0 || g.is_open());
}

#[test]
fn range_zero_is_a_gate_with_no_effect() {
    let mut g = gate(&[("range", 0.0)]);
    let input = vec![0.001; FRAMES];
    assert_eq!(run(&mut g, &input), input);
}

#[test]
fn the_gate_survives_nan_and_infinity() {
    let mut g = gate(&[]);
    let mut bad = vec![f32::NAN; FRAMES];
    bad[3] = f32::INFINITY;
    let out = run(&mut g, &bad);
    assert!(out.iter().all(|y| *y == 0.0));
    // The detector is not poisoned: a normal signal opens the gate afterwards.
    feed(&mut g, 0.1, 2);
    assert!(g.is_open());
}

#[test]
fn reset_closes_the_gate_and_a_parameter_edit_does_not() {
    let mut g = gate(&[]);
    feed(&mut g, 0.1, 2);
    assert!(g.is_open());
    g.set(&gate_values(&[("hold", 10.0)]));
    assert!(g.is_open(), "an edit keeps the state");
    g.reset();
    assert!(!g.is_open());
    assert_eq!(g.gain_db(), -80.0);
}

#[test]
fn the_meter_follows_the_audio() {
    let mut g = gate(&[]);
    let meter = g.meter().expect("a gate has a meter");
    feed(&mut g, 0.0, 1);
    assert_eq!(meter.input_peak_db(), -120.0);
    assert!(meter.gain_reduction_db() > 79.0);
    feed(&mut g, 0.1, 2);
    assert!((meter.input_peak_db() + 20.0).abs() < 0.1);
    assert_eq!(meter.gain_reduction_db(), 0.0);
}

#[test]
fn processing_does_not_allocate() {
    let mut g = Box::new(gate(&[]));
    let mut gain = Gain.make(setup(), &gain_values(0.0)).expect("valid");
    let mut l = vec![0.1f32; FRAMES];
    let mut r = vec![0.1f32; FRAMES];
    // Building `Values` allocates and is outside the audio path: build them first.
    let vals = gate_values(&[]);
    let gvals = gain_values(3.0);
    let info = allocation_counter::measure(|| {
        for _ in 0..20 {
            g.set(&vals);
            gain.set(&gvals);
            g.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            gain.process(&mut [&mut l[..], &mut r[..]], FRAMES);
        }
    });
    assert_eq!(info.count_total, 0);
}
