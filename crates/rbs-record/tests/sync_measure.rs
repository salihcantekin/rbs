//! A sync offset measurement on a running studio (ADR-0126; spec `recording.md`,
//! "Sync offset measurement"): nothing of it exists until it starts, the probes read
//! the measured source's sound and the reference's picture, it is refused for a
//! source the mix does not hear or a reference with no picture, an offset change ends
//! it, cancelling or ending removes its probes. Reference adapters on a `TestClock`;
//! tests advance the clock, never sleep.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_core::media::ContainerId;
use rbs_record::{
    OutputSettings, RecordError, Studio, StudioSettings, SyncMeasureFailure, SyncMeasureRefused,
    SyncMeasureState, SyncMeasureStatus,
};
use rbs_scene::model::SourceId;
use support::*;

fn id_of(col: &rbs_scene::SceneCollection, name: &str) -> SourceId {
    col.sources().find(|s| s.name == name).unwrap().id
}

/// Advances the clock by `steps` service steps.
fn steps(studio: &mut Studio, clock: &rbs_core::time::TestClock, steps: usize) {
    let mut n = 0;
    assert!(run_until(studio, clock, |_| {
        n += 1;
        n > steps
    }));
}

#[test]
fn a_measurement_reads_both_probes_and_cancelling_removes_them() {
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
    // The sources draw only while something takes the canvas, as the preview does in
    // the app.
    studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        let names: Vec<String> = s.sources().into_iter().map(|x| x.name).collect();
        names.iter().any(|n| n == "Microphone") && names.iter().any(|n| n == "Camera")
    }));
    let mic = id_of(&col, "Microphone");
    let camera = id_of(&col, "Camera");
    let nodes = studio.graph_node_count();

    // Nothing runs: no status.
    assert!(studio.sync_measure().is_none());
    assert!(studio.cancel_sync_measure().is_none());

    let id = studio.start_sync_measure(mic, camera).unwrap();
    assert_eq!(studio.graph_node_count(), nodes + 2);
    assert!(matches!(
        studio.start_sync_measure(mic, camera),
        Err(RecordError::SyncMeasure(SyncMeasureRefused::Busy))
    ));
    steps(&mut studio, &clock, 60);
    let status = studio.sync_measure().unwrap();
    assert_eq!(
        (status.id, status.source, status.reference),
        (id, mic, camera)
    );
    assert!(
        matches!(status.state, SyncMeasureState::Running { .. }),
        "{status:?}"
    );
    let (energy, frames) = studio.sync_measure_collected().unwrap();
    assert!(energy >= 200, "{energy} ms of sound logged");
    assert!(frames >= 5, "{frames} pictures logged");

    let cancelled = studio.cancel_sync_measure().unwrap();
    assert_eq!(
        cancelled.state,
        SyncMeasureState::Failed(SyncMeasureFailure::Cancelled)
    );
    assert_eq!(studio.graph_node_count(), nodes);
    assert!(studio.sync_measure().is_none());
    // A new one gets a new id.
    let next = studio.start_sync_measure(mic, camera).unwrap();
    assert_ne!(next, id);
    studio.shutdown();
}

#[test]
fn a_source_the_mix_does_not_hear_or_a_reference_without_picture_is_refused() {
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
    // The sources draw only while something takes the canvas, as the preview does in
    // the app.
    studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        let names: Vec<String> = s.sources().into_iter().map(|x| x.name).collect();
        names.iter().any(|n| n == "Microphone") && names.iter().any(|n| n == "Camera")
    }));
    let nodes = studio.graph_node_count();
    let mic = id_of(&col, "Microphone");
    let camera = id_of(&col, "Camera");
    let system = id_of(&col, "System audio");
    assert!(matches!(
        studio.start_sync_measure(camera, camera),
        Err(RecordError::SyncMeasure(SyncMeasureRefused::NotHeard))
    ));
    assert!(matches!(
        studio.start_sync_measure(mic, system),
        Err(RecordError::SyncMeasure(SyncMeasureRefused::NoPicture))
    ));
    assert_eq!(studio.graph_node_count(), nodes);
    studio.shutdown();
}

#[test]
fn an_offset_change_ends_the_measurement() {
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
    // The sources draw only while something takes the canvas, as the preview does in
    // the app.
    studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        let names: Vec<String> = s.sources().into_iter().map(|x| x.name).collect();
        names.iter().any(|n| n == "Microphone") && names.iter().any(|n| n == "Camera")
    }));
    let nodes = studio.graph_node_count();
    let mic = id_of(&col, "Microphone");
    let camera = id_of(&col, "Camera");
    studio.start_sync_measure(mic, camera).unwrap();
    steps(&mut studio, &clock, 10);
    // Another source's offset does not matter.
    studio.set_audio_offset(id_of(&col, "System audio"), 20_000_000);
    assert!(matches!(
        studio.sync_measure().unwrap().state,
        SyncMeasureState::Running { .. }
    ));
    studio.set_audio_offset(mic, 40_000_000);
    let end: SyncMeasureStatus = studio.sync_measure().unwrap();
    assert_eq!(
        end.state,
        SyncMeasureState::Failed(SyncMeasureFailure::OffsetChanged)
    );
    assert_eq!(studio.graph_node_count(), nodes);
    assert!(studio.sync_measure().is_none());
    studio.shutdown();
}

#[test]
#[ignore = "slow: drives 10 s of studio time (about 6 s on a software GPU)"]
fn slow_a_window_without_claps_ends_without_a_result() {
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
    // The sources draw only while something takes the canvas, as the preview does in
    // the app.
    studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        let names: Vec<String> = s.sources().into_iter().map(|x| x.name).collect();
        names.iter().any(|n| n == "Microphone") && names.iter().any(|n| n == "Camera")
    }));
    let nodes = studio.graph_node_count();
    studio
        .start_sync_measure(id_of(&col, "Microphone"), id_of(&col, "Camera"))
        .unwrap();
    // 10 s of steady tones and a test pattern: no claps.
    let end = loop {
        steps(&mut studio, &clock, 100);
        let s = studio.sync_measure().unwrap();
        if !matches!(s.state, SyncMeasureState::Running { .. }) {
            break s;
        }
    };
    assert!(
        matches!(
            end.state,
            SyncMeasureState::Failed(
                SyncMeasureFailure::TooFewClaps | SyncMeasureFailure::NoPictureMotion
            )
        ),
        "{end:?}"
    );
    assert_eq!(studio.graph_node_count(), nodes);
    studio.shutdown();
}
