//! Native pictures of the media source (ADR-0108 decisions 4 and 5): the surface
//! budget and the lease rules, with the fake native decoder of `rbs-media-decode-test`.
//! Its surfaces are fake values, so these tests never import one; the GPU import itself
//! is covered on macOS and Windows by the adapters' conformance and device runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::AudioFormat;
use rbs_core::time::{TestClock, Timestamp};
use rbs_media_decode::{DecodeTarget, InterleavedPcm};
use rbs_media_decode_test::{ScriptClip, ScriptFactory, ScriptFrame};
use rbs_nodes::{MEDIA_NATIVE_WAITING, MediaPair, media_pair};

const BLOCK: usize = 480;

struct Rig {
    pair: MediaPair,
    clock: Arc<TestClock>,
    factory: Arc<ScriptFactory>,
}

fn ms(n: u64) -> Timestamp {
    Timestamp::from_nanos(n * 1_000_000)
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    for _ in 0..20_000_000 {
        if ready() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("hang guard: the media worker did not reach the condition");
}

/// 200 pictures 5 ms apart (one second) with one second of quiet stereo sound, so
/// the lead of 150 ms holds 30 pictures: more than the budget.
fn dense_clip() -> ScriptClip {
    let step = Duration::from_millis(5);
    let frames = (0..200u32)
        .map(|i| ScriptFrame::rgba(2, 2, step * i, step, (i % 250) as u8 + 1))
        .collect();
    ScriptClip {
        frames,
        audio: Some(InterleavedPcm {
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.1; 48_000 * 2],
        }),
        ..ScriptClip::silent_frames()
    }
}

fn rig() -> Rig {
    let factory = Arc::new(ScriptFactory::native(dense_clip()));
    let clock = Arc::new(TestClock::new());
    let config = AudioGraphConfig::new(
        AudioFormat::STEREO_48K,
        BLOCK as u32,
        Duration::from_millis(50),
    )
    .unwrap();
    let pair = media_pair(
        factory.clone(),
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        false,
        true,
        clock.clone(),
        config,
    );
    Rig {
        pair,
        clock,
        factory,
    }
}

impl Rig {
    fn settle(&mut self, t: Timestamp) {
        self.clock.set(t);
        let picture = &self.pair.picture;
        wait_until(|| {
            picture.worker_ended() || picture.worker_settled_at().is_some_and(|s| s >= t)
        });
    }

    fn live(&self) -> u64 {
        self.factory.counts.native_live.load(Ordering::Acquire)
    }

    fn peak(&self) -> u64 {
        self.factory.counts.native_peak.load(Ordering::Acquire)
    }

    /// The target the worker opened with. Without a prepared picture node (these tests
    /// have no GPU) it is none (port invariant 11).
    fn opened_for(&self) -> Option<DecodeTarget> {
        self.factory.counts.last_target.lock().unwrap().clone()
    }
}

#[test]
fn queued_native_pictures_stay_within_the_surface_budget() {
    let mut rig = rig();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..400).step_by(10) {
        rig.settle(ms(t));
        let waiting = rig
            .pair
            .picture
            .stats()
            .native_waiting
            .load(Ordering::Acquire);
        assert!(waiting <= MEDIA_NATIVE_WAITING, "{waiting} at {t} ms");
    }
    // Nothing was imported: every live surface is a waiting one or the one the worker
    // has just decoded.
    assert!(
        rig.peak() <= u64::from(MEDIA_NATIVE_WAITING) + 1,
        "{}",
        rig.peak()
    );
    assert!(rig.peak() > 0);
    assert_eq!(rig.opened_for(), Some(DecodeTarget::none()));
    rig.pair.picture.standby();
    wait_until(|| rig.pair.picture.worker_joined());
    assert_eq!(rig.live(), 0, "every lease is released after standby");
    assert_eq!(
        rig.pair
            .picture
            .stats()
            .native_waiting
            .load(Ordering::Acquire),
        0
    );
}

#[test]
fn unseen_native_pictures_stay_within_the_budget_while_only_the_sound_plays() {
    let mut rig = rig();
    rig.pair.sound.attach().unwrap();
    let mut l = [0.0f32; BLOCK];
    let mut r = [0.0f32; BLOCK];
    for t in (0..600).step_by(10) {
        rig.settle(ms(t));
        rig.pair.sound.fill(ms(t), &mut [&mut l, &mut r], BLOCK);
    }
    assert!(
        rig.peak() <= u64::from(MEDIA_NATIVE_WAITING) + 1,
        "{}",
        rig.peak()
    );
    // Unseen pictures are neither copied nor queued.
    assert_eq!(rig.pair.picture.stats().uploaded.load(Ordering::Relaxed), 0);
    // A run the sound node started opens with no import device.
    assert_eq!(rig.opened_for(), Some(DecodeTarget::none()));
    rig.pair.sound.detach();
    wait_until(|| rig.pair.picture.worker_joined());
    assert_eq!(rig.live(), 0);
}

#[test]
fn a_native_picture_is_left_for_the_import_and_skipped_ones_go_back() {
    let mut rig = rig();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..300).step_by(10) {
        rig.settle(ms(t));
        // A native picture is never shared as a frame: the render path imports it.
        assert!(rig.pair.picture.dequeue(ms(t)).is_none());
    }
    // Pictures the clock passed went back to the worker, which released them.
    assert!(
        rig.pair
            .picture
            .stats()
            .dropped_video
            .load(Ordering::Relaxed)
            > 0
    );
    assert!(rig.live() <= u64::from(MEDIA_NATIVE_WAITING) + 1 + 1);
    rig.pair.picture.standby();
    wait_until(|| rig.pair.picture.worker_joined());
    assert_eq!(rig.live(), 0);
}
