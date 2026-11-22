//! Mixer meters only while something reads them (F-15 of the 2026-11 resource audit;
//! spec `audio-graph.md`, "Mixer"): with meters off the mixer records nothing, and
//! putting them back does not show a peak kept from before. Reference adapters on a
//! `TestClock`; tests wait for block counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::ContainerId;
use rbs_graph::node::NodeTypeId;
use rbs_record::{AUDIO, AUDIO_ENCODER, OUTPUT, OutputSettings, Studio, StudioSettings};
use rbs_scene::model::Settings;
use rbs_sources::types;
use support::*;

/// A studio whose audio branch rests from the start (ADR-0169).
fn resting_studio(
    r: &Rig,
    col: &rbs_scene::SceneCollection,
    canvas: rbs_scene::model::CanvasId,
) -> (Studio, rbs_core::time::TestClock) {
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio_awake = false;
    let studio =
        Studio::start(engine, gpu().unwrap(), col, &r.adapters.sources, &settings).unwrap();
    (studio, clock)
}

fn resting(studio: &Studio, r: &Rig) -> bool {
    !studio.engine().has_worker(AUDIO) && r.audio.active_streams() == 0
}

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

#[test]
fn the_audio_branch_rests_while_nothing_reads_it_and_wakes_without_a_rebuild() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut col, canvas) = scene();
    let (mut studio, clock) = resting_studio(&r, &col, canvas);
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;

    // Resting: no audio worker, no stream started, but the sources are open and
    // reported as usual, with their meters (ADR-0169 decisions 1 and 5).
    assert!(!studio.audio_awake());
    assert!(resting(&studio, &r));
    let names: Vec<String> = studio.sources().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["Microphone", "System audio"]);
    assert!(studio.sources().iter().all(|s| s.failed.is_none()));
    let meter = std::sync::Arc::clone(studio.meter(mic).unwrap());

    // A source added while resting is opened and metered, and still rests.
    let (added, _) = col
        .add_source(
            "Second mic",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    let scene = col.canvas(canvas).unwrap().program.unwrap();
    col.add_item(
        scene,
        rbs_scene::model::ItemContent::Source(added),
        rbs_scene::model::Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.meter(added).is_some(), "metered while resting");
    assert!(resting(&studio, &r), "a scene edit keeps it resting");

    // Awake: the worker and the streams come back, the kept meter moves.
    assert!(studio.set_audio_awake(true).is_empty());
    assert!(studio.engine().has_worker(AUDIO));
    assert_eq!(r.audio.active_streams(), 3);
    assert!(
        std::sync::Arc::ptr_eq(&meter, studio.meter(mic).unwrap()),
        "no rebuild"
    );
    let _ = meter.take();
    assert!(run_until(&mut studio, &clock, |_| meter.take().peak(0) > 0.0));

    // Resting again; a repeat applies nothing.
    assert!(studio.set_audio_awake(false).is_empty());
    assert!(resting(&studio, &r));
    assert!(studio.set_audio_awake(false).is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_reader_wakes_a_resting_branch_and_it_rests_again_when_the_reader_leaves() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let log = &r.log;
    let (col, canvas) = scene();
    let (mut studio, clock) = resting_studio(&r, &col, canvas);
    assert!(resting(&studio, &r));

    // Loudness, a trial graph of its own, wakes it while it runs.
    assert!(studio.start_loudness().unwrap());
    assert!(studio.engine().has_worker(AUDIO));
    assert_eq!(r.audio.active_streams(), 2);
    assert!(studio.stop_loudness());
    assert!(resting(&studio, &r), "rests again without the tap");

    // A recording started while resting records the sources' audio; its stop drains
    // the audio partition in pipeline order and the branch rests again.
    let out = OutputSettings::new("mem://rest", ContainerId::Matroska);
    let id = studio.start_output(&r.adapters, &out).unwrap();
    assert!(studio.engine().has_worker(AUDIO));
    assert!(studio.engine().has_worker(AUDIO_ENCODER));
    assert!(
        run_until(&mut studio, &clock, |_| log.count(1) >= 12),
        "audio packets"
    );
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    assert_eq!(log.trailers.load(Ordering::Relaxed), 1);
    assert!(resting(&studio, &r), "the last reader left");
    assert!(!studio.engine().has_worker(OUTPUT));
    assert_eq!(studio.mixer_anomalies(), 0);

    // Awake meanwhile, a recording's stop leaves it running.
    studio.set_audio_awake(true);
    let id = studio.start_output(&r.adapters, &out).unwrap();
    assert!(studio.stop_output(id).is_some());
    assert!(studio.engine().has_worker(AUDIO));
    assert_eq!(r.audio.active_streams(), 2);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
