//! Holding a stream on a running studio (ADR-0124 decisions 12 to 14, spec
//! `recording.md`, "Pause and hold"): the connection and the timestamps run on while
//! the export sends a pinned picture; a stream sharing a recording's encoder holds
//! only while that recording is paused, and the recording does not resume while the
//! hold is on. Reference adapters on a `TestClock`; tests wait for counts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::media::ContainerId;
use rbs_core::pause::HoldPicture;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_record::{
    HOLD_SCENE_TIMEOUT, HoldFallback, HoldRefused, HoldSound, OutputSettings, PauseRefused, Studio,
    StudioSettings,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneId, SceneScope};
use support::*;

fn start(r: &Rig) -> (Studio, TestClock) {
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    (studio, clock)
}

fn stream() -> OutputSettings {
    let mut out = OutputSettings::packets("stream://live/app");
    out.holdable = true;
    out
}

/// The largest step between two neighbouring times.
fn largest_step(times: &[Timestamp]) -> u64 {
    times
        .windows(2)
        .map(|w| w[1].as_nanos().saturating_sub(w[0].as_nanos()))
        .max()
        .unwrap_or(0)
}

const FRAME: u64 = 1_000_000_000 / 30 + 1;

#[test]
fn a_held_stream_keeps_sending_with_running_timestamps() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= 10
        && r.stream_log.count(1) >= 10));
    assert_eq!(studio.release_hold(live), Err(HoldRefused::NotHeld));

    let held_at = studio.hold_output(live, HoldPicture::Freeze).unwrap();
    assert_eq!(
        studio.hold_output(live, HoldPicture::Freeze),
        Err(HoldRefused::AlreadyHeld)
    );
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.picture, Some(HoldPicture::Freeze));
    assert_eq!(state.held_at, Some(held_at));
    // Half a second frozen, then half a second black.
    let video_at_hold = r.stream_log.count(0);
    let audio_at_hold = r.stream_log.count(1);
    let black_after = held_at + Duration::from_millis(500);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= black_after));
    studio.hold_output(live, HoldPicture::Black).unwrap();
    assert_eq!(studio.output_hold(live).unwrap().held_at, Some(held_at));
    let release_after = held_at + Duration::from_secs(1);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= release_after));
    let p = studio.output_progress(live).unwrap();
    assert!(p.held, "{p:?}");
    // The stream kept sending: a frame per tick and the audio blocks.
    assert!(r.stream_log.count(0) >= video_at_hold + 25, "{p:?}");
    assert!(r.stream_log.count(1) > audio_at_hold + 25, "{p:?}");

    let released_at = studio.release_hold(live).unwrap();
    assert_eq!(studio.release_hold(live), Err(HoldRefused::NotHeld));
    let before = r.stream_log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= before + 10));
    let p = studio.output_progress(live).unwrap();
    assert!(!p.held);
    assert_eq!(p.held_ns, released_at.as_nanos() - held_at.as_nanos());
    assert_eq!(p.export_drops, 0, "{p:?}");

    // No gap anywhere: before, during and after the hold, one frame per tick.
    let video = pts_of(&r.stream_log, 0);
    let audio = pts_of(&r.stream_log, 1);
    assert!(largest_step(&video) <= FRAME, "{video:?}");
    assert!(largest_step(&audio) <= 50_000_000, "{audio:?}");
    assert!(video.windows(2).all(|w| w[0] < w[1]), "{video:?}");
    studio.shutdown();
}

#[test]
fn recordings_and_streams_not_started_holdable_are_not_held() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let plain = studio
        .start_output(&r.adapters, &OutputSettings::packets("stream://live/app"))
        .unwrap();
    assert_eq!(
        studio.hold_output(plain, HoldPicture::Freeze),
        Err(HoldRefused::NotHoldable)
    );
    assert_eq!(studio.output_hold(plain), None);
    let mut rec = OutputSettings::new("mem://rec", ContainerId::Matroska);
    rec.pausable = true;
    rec.video = OutputSettings::packets("x").video;
    let rec = studio.start_output(&r.adapters, &rec).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= 2));
    assert_eq!(
        studio.hold_output(rec, HoldPicture::Black),
        Err(HoldRefused::NotHoldable)
    );
    studio.end_output(plain);
    assert_eq!(
        studio.hold_output(plain, HoldPicture::Freeze),
        Err(HoldRefused::Ending)
    );
    studio.shutdown();
}

