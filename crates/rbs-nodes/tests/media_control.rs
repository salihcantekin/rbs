//! Play, pause, seek, and restart of a media source (ADR-0108 decisions 10 and 13).
//! The fake adapter is `rbs-media-decode-test`. No sleep: the test moves the
//! `TestClock` and waits until the worker has settled at that time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::AudioFormat;
use rbs_core::time::{TestClock, Timestamp};
use rbs_media_decode::InterleavedPcm;
use rbs_media_decode_test::{ScriptClip, ScriptFactory};
use rbs_nodes::{MediaPair, MediaPlayState as PlayState, media_pair};

const BLOCK: usize = 480;
const LATENCY: Duration = Duration::from_millis(50);

struct Rig {
    pair: MediaPair,
    clock: Arc<TestClock>,
    /// Blocks heard, by block time in ms.
    blocks: BTreeMap<u64, Vec<f32>>,
    /// The picture mark at each tick, by tick time in ms.
    marks: BTreeMap<u64, Option<u8>>,
}

fn ms(n: u64) -> Timestamp {
    Timestamp::from_nanos(n * 1_000_000)
}

/// Three pictures (marks 1, 2, 3) at 0, 100, and 200 ms, over 0.3 s of 48 kHz stereo
/// sound: 0.25 under the first picture, 0.5 under the second, 0.75 under the third.
fn steps() -> ScriptClip {
    let per = 4_800;
    let mut samples = Vec::new();
    for level in [0.25f32, 0.5, 0.75] {
        samples.extend(std::iter::repeat_n(level, per * 2));
    }
    ScriptClip {
        audio: Some(InterleavedPcm {
            sample_rate: 48_000,
            channels: 2,
            samples,
        }),
        duration: Some(Duration::from_millis(300)),
        ..ScriptClip::silent_frames()
    }
}

