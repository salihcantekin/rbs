//! Mixer meters only while something reads them (F-15 of the 2026-11 resource audit;
//! spec `audio-graph.md`, "Mixer"): with meters off the mixer records nothing, and
//! putting them back does not show a peak kept from before. Reference adapters on a
//! `TestClock`; tests wait for block counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_record::{AUDIO, Studio, StudioSettings};
use support::*;

#[test]
fn meters_off_record_nothing_and_come_back_without_a_stale_peak() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let wait = |s: &mut Studio, n| {
        let t = blocks(s);
        assert!(run_until(s, &clock, |s| blocks(s) >= t + n));
    };
    // Both banks of the strip and the master: (UI peak, control API peak).
    let read = |s: &Studio| {
        let strip = s.meter(mic).unwrap();
        let master = s.master_meter().unwrap();
        [
            strip.take().peak(0),
            strip.observe().peak(0),
            master.take().peak(0),
            master.observe().peak(0),
        ]
    };

    // On by default: the microphone is heard on its strip and on the master.
    wait(&mut studio, 10);
    let heard = read(&studio);
    assert!(heard.iter().all(|&p| p > 0.0), "{heard:?}");

    // Off: once the mixer has the parameters, nothing is recorded in either bank.
    studio.set_meters_on(false);
    wait(&mut studio, 5);
    let _ = read(&studio);
    wait(&mut studio, 10);
    assert_eq!(read(&studio), [0.0; 4], "meters off record nothing");

    // A peak recorded but never read, then off: putting them back clears it. The
    // microphone and the master are muted meanwhile, so any peak after that would be
    // the stale one.
    studio.set_meters_on(true);
    wait(&mut studio, 10);
    studio.set_meters_on(false);
    col.set_source_muted(mic, true).unwrap();
    col.set_canvas_master_muted(canvas, true).unwrap();
    assert!(studio.set_mix(&col));
    wait(&mut studio, 10);
    studio.set_meters_on(true);
    wait(&mut studio, 10);
    assert_eq!(
        read(&studio),
        [0.0; 4],
        "no peak from before the meters came off"
    );

    // A repeat changes nothing, and unmuting is heard again.
    studio.set_meters_on(true);
    col.set_source_muted(mic, false).unwrap();
    col.set_canvas_master_muted(canvas, false).unwrap();
    assert!(studio.set_mix(&col));
    wait(&mut studio, 10);
    let heard = read(&studio);
    assert!(heard.iter().all(|&p| p > 0.0), "{heard:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
