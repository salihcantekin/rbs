//! Several destinations of one stream (ADR-0127): outputs started together in one
//! apply share one encoder and start on its same first keyframe, one that is refused
//! fails alone, a late one joins at a keyframe, a stalled one is removed without a
//! drain while the others go on, and End stream stops them all in one apply.
//! Reference adapters on a `TestClock`; tests wait for counts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_codec::RateControl;
use rbs_record::{OutputSettings, RecordError, Studio, StudioSettings};
use support::*;

fn start(r: &Rig) -> (Studio, rbs_core::time::TestClock) {
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

/// A destination of the stream: the same encoding for all, so they share it.
fn destination(url: &str) -> OutputSettings {
    let mut out = OutputSettings::packets(url);
    out.video.as_mut().unwrap().rate_control = RateControl::Cbr {
        bitrate_bps: 6_000_000,
    };
    out
}

const A: &str = "stream://live/a";
const B: &str = "stream://live/b";
const C: &str = "stream://live/c";

#[test]
fn destinations_started_together_share_one_encoder_and_a_refused_one_fails_alone() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let results = studio.start_outputs(
        &r.adapters,
        &[
            destination(A),
            destination("stream://fail/b"),
            destination(C),
        ],
    );
    assert_eq!(results.len(), 3);
    let a = *results[0].as_ref().unwrap();
    match &results[1] {
        Err(RecordError::Start(failed)) => {
            assert!(failed.iter().any(|f| f.node == "output"), "{failed:?}");
        }
        other => panic!("{other:?}"),
    }
    let c = *results[2].as_ref().unwrap();
    assert_eq!(studio.outputs(), [a, c]);
    // One encoder chain for the stream; the second destination shares it.
    assert!(!studio.output(a).unwrap().video_shared);
    assert!(studio.output(c).unwrap().video_shared);

    let reached = run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(A, 0).len() >= 8 && r.stream_log.sent_to(C, 0).len() >= 8
    });
    assert!(reached, "{:?}", studio.output_progress(c));
    // Both start on the same first keyframe and get the same packets.
    let (to_a, to_c) = (r.stream_log.sent_to(A, 0), r.stream_log.sent_to(C, 0));
    assert!(to_a[0].keyframe && to_c[0].keyframe);
    assert_eq!(to_a[0].pts, to_c[0].pts);
    assert_eq!(
        to_a[..8].iter().map(|w| w.pts).collect::<Vec<_>>(),
        to_c[..8].iter().map(|w| w.pts).collect::<Vec<_>>()
    );
    for id in [a, c] {
        assert_eq!(studio.output_failure(id), None);
    }
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn every_refused_destination_leaves_nothing_behind() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, _clock) = start(&r);
    let results = studio.start_outputs(
        &r.adapters,
        &[
            destination("stream://fail/a"),
            destination("stream://fail/b"),
        ],
    );
    assert!(
        results
            .iter()
            .all(|r| matches!(r, Err(RecordError::Start(_))))
    );
    assert!(studio.outputs().is_empty());
    // The shared chain went with them: nothing runs past the render (P13).
    assert!(!studio.engine().has_worker(rbs_record::VIDEO_ENCODER));
    assert!(!studio.engine().has_worker(rbs_record::OUTPUT));
    assert!(studio.start_outputs(&r.adapters, &[]).is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_destination_started_later_joins_the_shared_encoder_at_a_keyframe() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let a = studio.start_output(&r.adapters, &destination(A)).unwrap();
    // Past the first keyframe (the reference encoder makes one every 30 frames).
    assert!(run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(A, 0).len() >= 10
    }));
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    let b = studio.start_output(&r.adapters, &destination(B)).unwrap();
    assert!(studio.output(b).unwrap().video_shared);
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);
    assert!(run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(B, 0).len() >= 4
    }));
    let (to_a, to_b) = (r.stream_log.sent_to(A, 0), r.stream_log.sent_to(B, 0));
    assert!(to_b[0].keyframe, "{:?}", &to_b[..4]);
    assert!(to_b[0].pts > to_a[0].pts);
    // From there on it gets what the first destination gets.
    let from = to_a.iter().position(|w| w.pts == to_b[0].pts).unwrap();
    assert_eq!(
        to_a[from..from + 4]
            .iter()
            .map(|w| w.pts)
            .collect::<Vec<_>>(),
        to_b[..4].iter().map(|w| w.pts).collect::<Vec<_>>()
    );
    for id in [a, b] {
        assert!(studio.stop_output(id).is_some());
    }
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_stalled_destination_is_aborted_without_a_drain_and_the_others_go_on() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let results = studio.start_outputs(
        &r.adapters,
        &[
            destination(A),
            destination("stream://stall/b"),
            destination(C),
        ],
    );
    let ids: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    assert!(run_until(&mut studio, &clock, |s| {
        r.stream_log.sent_to(C, 0).len() >= 8
            && s.output_progress(ids[1])
                .is_some_and(|p| p.packets_taken >= 8)
    }));
    let finished = r.stream_log.finished.load(Ordering::Relaxed);
    let report = studio.abort_output(ids[1]).expect("it ran");
    // Nothing was delivered or finished on the way out; the others are untouched.
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), finished);
    assert_eq!(report.flush_lost, 0, "the shared encoder was not flushed");
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(studio.outputs(), [ids[0], ids[2]]);
    assert!(studio.abort_output(ids[1]).is_none());
    let before = r.stream_log.sent_to(A, 0).len();
    assert!(run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(A, 0).len() >= before + 8
    }));

    // End stream: the rest stop in one draining apply; each transport is finished.
    let reports = studio.stop_outputs(&[ids[0], ids[2], ids[1]]);
    assert_eq!(
        reports.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [ids[0], ids[2]]
    );
    for (_, report) in &reports {
        assert!(report.shutdown_errors.is_empty(), "{report:?}");
        assert_eq!(report.failure, None);
        // Readbacks the stop cut are not drops.
        assert_eq!(report.progress.export_drops, 0, "{report:?}");
    }
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), finished + 2);
    assert!(studio.outputs().is_empty());
    assert!(!studio.engine().has_worker(rbs_record::VIDEO_ENCODER));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_destination_started_during_a_hold_joins_held() {
    use rbs_core::pause::HoldPicture;
    use rbs_core::time::Clock;

    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let holdable = |url| {
        let mut out = destination(url);
        out.holdable = true;
        out
    };
    let a = studio.start_output(&r.adapters, &holdable(A)).unwrap();
    assert!(run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(A, 0).len() >= 4
    }));
    let held_at = studio.hold_output(a, HoldPicture::Freeze).unwrap();
    assert!(run_until(&mut studio, &clock, |_| clock.now() > held_at));

    let joined_at = clock.now();
    let b = studio.start_output(&r.adapters, &holdable(B)).unwrap();
    assert!(studio.output(b).unwrap().video_shared);
    let state = studio.output_hold(b).unwrap();
    assert_eq!(state.picture, Some(HoldPicture::Freeze));
    assert_eq!(state.held_at, Some(joined_at));
    assert!(run_until(&mut studio, &clock, |_| {
        r.stream_log.sent_to(B, 0).len() >= 4
    }));
    assert!(studio.output_progress(b).unwrap().held);

    // Releasing the stream releases every destination on its chain.
    studio.release_hold(a).unwrap();
    assert_eq!(studio.output_hold(b).unwrap().picture, None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