#[test]
fn a_stream_sharing_a_recordings_encoder_holds_only_while_it_is_paused() {
    if gpu().is_none() {
        return;
    }
    let r = rig_with_keyframes();
    let (mut studio, clock) = start(&r);
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 4));
    let mut out = OutputSettings::new("mem://rec", ContainerId::Matroska);
    out.pausable = true;
    out.video = OutputSettings::packets("x").video;
    let rec = studio.start_output(&r.adapters, &out).unwrap();
    let report = studio.output(rec).unwrap();
    assert!(report.video_shared && report.audio_shared, "{report:?}");
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= 10
        && r.log.count(1) >= 10));

    // The recording takes the stream's packets: no hold while it records.
    assert_eq!(
        studio.hold_output(live, HoldPicture::Freeze),
        Err(HoldRefused::SharedWithRecording)
    );
    // Paused first (ADR-0124 decision 14), then held.
    let pause_at = studio.pause_output(rec).unwrap();
    assert_eq!(
        studio.hold_output(live, HoldPicture::Freeze),
        Err(HoldRefused::SharedWithRecording),
        "the pause has not begun yet"
    );
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= pause_at));
    let held_at = studio.hold_output(live, HoldPicture::Black).unwrap();
    let release_after = held_at + Duration::from_millis(500);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= release_after));
    assert_eq!(studio.resume_output(rec), Err(PauseRefused::Held));
    let released_at = studio.release_hold(live).unwrap();
    let resume_at = studio.resume_output(rec).unwrap();
    let before = r.log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 10));

    // The file holds nothing of the hold: no packet between the pause and the
    // resume, and it continues without a gap.
    let video = pts_of(&r.log, 0);
    assert!(largest_step(&video) <= FRAME, "{video:?}");
    let left_out = resume_at.as_nanos() - pause_at.as_nanos();
    assert_eq!(studio.output_pause(rec).unwrap().paused_ns, left_out);
    assert!(resume_at > released_at);
    // The stream ran on through the hold.
    let stream_video = pts_of(&r.stream_log, 0);
    assert!(largest_step(&stream_video) <= FRAME, "{stream_video:?}");
    studio.shutdown();
}

// ---- Hold scene (ADR-0124 phase 2b) --------------------------------------------------

/// A studio whose collection also has an empty shared scene, "Break" (drawn black).
fn start_with_break(r: &Rig) -> (Studio, TestClock, SceneCollection, SceneId) {
    let (mut col, canvas) = scene();
    let (brk, _) = col.add_scene("Break", SceneScope::Shared).unwrap();
    let (engine, clock) = engine_at_start();
    let studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    (studio, clock, col, brk)
}

#[test]
fn a_stream_held_with_a_scene_sends_it_and_comes_back_without_a_gap() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock, _col, brk) = start_with_break(&r);
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= 10
        && r.stream_log.count(1) >= 10));
    assert_eq!(
        studio.hold_output(live, HoldPicture::Scene),
        Err(HoldRefused::NoScene)
    );
    let held_at = studio
        .hold_output_scene(live, brk, HoldSound::Silence, &r.adapters.sources)
        .unwrap();
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.picture, Some(HoldPicture::Scene));
    assert_eq!(state.scene, Some(brk));
    assert_eq!(state.fallback, None);
    // Past the fallback deadline: the scene's pictures arrived, so no fallback.
    let after = held_at + HOLD_SCENE_TIMEOUT + Duration::from_millis(500);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= after));
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.fallback, None, "{state:?}");
    assert_eq!(state.picture, Some(HoldPicture::Scene));

    studio
        .release_hold_scene(live, &r.adapters.sources)
        .unwrap();
    assert_eq!(studio.output_hold(live).unwrap().picture, None);
    let before = r.stream_log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= before + 10));
    let video = pts_of(&r.stream_log, 0);
    let audio = pts_of(&r.stream_log, 1);
    assert!(largest_step(&video) <= FRAME, "{video:?}");
    assert!(video.windows(2).all(|w| w[0] < w[1]), "{video:?}");
    assert!(largest_step(&audio) <= 50_000_000, "{audio:?}");
    assert_eq!(studio.output_progress(live).unwrap().export_drops, 0);
    studio.shutdown();
}

