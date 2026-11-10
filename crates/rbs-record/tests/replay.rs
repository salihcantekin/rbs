//! The replay buffer on a running studio (ADR-0088): arming adds its own encoders and
//! ring node, a requested clip starts at a keyframe, disarming removes everything and
//! stops its workers. Reference adapters on a `TestClock`, as in `outputs.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::media::ContainerId;
use rbs_core::time::TestClock;
use rbs_record::{
    OutputSettings, REPLAY_AUDIO_ENCODER, REPLAY_RING, REPLAY_VIDEO_ENCODER, RecordError,
    ReplaySettings, Studio, StudioSettings,
};
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

fn settings() -> ReplaySettings {
    let out = OutputSettings::new("mem://rec", ContainerId::Matroska);
    ReplaySettings::new(
        out.video.clone(),
        out.audio.clone(),
        Duration::from_secs(60),
        256 << 20,
    )
}

fn workers(studio: &Studio) -> [bool; 3] {
    [REPLAY_VIDEO_ENCODER, REPLAY_AUDIO_ENCODER, REPLAY_RING].map(|p| studio.engine().has_worker(p))
}

#[test]
fn arming_fills_the_buffer_and_a_clip_starts_at_a_keyframe() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    assert!(!studio.replay_armed());
    assert_eq!(
        workers(&studio),
        [false; 3],
        "disarmed: no worker (ADR-0013)"
    );

    studio.arm_replay(&r.adapters, &settings()).unwrap();
    assert!(studio.replay_armed());
    assert_eq!(workers(&studio), [true; 3]);
    assert!(matches!(
        studio.arm_replay(&r.adapters, &settings()),
        Err(RecordError::ReplayArmed)
    ));

    let reached = run_until(&mut studio, &clock, |s| {
        s.replay_status().is_some_and(|st| st.packets >= 12)
    });
    assert!(reached, "{:?}", studio.replay_status());

    studio.request_replay_clip(None).unwrap().unwrap();
    let mut clip = None;
    let reached = run_until(&mut studio, &clock, |s| {
        clip = clip.take().or_else(|| s.take_replay_clip());
        clip.is_some()
    });
    assert!(reached);
    let clip = clip.unwrap().unwrap();
    let first_video = clip
        .entries
        .iter()
        .find(|e| e.stream.0 == 0)
        .expect("a video packet");
    assert!(first_video.packet.keyframe());
    assert!(
        clip.rebased()
            .unwrap()
            .iter()
            .all(|e| e.packet.dts().as_nanos() < 60_000_000_000)
    );

    assert!(studio.disarm_replay());
    assert!(!studio.disarm_replay(), "second disarm: nothing to do");
    assert_eq!(studio.replay_status(), None);
    let run = run_until(&mut studio, &clock, |s| !s.engine().has_worker(REPLAY_RING));
    assert!(run, "the ring's worker stopped");
    assert_eq!(workers(&studio), [false; 3]);
    let (reports, errors) = studio.shutdown();
    assert!(reports.is_empty());
    assert!(errors.is_empty(), "{errors:?}");
}
