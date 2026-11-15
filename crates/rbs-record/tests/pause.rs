//! Pausing a recording on a running studio (ADR-0124, spec `recording.md`, "Pause and
//! hold"): a pausable recording with its own encoders leaves out what lies in a pause
//! and continues its file without a gap; streams and shared encoders are refused.
//! Reference adapters on a `TestClock`; tests wait for counts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_core::media::ContainerId;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_record::{OutputSettings, PauseRefused, Studio, StudioSettings};
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

fn recording() -> OutputSettings {
    let mut out = OutputSettings::new("mem://rec", ContainerId::Matroska);
    out.pausable = true;
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

#[test]
fn a_paused_recording_continues_its_file_without_a_gap() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= 10
        && r.log.count(1) >= 10));

    let asked = clock.now();
    let pause_at = studio.pause_output(rec).unwrap();
    assert!(pause_at > asked, "the pause begins ahead of the request");
    assert_eq!(studio.pause_output(rec), Err(PauseRefused::AlreadyPaused));
    let state = studio.output_pause(rec).unwrap();
    assert!(state.paused);
    // Paused for a second of master time.
    let resume_after = pause_at + std::time::Duration::from_secs(1);
    assert!(run_until(&mut studio, &clock, |_| clock.now() >= resume_after));
    let video_while_paused = r.log.count(0);
    let resume_at = studio.resume_output(rec).unwrap();
    assert_eq!(studio.resume_output(rec), Err(PauseRefused::NotPaused));
    let before = r.log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 10));
    let p = studio.output_progress(rec).unwrap();
    assert!(!p.paused);
    let left_out = resume_at.as_nanos() - pause_at.as_nanos();
    assert_eq!(p.paused_ns, left_out, "{p:?}");
    assert!(video_while_paused > 0);

    assert!(studio.end_output(rec));
    assert!(run_until(&mut studio, &clock, |s| s.output_tail_done(rec)));
    let report = studio.stop_output(rec).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");

    // Video and audio run on without a gap: no step is longer than a frame or an
    // audio packet's few milliseconds of slack, though a second was left out.
    let video = pts_of(&r.log, 0);
    let audio = pts_of(&r.log, 1);
    let frame = 1_000_000_000 / 30 + 1;
    assert!(largest_step(&video) <= frame, "{video:?}");
    assert!(largest_step(&audio) <= 50_000_000, "{audio:?}");
    // And the two end together: audio within one frame of video.
    let (v, a) = (video.last().unwrap(), audio.last().unwrap());
    // The second left out is not in the file: its last frame is a second earlier than
    // the master clock's end.
    assert!(
        v.as_nanos() + left_out <= clock.now().as_nanos(),
        "{v:?} {left_out} {:?}",
        clock.now()
    );
    assert!(
        v.as_nanos().abs_diff(a.as_nanos()) <= 2 * frame,
        "{v:?} {a:?}"
    );
}

#[test]
fn a_pause_cancelled_before_it_began_leaves_nothing_out() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= 4));
    studio.pause_output(rec).unwrap();
    studio.resume_output(rec).unwrap();
    let state = studio.output_pause(rec).unwrap();
    assert!(!state.paused && state.paused_ns == 0, "{state:?}");
    let before = r.log.count(0);
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 10));
    let video = pts_of(&r.log, 0);
    assert!(largest_step(&video) <= 1_000_000_000 / 30 + 1, "{video:?}");
    studio.shutdown();
}

#[test]
fn streams_shared_encoders_and_ended_outputs_are_not_paused() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let live = studio
        .start_output(&r.adapters, &OutputSettings::packets("stream://live/app"))
        .unwrap();
    assert_eq!(studio.pause_output(live), Err(PauseRefused::NotPausable));
    assert_eq!(studio.output_pause(live), None);
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 4));

    // A pausable recording that joins the stream's encoders cannot pause yet.
    let mut out = recording();
    out.video = OutputSettings::packets("x").video;
    let rec = studio.start_output(&r.adapters, &out).unwrap();
    assert!(studio.output(rec).unwrap().video_shared);
    assert_eq!(studio.pause_output(rec), Err(PauseRefused::SharedEncoder));
    studio.end_output(rec);
    assert_eq!(studio.pause_output(rec), Err(PauseRefused::Ending));
    studio.shutdown();
}

#[test]
fn a_chain_once_paused_is_not_shared_with_a_later_output() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.log.count(0) >= 4));
    studio.pause_output(rec).unwrap();
    // Same settings as the recording: it would share a chain never paused.
    let mut same = OutputSettings::packets("stream://live/app");
    same.video = recording().video;
    let live = studio.start_output(&r.adapters, &same).unwrap();
    let report = studio.output(live).unwrap();
    assert!(!report.video_shared && !report.audio_shared, "{report:?}");
    studio.shutdown();
}