#[test]
fn a_hold_scene_that_leaves_the_collection_falls_back_to_the_frozen_frame() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock, mut col, brk) = start_with_break(&r);
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 4));
    assert_eq!(
        studio.hold_output_scene(
            live,
            SceneId::from_raw(9_999),
            HoldSound::Silence,
            &r.adapters.sources
        ),
        Err(HoldRefused::UnknownScene)
    );
    studio
        .hold_output_scene(live, brk, HoldSound::Silence, &r.adapters.sources)
        .unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 10));
    col.remove_scene(brk).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.fallback, Some(HoldFallback::SceneGone));
    assert_eq!(state.picture, Some(HoldPicture::Freeze));
    let before = r.stream_log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= before + 10));
    studio
        .release_hold_scene(live, &r.adapters.sources)
        .unwrap();
    let before = r.stream_log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0)
        >= before + 10));
    let video = pts_of(&r.stream_log, 0);
    assert!(largest_step(&video) <= FRAME, "{video:?}");
    assert!(video.windows(2).all(|w| w[0] < w[1]), "{video:?}");
    studio.shutdown();
}

#[test]
fn a_stream_held_with_a_scenes_sound_hears_its_sources_and_runs_on() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut col, canvas) = scene();
    let (brk, _) = col.add_scene("Break", SceneScope::Shared).unwrap();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    col.add_item(
        brk,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(1) >= 10));
    let held_at = studio
        .hold_output_scene(live, brk, HoldSound::Scene, &r.adapters.sources)
        .unwrap();
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.sound, Some(HoldSound::Scene));
    assert_eq!(state.sound_fallback, None);
    let audio_at_hold = r.stream_log.count(1);
    let after = held_at + HOLD_SCENE_TIMEOUT + Duration::from_millis(500);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= after));
    let state = studio.output_hold(live).unwrap();
    assert_eq!(
        (state.fallback, state.sound_fallback),
        (None, None),
        "{state:?}"
    );
    // The hold mixer heard the microphone through a strip: nothing unknown.
    assert_eq!(studio.mixer_anomalies(), 0);
    assert!(r.stream_log.count(1) > audio_at_hold + 25);
    // A volume change reaches the hold mixer too, without a rebuild.
    col.set_source_volume(mic, 0.5).unwrap();
    assert!(studio.set_mix(&col));

    studio
        .release_hold_scene(live, &r.adapters.sources)
        .unwrap();
    let before = r.stream_log.count(1);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(1)
        >= before + 10));
    assert_eq!(studio.mixer_anomalies(), 0);
    let audio = pts_of(&r.stream_log, 1);
    assert!(largest_step(&audio) <= 50_000_000, "{audio:?}");
    assert!(audio.windows(2).all(|w| w[0] < w[1]), "{audio:?}");
    studio.shutdown();
}

#[test]
fn a_stream_sharing_a_recordings_chain_sends_the_scenes_sound_while_the_recording_pauses() {
    if gpu().is_none() {
        return;
    }
    let r = rig_with_keyframes();
    let (mut col, canvas) = scene();
    let (brk, _) = col.add_scene("Break", SceneScope::Shared).unwrap();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    col.add_item(
        brk,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    // The recording builds the chains (`PauseGate` with a hold); the stream joins.
    let mut out = OutputSettings::new("mem://rec", ContainerId::Matroska);
    out.pausable = true;
    out.video = OutputSettings::packets("x").video;
    let rec = studio.start_output(&r.adapters, &out).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.log.count(1) >= 4));
    let live = studio.start_output(&r.adapters, &stream()).unwrap();
    let report = studio.output(live).unwrap();
    assert!(report.video_shared && report.audio_shared, "{report:?}");
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(1) >= 10));

    let pause_at = studio.pause_output(rec).unwrap();
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= pause_at));
    let held_at = studio
        .hold_output_scene(live, brk, HoldSound::Scene, &r.adapters.sources)
        .unwrap();
    let audio_at_hold = r.stream_log.count(1);
    let after = held_at + HOLD_SCENE_TIMEOUT + Duration::from_millis(500);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= after));
    let state = studio.output_hold(live).unwrap();
    assert_eq!(state.sound, Some(HoldSound::Scene));
    assert_eq!(
        (state.fallback, state.sound_fallback),
        (None, None),
        "{state:?}"
    );
    assert_eq!(studio.mixer_anomalies(), 0);
    assert!(r.stream_log.count(1) > audio_at_hold + 25);

    studio
        .release_hold_scene(live, &r.adapters.sources)
        .unwrap();
    studio.resume_output(rec).unwrap();
    let before = r.stream_log.count(1);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(1)
        >= before + 10));
    let audio = pts_of(&r.stream_log, 1);
    assert!(largest_step(&audio) <= 50_000_000, "{audio:?}");
    assert!(audio.windows(2).all(|w| w[0] < w[1]), "{audio:?}");
    studio.shutdown();
}
