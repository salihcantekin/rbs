//! Ducker behaviour, driven by blocks (no sleeping, no clock). ADR-0095.

#![allow(clippy::expect_used)]

use rbs_audio_filters::ducker::{Ducker, DuckerDsp};
use rbs_audio_filters::{AudioDsp, AudioFilterType, DspSetup, conformance};
use rbs_core::media::AudioFormat;
use rbs_params::{Value, Values};

const FRAMES: usize = 480;

fn setup() -> DspSetup {
    DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32)
}

fn values(pairs: &[(&'static str, f32)]) -> Values {
    let ty: &dyn AudioFilterType = &Ducker;
    Values::from_pairs(ty, pairs.iter().map(|&(k, v)| (k, Value::Number(v)))).expect("valid")
}

fn ducker(pairs: &[(&'static str, f32)]) -> DuckerDsp {
    DuckerDsp::new(setup(), &values(pairs))
}

/// One block: music of constant `music` under a key of constant `key`; the music
/// channel is returned. `None` means no key block.
fn block(d: &mut DuckerDsp, music: f32, key: Option<f32>) -> Vec<f32> {
    let mut l = vec![music; FRAMES];
    let mut r = vec![music; FRAMES];
    match key {
        Some(k) => {
            let kl = vec![k; FRAMES];
            let kr = vec![k; FRAMES];
            d.process_keyed(
                &mut [&mut l[..], &mut r[..]],
                Some(&[&kl[..], &kr[..]]),
                FRAMES,
            );
        }
        None => d.process_keyed(&mut [&mut l[..], &mut r[..]], None, FRAMES),
    }
    l
}

fn feed(d: &mut DuckerDsp, key: Option<f32>, blocks: usize) {
    for _ in 0..blocks {
        block(d, 0.5, key);
    }
}

#[test]
fn the_type_passes_conformance_and_has_a_key_input() {
    assert_eq!(conformance::check_type(&Ducker), Vec::<String>::new());
    assert!(Ducker.key_input());
    let d = rbs_audio_filters::descriptor(&Ducker);
    assert_eq!(d.inputs.len(), 2);
    assert_eq!(d.inputs[1].name, "key");
}

#[test]
fn without_a_loud_key_the_music_is_untouched_bit_exact() {
    let mut d = ducker(&[]);
    assert_eq!(block(&mut d, 0.25, None), vec![0.25; FRAMES]);
    assert_eq!(block(&mut d, 0.25, Some(0.001)), vec![0.25; FRAMES]);
    assert_eq!(d.gain_db(), 0.0);
}

#[test]
fn a_loud_key_lowers_the_music_by_the_amount_after_the_attack() {
    let mut d = ducker(&[("amount", 12.0), ("attack", 20.0)]);
    // 20 ms attack: two blocks of 10 ms are enough.
    feed(&mut d, Some(0.5), 4);
    assert!(d.is_ducking());
    assert!((d.gain_db() + 12.0).abs() < 0.01, "{}", d.gain_db());
    let out = block(&mut d, 0.5, Some(0.5));
    let expected = 0.5 * 10f32.powf(-12.0 / 20.0);
    assert!((out[FRAMES - 1] - expected).abs() < 1e-4);
}

#[test]
fn the_duck_ramps_without_a_step() {
    let mut d = ducker(&[("amount", 12.0), ("attack", 20.0)]);
    let out = block(&mut d, 1.0, Some(0.5));
    let max_step = out
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(max_step < 0.01, "{max_step}");
}

#[test]
fn hold_then_release_bring_the_music_back() {
    let mut d = ducker(&[("hold", 100.0), ("release", 100.0), ("attack", 5.0)]);
    feed(&mut d, Some(0.5), 5);
    assert!(d.gain_db() < -11.9);
    // Hold keeps the duck for 100 ms (10 blocks) after the key falls quiet; the
    // detector needs a few ms to fall below the threshold first.
    feed(&mut d, Some(0.0), 5);
    assert!(d.gain_db() < -11.9, "still held: {}", d.gain_db());
    feed(&mut d, Some(0.0), 30);
    assert_eq!(d.gain_db(), 0.0);
    assert!(!d.is_ducking());
}

#[test]
fn losing_the_key_releases_at_the_release_rate_without_a_jump() {
    let mut d = ducker(&[
        ("amount", 12.0),
        ("hold", 0.0),
        ("release", 500.0),
        ("attack", 5.0),
    ]);
    feed(&mut d, Some(0.5), 5);
    assert!(d.gain_db() < -11.9);
    // The key disappears (None = silence): a few blocks later the gain is rising
    // smoothly, not at 0 dB at once.
    feed(&mut d, None, 5);
    assert!(d.gain_db() < 0.0 && d.gain_db() > -12.0, "{}", d.gain_db());
    let out = block(&mut d, 1.0, None);
    let max_step = out
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(max_step < 0.01, "{max_step}");
    feed(&mut d, None, 100);
    assert_eq!(d.gain_db(), 0.0);
    assert_eq!(block(&mut d, 0.25, None), vec![0.25; FRAMES]);
}

#[test]
fn a_key_below_the_threshold_does_not_duck() {
    let mut d = ducker(&[("threshold", -30.0)]);
    // -40 dB is about 0.01.
    feed(&mut d, Some(0.01), 10);
    assert_eq!(d.gain_db(), 0.0);
}

#[test]
fn an_amount_of_zero_changes_nothing_and_a_lowered_gain_still_returns() {
    let mut d = ducker(&[
        ("amount", 12.0),
        ("attack", 5.0),
        ("hold", 0.0),
        ("release", 50.0),
    ]);
    feed(&mut d, Some(0.5), 5);
    assert!(d.gain_db() < -11.9);
    d.set(&values(&[
        ("amount", 0.0),
        ("hold", 0.0),
        ("release", 50.0),
    ]));
    // With an amount of 0 the ramp is 1 dB per release time: 12 dB take 600 ms.
    feed(&mut d, Some(0.5), 80);
    assert_eq!(d.gain_db(), 0.0);
}

#[test]
fn nan_and_infinity_in_the_key_or_the_music_are_harmless() {
    let mut d = ducker(&[]);
    let mut l = vec![f32::NAN; FRAMES];
    let mut r = vec![f32::INFINITY; FRAMES];
    let k = vec![f32::NAN; FRAMES];
    d.process_keyed(
        &mut [&mut l[..], &mut r[..]],
        Some(&[&k[..], &k[..]]),
        FRAMES,
    );
    assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    assert_eq!(d.gain_db(), 0.0);
}

#[test]
fn the_meter_shows_the_key_and_the_reduction() {
    let mut d = ducker(&[("attack", 5.0)]);
    let meter = d.meter().expect("a ducker has a meter");
    feed(&mut d, Some(0.0), 1);
    assert_eq!(meter.input_peak_db(), -120.0);
    assert_eq!(meter.gain_reduction_db(), 0.0);
    feed(&mut d, Some(0.1), 4);
    assert!((meter.input_peak_db() + 20.0).abs() < 0.1);
    assert!(meter.gain_reduction_db() > 11.9);
}

#[test]
fn reset_stops_ducking_and_a_value_edit_does_not() {
    let mut d = ducker(&[("attack", 5.0)]);
    feed(&mut d, Some(0.5), 5);
    d.set(&values(&[("attack", 5.0), ("amount", 12.0)]));
    assert!(d.gain_db() < -11.9);
    d.reset();
    assert_eq!(d.gain_db(), 0.0);
    assert!(!d.is_ducking());
}

#[test]
fn processing_does_not_allocate() {
    let mut d = Box::new(ducker(&[]));
    let mut l = vec![0.1f32; FRAMES];
    let mut r = vec![0.1f32; FRAMES];
    let kl = vec![0.5f32; FRAMES];
    let kr = vec![0.5f32; FRAMES];
    let vals = values(&[]);
    let info = allocation_counter::measure(|| {
        for i in 0..20 {
            d.set(&vals);
            let key: Option<&[&[f32]]> = if i % 5 == 4 {
                None
            } else {
                Some(&[&kl[..], &kr[..]])
            };
            d.process_keyed(&mut [&mut l[..], &mut r[..]], key, FRAMES);
        }
    });
    assert_eq!(info.count_total, 0);
}
