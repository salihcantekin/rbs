//! The studio's loudness meter (ADR-0092 stage 1; spec `recording.md`, "Loudness
//! meter"): nothing of it exists until it is started, it measures the main canvas's
//! mix beside the meter sink and the encoders, a reset restarts the measurement, and
//! stopping it removes it. Reference adapters on a `TestClock`; tests wait for values,
//! never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_record::{RecordError, Studio, StudioSettings};
use support::*;

#[test]
fn loudness_is_measured_only_while_it_is_on() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert!(run_until(&mut studio, &clock, |s| !s.sources().is_empty()));

    // Off: no values, nothing to reset or stop.
    assert!(studio.loudness().is_none());
    assert!(!studio.reset_loudness());
    assert!(!studio.stop_loudness());

    assert!(studio.start_loudness().unwrap());
    assert!(!studio.start_loudness().unwrap());

    // The reference microphone's tone fills the momentary window, then the gating
    // blocks give an integrated value.
    assert!(run_until(&mut studio, &clock, |s| s
        .loudness()
        .is_some_and(
            |l| l.integrated.is_finite() && l.true_peak.is_finite()
        )));
    let l = studio.loudness().unwrap();
    // The reference sources are steady tones near full scale: the momentary value
    // is the programme's, and the true peak is at least the momentary level of a sine.
    assert!((l.momentary - l.integrated).abs() < 0.5, "{l:?}");
    assert!(l.true_peak > l.momentary, "{l:?}");

    // A reset clears the programme values at the next block.
    assert!(studio.reset_loudness());
    assert!(run_until(&mut studio, &clock, |s| s
        .loudness()
        .is_some_and(|l| l.integrated == f32::NEG_INFINITY)));

    assert!(studio.stop_loudness());
    assert!(studio.loudness().is_none());
    assert!(!studio.stop_loudness());
    // Started again, it measures from scratch.
    assert!(studio.start_loudness().unwrap());
    assert_eq!(studio.loudness().unwrap().integrated, f32::NEG_INFINITY);
    studio.shutdown();
}

#[test]
fn a_studio_without_audio_has_no_loudness_meter() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, _clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    assert!(matches!(
        studio.start_loudness(),
        Err(RecordError::NotBound("mixer"))
    ));
    assert!(studio.loudness().is_none());
    studio.shutdown();
}