fn rig(loop_playback: bool) -> Rig {
    let factory = Arc::new(ScriptFactory::new(steps()));
    let clock = Arc::new(TestClock::new());
    let config = AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap();
    let pair = media_pair(
        factory,
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        loop_playback,
        true,
        clock.clone(),
        config,
    );
    let mut rig = Rig {
        pair,
        clock,
        blocks: BTreeMap::new(),
        marks: BTreeMap::new(),
    };
    // The sound node joins first and learns the block grid (multiples of 10 ms).
    rig.pair.sound.attach().unwrap();
    let mut l = [0.0f32; BLOCK];
    let mut r = [0.0f32; BLOCK];
    rig.pair
        .sound
        .fill(Timestamp::ZERO, &mut [&mut l, &mut r], BLOCK);
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    rig
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

impl Rig {
    /// Runs ticks every 10 ms from `from` to `to` (exclusive): picture and sound.
    fn run(&mut self, from: u64, to: u64) {
        for t in (from..to).step_by(10) {
            self.clock.set(ms(t));
            let picture = &self.pair.picture;
            wait_until(|| {
                picture.worker_ended() || picture.worker_settled_at().is_some_and(|s| s >= ms(t))
            });
            let _ = self.pair.picture.dequeue(ms(t));
            self.marks.insert(t, self.pair.picture.last_mark());
            if t >= 50 {
                let mut l = vec![0.0f32; BLOCK];
                let mut r = vec![0.0f32; BLOCK];
                let written = self
                    .pair
                    .sound
                    .fill(ms(t) - LATENCY, &mut [&mut l, &mut r], BLOCK);
                if written {
                    self.blocks.insert(t - 50, l);
                }
            }
        }
    }

    fn block(&self, at: u64) -> &[f32] {
        self.blocks
            .get(&at)
            .unwrap_or_else(|| panic!("no block at {at} ms"))
    }

    fn underruns(&self) -> u64 {
        self.pair
            .picture
            .stats()
            .sound_underruns
            .load(Ordering::Relaxed)
    }
}

fn all(block: &[f32], level: f32) -> bool {
    block.iter().all(|s| *s == level)
}

#[test]
fn pause_stops_picture_and_sound_at_the_same_media_time() {
    let mut rig = rig(false);
    rig.run(0, 210);
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    // At 200 ms: the instant is 200 plus the 50 ms render margin, 250 ms, which is
    // media 100 ms (the anchor is 150 ms).
    rig.pair.control.pause();
    rig.run(210, 600);
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
    // The first picture holds; the second (due at 250 ms) never shows.
    assert!(rig.marks.range(250..600).all(|(_, m)| *m == Some(1)));
    // Sound up to 250 ms, then silence that is not an underrun.
    assert!(all(rig.block(240), 0.25));
    assert!(rig.blocks.range(250..).next().is_none());
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn play_after_pause_skips_and_doubles_nothing() {
    let mut rig = rig(false);
    rig.run(0, 210);
    rig.pair.control.pause();
    rig.run(210, 410);
    // At 400 ms: the instant is 450 ms; the worker anchors one lead later, 600 ms.
    rig.pair.control.play().unwrap();
    rig.run(410, 950);
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    // Media 100 ms (where the pause stopped) plays at the new anchor.
    assert!(rig.blocks.range(250..600).next().is_none());
    assert!(all(rig.block(600), 0.5));
    assert!(all(rig.block(690), 0.5));
    assert!(all(rig.block(700), 0.75));
    assert!(all(rig.block(790), 0.75));
    assert!(!rig.blocks.contains_key(&800));
    assert_eq!(rig.marks[&590], Some(1));
    assert_eq!(rig.marks[&600], Some(2));
    assert_eq!(rig.marks[&700], Some(3));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn seek_while_playing_moves_picture_and_sound_together() {
    let mut rig = rig(false);
    rig.run(0, 210);
    // At 200 ms, to media 200 ms: the instant is 250 ms, the new anchor 400 ms.
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    rig.run(210, 600);
    assert!(all(rig.block(240), 0.25));
    assert!(rig.blocks.range(250..400).next().is_none());
    assert!(all(rig.block(400), 0.75));
    assert!(all(rig.block(490), 0.75));
    assert!(!rig.blocks.contains_key(&500));
    assert!(rig.marks.range(250..400).all(|(_, m)| *m == Some(1)));
    assert_eq!(rig.marks[&400], Some(3));
    assert_eq!(rig.underruns(), 0);
    let superseded = rig.pair.picture.stats().superseded.load(Ordering::Relaxed);
    assert!(superseded >= 1, "the old position's pictures were skipped");
}

#[test]
fn two_seeks_within_one_latency_play_only_the_second() {
    let mut rig = rig(false);
    rig.run(0, 210);
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    rig.pair.control.seek(Duration::ZERO).unwrap();
    rig.run(210, 750);
    // The second seek ended the first at the same instant (250 ms): from the new
    // anchor (400 ms) the clip plays from 0, and nothing of media 200 ms comes earlier.
    assert!(rig.blocks.range(250..400).next().is_none());
    assert!(all(rig.block(400), 0.25));
    assert!(all(rig.block(500), 0.5));
    assert!(all(rig.block(600), 0.75));
    assert!(!rig.blocks.contains_key(&700));
    assert!(rig.blocks.range(..600).all(|(_, b)| !b.contains(&0.75)));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn seek_while_paused_shows_the_new_picture_and_stays_silent() {
    let mut rig = rig(false);
    rig.run(0, 210);
    rig.pair.control.pause();
    rig.run(210, 310);
    // At 300 ms, to media 200 ms (the third picture): shown from 350 ms and held.
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    rig.run(310, 610);
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    assert_eq!(rig.marks[&340], Some(1));
    assert!(rig.marks.range(350..600).all(|(_, m)| *m == Some(3)));
    assert!(rig.blocks.range(250..).next().is_none());
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(200));
    assert_eq!(rig.underruns(), 0);
    // Play goes on from 200 ms: the last 100 ms of 0.75, anchored one lead after the
    // instant (650 ms).
    rig.pair.control.play().unwrap();
    rig.run(610, 950);
    let first = *rig.blocks.range(250..).next().unwrap().0;
    assert_eq!(first, 800);
    assert!(all(rig.block(800), 0.75));
    assert!(all(rig.block(890), 0.75));
    assert!(!rig.blocks.contains_key(&900));
}

#[test]
fn restart_from_the_end_plays_from_the_start() {
    let mut rig = rig(false);
    rig.run(0, 710);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    assert_eq!(rig.marks[&690], Some(3));
    rig.pair.control.play().unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    // A new worker anchors at 700 + 150 ms.
    rig.run(710, 1_200);
    assert_eq!(
        rig.marks[&840],
        Some(3),
        "the last picture holds until the new run"
    );
    assert_eq!(rig.marks[&850], Some(1));
    assert_eq!(rig.marks[&950], Some(2));
    assert!(all(rig.block(850), 0.25));
    assert!(all(rig.block(1_140), 0.75));
    assert!(!rig.blocks.contains_key(&1_150));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_source_paused_while_asleep_wakes_paused_at_its_cursor() {
    let factory = Arc::new(ScriptFactory::new(steps()));
    let clock = Arc::new(TestClock::new());
    let config = AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap();
    let mut pair = media_pair(
        factory,
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        false,
        false,
        clock.clone(),
        config,
    );
    assert_eq!(pair.control.state(), PlayState::Standby);
    pair.control.seek(Duration::from_millis(100)).unwrap();
    pair.control.pause();
    assert_eq!(pair.control.state(), PlayState::Paused);
    pair.picture.wake(Timestamp::ZERO).unwrap();
    clock.set(ms(10));
    let picture = &pair.picture;
    wait_until(|| picture.worker_settled_at().is_some_and(|s| s >= ms(10)));
    let _ = pair.picture.dequeue(ms(10));
    assert_eq!(pair.picture.last_mark(), Some(2), "the picture at 100 ms");
    clock.set(ms(500));
    let picture = &pair.picture;
    wait_until(|| picture.worker_settled_at().is_some_and(|s| s >= ms(500)));
    let _ = pair.picture.dequeue(ms(500));
    assert_eq!(pair.picture.last_mark(), Some(2), "still paused");
    assert_eq!(pair.control.state(), PlayState::Paused);
}

#[test]
fn a_command_does_not_allocate_in_either_node() {
    let mut rig = rig(true);
    rig.run(0, 200);
    rig.pair.control.pause();
    let mut l = vec![0.0f32; BLOCK];
    let mut r = vec![0.0f32; BLOCK];
    for t in (200..400).step_by(10) {
        rig.clock.set(ms(t));
        let picture = &rig.pair.picture;
        wait_until(|| picture.worker_settled_at().is_some_and(|s| s >= ms(t)));
        let picture = &mut rig.pair.picture;
        let sound = &mut rig.pair.sound;
        let info = allocation_counter::measure(|| {
            drop(picture.dequeue(ms(t)));
            sound.fill(ms(t) - LATENCY, &mut [&mut l, &mut r], BLOCK);
        });
        assert_eq!(info.count_total, 0, "allocated at {t} ms");
        if t == 300 {
            rig.pair.control.play().unwrap();
        }
    }
}

#[test]
fn play_within_one_latency_of_pause_doubles_nothing() {
    let mut rig = rig(false);
    rig.run(0, 210);
    // Both at 200 ms, instant 250 ms: the pause stops at media 100 ms, the play goes
    // on from there one lead after the instant (400 ms).
    rig.pair.control.pause();
    rig.pair.control.play().unwrap();
    rig.run(210, 700);
    assert!(all(rig.block(240), 0.25));
    assert!(rig.blocks.range(250..400).next().is_none());
    assert!(all(rig.block(400), 0.5));
    assert!(all(rig.block(500), 0.75));
    assert!(!rig.blocks.contains_key(&600));
    assert!(rig.blocks.range(250..).all(|(_, b)| !b.contains(&0.25)));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn seek_then_play_while_paused_plays_from_the_seek() {
    let mut rig = rig(false);
    rig.run(0, 210);
    rig.pair.control.pause();
    rig.run(210, 310);
    // Both at 300 ms, instant 350 ms: play from media 200 ms at 500 ms.
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    rig.pair.control.play().unwrap();
    rig.run(310, 700);
    assert!(rig.blocks.range(250..500).next().is_none());
    assert!(all(rig.block(500), 0.75));
    assert!(all(rig.block(590), 0.75));
    assert!(!rig.blocks.contains_key(&600));
    assert_eq!(rig.marks[&500], Some(3));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn pause_then_seek_within_one_latency_holds_the_new_picture() {
    let mut rig = rig(false);
    rig.run(0, 210);
    rig.pair.control.pause();
    rig.pair.control.seek(Duration::ZERO).unwrap();
    rig.run(210, 500);
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    assert_eq!(rig.pair.control.cursor(), Duration::ZERO);
    assert!(rig.marks.range(250..500).all(|(_, m)| *m == Some(1)));
    assert!(rig.blocks.range(250..).next().is_none());
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn restart_after_the_end_hold_reaches_the_picture_node() {
    let mut rig = rig(false);
    for t in (0..710).step_by(10) {
        rig.run(t, t + 10);
        rig.pair.picture.service();
    }
    assert_eq!(rig.pair.picture.phase(), rbs_nodes::MediaPhase::Hold);
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.pair.control.restart().unwrap();
    rig.run(710, 1_000);
    // After the hold no picture is current, so the new run's first picture is on air
    // at once (the cued rule); motion and sound start one lead later (850 ms).
    assert_eq!(rig.marks[&710], Some(1));
    assert_eq!(rig.marks[&940], Some(1));
    assert_eq!(rig.marks[&950], Some(2));
    assert!(all(rig.block(850), 0.25));
}

#[test]
fn pause_in_the_second_loop_pass_stops_at_that_pass_media_time() {
    let mut rig = rig(true);
    rig.run(0, 430);
    // At 420 ms, instant 470 ms: the second pass began at 450 ms, so media 20 ms.
    rig.pair.control.pause();
    rig.run(430, 610);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(20));
    // Play at 600 ms: media 20 ms at 800 ms.
    rig.pair.control.play().unwrap();
    rig.run(610, 1_000);
    assert!(rig.blocks.range(470..800).next().is_none());
    assert!(all(rig.block(800), 0.25));
    assert!(all(rig.block(870), 0.25));
    assert!(all(rig.block(880), 0.5));
    assert_eq!(rig.underruns(), 0);
}
