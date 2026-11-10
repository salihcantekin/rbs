//! The studio's monitor output (ADR-0097 F1; spec `recording.md`, "Monitor output"):
//! nothing of it exists until the host starts it, it plays the mixer's monitor pin into
//! a device output, a device that cannot start is reported without stopping the studio,
//! and stopping it hands the device back. Reference adapters on a `TestClock`; tests
//! wait for counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::collections::BTreeSet;

use rbs_audio_io::{AudioBackend, EndpointId, OutputRequest};
use rbs_audio_io_test::OUTPUT;
use rbs_record::{RecordError, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::model::{SourceId, SourceMonitor};
use support::*;

fn source(col: &SceneCollection, name: &str) -> SourceId {
    col.sources().find(|s| s.name == name).unwrap().id
}

fn monitor_frames(studio: &Studio) -> u64 {
    studio.monitor().map_or(0, |m| m.stats.read().frames_out)
}

#[test]
fn the_monitor_plays_while_a_source_is_monitored_and_hands_the_device_back() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = source(&col, "Microphone");
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

    // Nothing is monitored: the host has no reason to open a device.
    assert!(!studio.monitor_wanted());
    assert!(studio.monitor().is_none());
    assert!(!studio.stop_monitor());
    assert!(!studio.set_monitor_volume(0.5));

    col.set_source_monitor(mic, SourceMonitor::Only).unwrap();
    assert!(studio.set_mix(&col));
    assert!(studio.monitor_wanted());

    let streams = r.audio.active_streams();
    let output = r
        .audio
        .open_output(&OutputRequest::new(EndpointId::new(OUTPUT)))
        .unwrap();
    studio.start_monitor(output, 0.8).unwrap();
    let second = r
        .audio
        .open_output(&OutputRequest::new(EndpointId::new(OUTPUT)))
        .unwrap();
    assert!(matches!(
        studio.start_monitor(second, 1.0),
        Err(RecordError::MonitorPlaying)
    ));
    let state = studio.monitor().unwrap();
    assert_eq!(state.failure, None);
    assert!(state.latency > StudioSettings::new(canvas).audio_graph.latency);

    // The device plays what the mixer's monitor pin gives it.
    let audio = std::sync::Arc::clone(&r.audio);
    assert!(run_until(&mut studio, &clock, |s| {
        audio.pump_outputs(240);
        monitor_frames(s) >= 4_800
    }));
    assert_eq!(r.audio.active_streams(), streams + 1);
    assert!(studio.set_monitor_volume(0.2));

    // Monitoring ends: the node leaves, the device is stopped and dropped here.
    col.set_source_monitor(mic, SourceMonitor::Off).unwrap();
    studio.set_mix(&col);
    assert!(!studio.monitor_wanted());
    assert!(studio.stop_monitor());
    assert!(studio.monitor().is_none());
    assert!(run_until(&mut studio, &clock, |_| audio.active_streams() == streams));

    // Solo alone also asks for the monitor (session state, not saved).
    assert!(studio.set_solo(BTreeSet::from([mic])));
    assert!(studio.monitor_wanted());
    assert_eq!(studio.solo(), &BTreeSet::from([mic]));
    assert_eq!(col.source(mic).unwrap().audio.monitor, SourceMonitor::Off);
    studio.set_solo(BTreeSet::new());
    assert!(!studio.monitor_wanted());
    studio.shutdown();
}

#[test]
fn a_monitor_device_that_cannot_start_is_reported_and_the_studio_runs_on() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = source(&col, "Microphone");
    col.set_source_monitor(mic, SourceMonitor::Only).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert!(studio.monitor_wanted());

    let endpoint = EndpointId::new(OUTPUT);
    let output = r
        .audio
        .open_output(&OutputRequest::new(endpoint.clone()))
        .unwrap();
    assert!(r.audio.remove(&endpoint));
    studio.start_monitor(output, 1.0).unwrap();
    let failure = studio.monitor().unwrap().failure;
    assert!(failure.is_some(), "{failure:?}");
    // The studio runs on (run_until checks it has no failure); the host stops the
    // monitor and opens the device again later.
    assert!(run_until(&mut studio, &clock, |s| s
        .sources()
        .iter()
        .all(|x| x.failed.is_none())));
    assert!(studio.stop_monitor());
    studio.shutdown();
}

#[test]
fn a_studio_without_audio_has_no_monitor() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = source(&col, "Microphone");
    col.set_source_monitor(mic, SourceMonitor::Also).unwrap();
    let (engine, _clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    assert!(!studio.monitor_wanted());
    let output = r
        .audio
        .open_output(&OutputRequest::new(EndpointId::new(OUTPUT)))
        .unwrap();
    assert!(matches!(
        studio.start_monitor(output, 1.0),
        Err(RecordError::NotBound("mixer"))
    ));
    assert!(!studio.set_solo(BTreeSet::from([mic])));
    studio.shutdown();
}
