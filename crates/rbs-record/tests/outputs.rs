//! Several outputs per canvas (spec `recording.md`, "Several outputs", ADR-0047): a
//! recording and a live stream at once, on their own encoders when their settings
//! differ and sharing them when they match; each stops without touching the other.
//! Reference adapters on a `TestClock`, as in `studio.rs`; tests wait for counts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_codec::RateControl;
use rbs_core::media::ContainerId;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_record::{
    AUDIO_ENCODER, AUDIO_ENCODER_2, MAX_OUTPUTS, OUTPUT, OUTPUT_2, OutputSettings, RecordError,
    Studio, StudioSettings, VIDEO_ENCODER, VIDEO_ENCODER_2,
};
use support::*;

/// Workers of the two slots: video encoder, audio encoder, output; slot 0 then 1.
fn slots(studio: &Studio) -> [bool; 6] {
    [
        VIDEO_ENCODER,
        AUDIO_ENCODER,
        OUTPUT,
        VIDEO_ENCODER_2,
        AUDIO_ENCODER_2,
        OUTPUT_2,
    ]
    .map(|p| studio.engine().has_worker(p))
}

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
    OutputSettings::new("mem://rec", ContainerId::Matroska)
}

fn stream(bitrate_bps: u64) -> OutputSettings {
    let mut out = OutputSettings::packets("stream://live/app");
    out.video.rate_control = RateControl::Cbr { bitrate_bps };
    out
}

#[test]
fn a_recording_and_a_stream_with_different_settings_run_on_their_own_encoders() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    let one = r.video_sessions.load(Ordering::Relaxed);
    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    let report = studio.output(live).unwrap();
    assert!(!report.video_shared, "other bitrate: an encoder of its own");
    assert!(
        report.audio_shared,
        "same audio settings: the AAC encoder is shared"
    );
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 2 * one);
    assert_eq!(studio.outputs(), [rec, live]);
    assert_eq!(slots(&studio), [true, true, true, true, false, true]);

    // A third output is refused before anything is chosen.
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    assert!(matches!(
        studio.start_output(&r.adapters, &stream(3_000_000)),
        Err(RecordError::TooManyOutputs)
    ));
    assert_eq!(studio.outputs().len(), MAX_OUTPUTS);
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);

    let reached = run_until(&mut studio, &clock, |_| {
        r.log.count(0) >= 8 && r.log.count(1) >= 8 && r.stream_log.count(0) >= 8
    });
    assert!(reached, "{:?}", studio.output_progress(live));

    // The recording stops: its file is complete, the stream goes on.
    let report = studio.stop_output(rec).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(r.log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 0);
    // Its video encoder and output stopped; the AAC encoder stays for the stream.
    assert_eq!(slots(&studio), [false, true, false, true, false, true]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached, "the stream keeps delivering");
    let p = studio.output_progress(live).unwrap();
    assert_eq!((p.encoder_drops, p.mux_drops), (0, 0), "{p:?}");

    let report = studio.stop_output(live).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(slots(&studio), [false; 6]);
    let (reports, errors) = studio.shutdown();
    assert!(reports.is_empty());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn outputs_with_the_same_settings_share_the_encoders() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 16);
    assert!(reached);

    // A recording with the stream's settings joins the running encoders.
    let mut out = recording();
    out.video = stream(6_000_000).video;
    let joined = clock.now();
    let rec = studio.start_output(&r.adapters, &out).unwrap();
    let report = studio.output(rec).unwrap();
    assert!(report.video_shared && report.audio_shared, "{report:?}");
    assert_eq!(
        r.video_sessions.load(Ordering::Relaxed),
        sessions,
        "no second session"
    );
    // One encoder chain (slot 0), two outputs.
    assert_eq!(slots(&studio), [true, true, true, false, false, true]);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 16);
    assert!(reached, "{:?}", studio.output_progress(rec));

    // The joining output starts at a keyframe, at or after the join (less what the
    // encoder held then), and counts only what it took.
    let first = r
        .log
        .packets
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.stream == 0)
        .copied()
        .unwrap();
    assert!(first.keyframe, "{first:?}");
    let held = Timestamp::from_nanos(joined.as_nanos() - 100_000_000);
    assert!(first.pts >= held, "{first:?} joined at {joined:?}");
    let p_rec = studio.output_progress(rec).unwrap();
    let p_live = studio.output_progress(live).unwrap();
    assert!(
        p_rec.frames_encoded < p_live.frames_encoded,
        "{p_rec:?} {p_live:?}"
    );
    assert!(p_rec.frames_encoded > 0);

    // The stream ends first: the recording keeps its encoders and goes on.
    let report = studio.stop_output(live).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.flush_lost, 0, "a shared encoder is not flushed");
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(slots(&studio), [true, true, false, false, false, true]);
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 8);
    assert!(reached, "the recording keeps delivering");

    // A new stream takes the free output partition and shares the recording's chain.
    let again = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    assert!(studio.output(again).unwrap().video_shared);
    assert_eq!(slots(&studio), [true, true, true, false, false, true]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached);

    let (reports, errors) = studio.shutdown();
    assert_eq!(reports.len(), 2);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(reports.iter().all(|(_, r)| r.shutdown_errors.is_empty()));
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 2);
    assert_eq!(studio.engine().workers(), 0);
}

#[test]
fn an_output_that_cannot_start_leaves_the_running_one_alone() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 8));

    // Same settings (it would share the encoders), but the file cannot be opened.
    let mut out = OutputSettings::new("mem://fail", ContainerId::Matroska);
    out.video = stream(6_000_000).video;
    match studio.start_output(&r.adapters, &out) {
        Err(RecordError::Start(failed)) => {
            assert!(failed.iter().any(|f| f.node == "output"), "{failed:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(studio.outputs(), [live]);
    assert_eq!(slots(&studio), [true, true, true, false, false, false]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached, "the stream goes on");
    assert_eq!(studio.output_failure(live), None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
