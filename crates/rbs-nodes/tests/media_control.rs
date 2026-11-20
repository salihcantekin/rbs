//! Play, pause, seek, and restart of a media source (ADR-0108 decisions 10 and 13).
//! The fake adapter is `rbs-media-decode-test`. No sleep: the test moves the
//! `TestClock` and waits until the worker has settled at that time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::capability::CapabilitySet;
use rbs_core::media::AudioFormat;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::{TestClock, Timestamp};
use rbs_media_decode::{DecodeError, InterleavedPcm, MediaDecoder, MediaDecoderFactory};
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
    rig_with(loop_playback, true, true)
}

/// A rig whose source starts on air or only in a preview scene.
fn rig_with(loop_playback: bool, restart_when_shown: bool, on_air: bool) -> Rig {
    let factory = Arc::new(ScriptFactory::new(steps()));
    rig_from(factory, loop_playback, restart_when_shown, on_air)
}

fn rig_from(
    factory: Arc<dyn MediaDecoderFactory>,
    loop_playback: bool,
    restart_when_shown: bool,
    on_air: bool,
) -> Rig {
    rig_ranged(factory, loop_playback, restart_when_shown, on_air, None)
}

/// A rig on air whose play range is `start` to `end` ms, set before the first wake.
fn rig_range(loop_playback: bool, start: u64, end: Option<u64>) -> Rig {
    let factory = Arc::new(ScriptFactory::new(steps()));
    let range = (Duration::from_millis(start), end.map(Duration::from_millis));
    rig_ranged(factory, loop_playback, true, true, Some(range))
}

fn rig_ranged(
    factory: Arc<dyn MediaDecoderFactory>,
    loop_playback: bool,
    restart_when_shown: bool,
    on_air: bool,
    range: Option<(Duration, Option<Duration>)>,
) -> Rig {
    let clock = Arc::new(TestClock::new());
    let config = AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap();
    let pair = media_pair(
        factory,
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        loop_playback,
        restart_when_shown,
        clock.clone(),
        config,
    );
    if let Some((start, end)) = range {
        pair.control.set_range(start, end).unwrap();
    }
    pair.control.set_on_air(on_air).unwrap();
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
            let control = &self.pair.control;
            wait_until(|| {
                picture.worker_ended()
                    || control.is_released()
                    || picture.worker_settled_at().is_some_and(|s| s >= ms(t))
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
fn a_new_file_stays_stopped_until_play() {
    let factory = Arc::new(ScriptFactory::new(steps()));
    let clock = Arc::new(TestClock::new());
    let config = AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap();
    let mut pair = media_pair(
        factory,
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        true,
        true,
        clock.clone(),
        config,
    );
    pair.control.set_on_air(true).unwrap();
    pair.control.hold_until_play();
    pair.sound.attach().unwrap();
    pair.picture.wake(Timestamp::ZERO).unwrap();
    assert_eq!(pair.control.state(), PlayState::Paused);
    clock.set(ms(10));
    let picture = &pair.picture;
    wait_until(|| picture.worker_settled_at().is_some_and(|s| s >= ms(10)));
    let _ = pair.picture.dequeue(ms(10));
    let mark = pair.picture.last_mark();
    assert!(mark.is_some());
    clock.set(ms(400));
    let picture = &pair.picture;
    wait_until(|| picture.worker_settled_at().is_some_and(|s| s >= ms(400)));
    let _ = pair.picture.dequeue(ms(400));
    assert_eq!(pair.picture.last_mark(), mark);
    pair.control.play().unwrap();
    assert_eq!(pair.control.state(), PlayState::Playing);
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
    pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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
    rig.pair.control.pause().unwrap();
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

#[test]
fn the_pause_release_frees_the_worker_and_play_resumes_at_the_cursor() {
    let mut rig = rig(false);
    rig.pair
        .control
        .set_pause_release(Duration::from_millis(200));
    rig.run(0, 210);
    // Paused from 250 ms at media 100 ms.
    rig.pair.control.pause().unwrap();
    rig.run(210, 450);
    rig.pair.control.service();
    assert!(
        !rig.pair.control.is_released(),
        "200 ms after 250 ms is 450 ms"
    );
    rig.run(450, 460);
    rig.pair.control.service();
    assert!(rig.pair.control.is_released());
    wait_until(|| rig.pair.picture.worker_joined());
    wait_until(|| !rig.pair.picture.decoder_held());
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
    rig.run(460, 600);
    assert!(rig.marks.range(250..600).all(|(_, m)| *m == Some(1)));
    // At 590 ms play opens the file again; the new worker anchors media 100 ms one
    // lead after its start, at 740 ms.
    rig.pair.control.play().unwrap();
    assert!(!rig.pair.control.is_released());
    assert!(rig.pair.picture.decoder_held());
    rig.run(600, 1_000);
    assert!(rig.blocks.range(250..740).next().is_none());
    assert!(all(rig.block(740), 0.5));
    assert!(all(rig.block(830), 0.5));
    assert!(all(rig.block(840), 0.75));
    assert!(all(rig.block(930), 0.75));
    assert!(!rig.blocks.contains_key(&940));
    assert_eq!(rig.marks[&730], Some(1));
    assert_eq!(rig.marks[&740], Some(2));
    assert_eq!(rig.marks[&840], Some(3));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_seek_after_the_pause_release_shows_its_picture_and_releases_again() {
    let mut rig = rig(false);
    rig.pair
        .control
        .set_pause_release(Duration::from_millis(100));
    rig.run(0, 210);
    rig.pair.control.pause().unwrap();
    rig.run(210, 360);
    rig.pair.control.service();
    assert!(rig.pair.control.is_released());
    // At 350 ms a worker starts for the one picture at 200 ms, shown from 400 ms.
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    assert!(!rig.pair.control.is_released());
    rig.run(360, 500);
    assert_eq!(rig.marks[&390], Some(1));
    assert_eq!(rig.marks[&400], Some(3));
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(200));
    // The seek's instant is 400 ms: released again 100 ms later.
    rig.pair.control.service();
    assert!(!rig.pair.control.is_released());
    rig.run(500, 510);
    rig.pair.control.service();
    assert!(rig.pair.control.is_released());
    wait_until(|| !rig.pair.picture.decoder_held());
    rig.run(510, 600);
    assert!(rig.marks.range(400..600).all(|(_, m)| *m == Some(3)));
    assert!(rig.blocks.range(250..).next().is_none());
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_source_only_a_preview_draws_is_cued_and_plays_on_the_take() {
    let mut rig = rig_with(false, true, false);
    rig.run(0, 300);
    // The picture at the start is shown from the first tick after the worker sees the
    // picture node (the sound node started it) and held; no sound, no underrun.
    assert!(rig.marks.range(10..300).all(|(_, m)| *m == Some(1)));
    assert!(rig.blocks.is_empty());
    // Play in a preview does not play.
    rig.pair.control.play().unwrap();
    rig.run(300, 400);
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    assert!(rig.blocks.is_empty());
    // The take at 390 ms: the instant is 440 ms, motion and sound one lead later.
    rig.pair.control.set_on_air(true).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    rig.run(400, 1_000);
    assert!(rig.blocks.range(..590).next().is_none());
    assert!(all(rig.block(590), 0.25));
    assert!(all(rig.block(690), 0.5));
    assert!(all(rig.block(790), 0.75));
    assert!(!rig.blocks.contains_key(&890));
    assert_eq!(rig.marks[&580], Some(1));
    assert_eq!(rig.marks[&690], Some(2));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_take_cues_a_playing_clip_at_the_start_and_a_take_back_plays_it() {
    let mut rig = rig_with(false, true, true);
    rig.run(0, 260);
    assert_eq!(rig.marks[&250], Some(2));
    // Taken off air at 250 ms: cued at the start (restart_when_shown) from 300 ms.
    rig.pair.control.set_on_air(false).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    rig.run(260, 500);
    assert!(rig.marks.range(300..500).all(|(_, m)| *m == Some(1)));
    assert!(rig.blocks.range(300..).next().is_none());
    assert_eq!(rig.pair.control.cursor(), Duration::ZERO);
    // Taken back at 490 ms: from the start, anchored at 540 + 150 ms.
    rig.pair.control.set_on_air(true).unwrap();
    rig.run(500, 1_100);
    assert!(rig.blocks.range(300..690).next().is_none());
    assert!(all(rig.block(690), 0.25));
    assert!(all(rig.block(890), 0.75));
    assert!(!rig.blocks.contains_key(&990));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn without_restart_when_shown_a_take_cues_where_it_stopped() {
    let mut rig = rig_with(false, false, true);
    rig.run(0, 260);
    // Off air at 250 ms: the instant is 300 ms, media 150 ms.
    rig.pair.control.set_on_air(false).unwrap();
    rig.run(260, 400);
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(150));
    assert!(rig.marks.range(300..400).all(|(_, m)| *m == Some(2)));
    // Back on air at 390 ms: media 150 ms plays from 440 + 150 ms.
    rig.pair.control.set_on_air(true).unwrap();
    rig.run(400, 900);
    assert!(rig.blocks.range(300..590).next().is_none());
    assert!(all(rig.block(590), 0.5));
    assert!(all(rig.block(640), 0.75));
    assert!(!rig.blocks.contains_key(&790));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_paused_clip_stays_paused_through_a_take_and_back_without_restart() {
    let mut rig = rig_with(false, false, true);
    rig.run(0, 210);
    rig.pair.control.pause().unwrap();
    rig.run(210, 300);
    rig.pair.control.set_on_air(false).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    rig.run(300, 400);
    rig.pair.control.set_on_air(true).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    rig.run(400, 600);
    assert!(rig.marks.range(250..600).all(|(_, m)| *m == Some(1)));
    assert!(rig.blocks.range(250..).next().is_none());
}

#[test]
fn a_paused_clip_taken_back_with_restart_when_shown_plays_from_the_start() {
    let mut rig = rig_with(false, true, true);
    rig.run(0, 210);
    rig.pair.control.pause().unwrap();
    rig.run(210, 300);
    rig.pair.control.set_on_air(false).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    rig.run(300, 400);
    // At 390 ms: from the start, anchored at 440 + 150 ms.
    rig.pair.control.set_on_air(true).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    rig.run(400, 1_000);
    assert!(rig.blocks.range(250..590).next().is_none());
    assert!(all(rig.block(590), 0.25));
    assert!(all(rig.block(790), 0.75));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_cued_source_that_sleeps_plays_when_a_program_scene_wakes_it() {
    let mut rig = rig_with(false, false, false);
    rig.run(0, 100);
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    rig.pair.picture.standby();
    rig.pair.sound.detach();
    assert_eq!(rig.pair.control.state(), PlayState::Standby);
    rig.pair.control.set_on_air(true).unwrap();
    rig.clock.set(ms(100));
    rig.pair.sound.attach().unwrap();
    rig.pair.picture.wake(ms(100)).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    // The new worker anchors the cursor (0) at 100 + 150 ms.
    rig.run(110, 700);
    assert!(rig.blocks.range(..250).next().is_none());
    assert!(all(rig.block(250), 0.25));
    assert!(all(rig.block(450), 0.75));
    assert!(!rig.blocks.contains_key(&550));
    assert_eq!(rig.marks[&350], Some(2));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_source_woken_cued_long_after_its_pause_keeps_its_worker_for_the_release_time() {
    let mut rig = rig_with(false, true, false);
    rig.pair
        .control
        .set_pause_release(Duration::from_millis(200));
    rig.run(0, 100);
    rig.pair.picture.standby();
    rig.pair.sound.detach();
    // Asleep far longer than the release time; the cue record is from 0 ms.
    rig.clock.set(ms(5_000));
    rig.pair.sound.attach().unwrap();
    rig.pair.picture.wake(ms(5_000)).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    rig.run(5_000, 5_190);
    rig.pair.control.service();
    assert!(!rig.pair.control.is_released(), "counted from the wake");
    rig.run(5_190, 5_210);
    rig.pair.control.service();
    assert!(rig.pair.control.is_released());
}

#[test]
fn a_take_after_a_seek_while_cued_starts_at_the_range_start() {
    let mut rig = rig_with(false, true, false);
    rig.run(0, 100);
    rig.pair.control.seek(Duration::from_millis(200)).unwrap();
    rig.run(100, 200);
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    assert_eq!(rig.marks[&190], Some(3));
    // The take at 190 ms: from 0, anchored at 240 + 150 ms.
    rig.pair.control.set_on_air(true).unwrap();
    rig.run(200, 800);
    assert!(rig.blocks.range(..390).next().is_none());
    assert!(all(rig.block(390), 0.25));
    assert!(all(rig.block(590), 0.75));
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn pause_of_a_cued_source_holds_and_restart_off_air_cues_at_the_start() {
    let mut rig = rig_with(false, true, false);
    rig.run(0, 100);
    // Pause makes it paused: it no longer plays on the take.
    rig.pair.control.pause().unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Paused);
    // Restart off air does not play: it cues at the start.
    rig.pair.control.restart().unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    rig.run(100, 300);
    assert!(rig.blocks.is_empty());
    assert!(rig.marks.range(160..300).all(|(_, m)| *m == Some(1)));
}

#[test]
fn an_ended_clip_stays_ended_in_a_preview_and_plays_from_the_start_on_air() {
    let mut rig = rig_with(false, true, true);
    rig.run(0, 710);
    wait_until(|| rig.pair.picture.worker_ended());
    rig.pair.picture.service();
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.pair.control.set_on_air(false).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.run(710, 800);
    assert!(rig.marks.range(710..800).all(|(_, m)| *m == Some(3)));
    // On air again with `restart_when_shown`: from the start at 790 + 150 ms.
    rig.pair.control.set_on_air(true).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    rig.run(800, 1_300);
    assert!(all(rig.block(940), 0.25));
    assert_eq!(rig.marks[&940], Some(1));
}

/// Opens the steps clip until `fail` is set.
struct Flaky {
    inner: ScriptFactory,
    fail: std::sync::atomic::AtomicBool,
}

impl AdapterFactory for Flaky {
    fn id(&self) -> AdapterId {
        self.inner.id()
    }

    fn display_name(&self) -> &str {
        "Flaky scripted media"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.inner.probe()
    }
}

impl MediaDecoderFactory for Flaky {
    fn open(&self, path: &std::path::Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(DecodeError::Failed("the file is gone".into()));
        }
        self.inner.open(path)
    }
}

#[test]
fn a_file_gone_after_the_pause_release_is_unavailable_and_keeps_its_picture() {
    let factory = Arc::new(Flaky {
        inner: ScriptFactory::new(steps()),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let mut rig = rig_from(factory.clone(), false, true, true);
    rig.pair
        .control
        .set_pause_release(Duration::from_millis(100));
    rig.run(0, 160);
    rig.pair.control.pause().unwrap();
    rig.run(160, 320);
    rig.pair.control.service();
    assert!(rig.pair.control.is_released());
    factory.fail.store(true, Ordering::Relaxed);
    assert!(rig.pair.control.play().is_err());
    assert_eq!(rig.pair.control.state(), PlayState::Unavailable);
    rig.pair.picture.service();
    rig.run(320, 400);
    assert!(rig.marks.range(320..400).all(|(_, m)| *m == Some(1)));
    assert!(rig.pair.picture.attempts() >= 1);
    // The file is back: the next play opens it again.
    factory.fail.store(false, Ordering::Relaxed);
    rig.pair.control.play().unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
}

// Play range (ADR-0108 decision 11).

impl Rig {
    /// Every tick from `from` to `to` (exclusive) shows the picture `mark`.
    fn assert_marks(&self, from: u64, to: u64, mark: u8) {
        let wrong: Vec<_> = self
            .marks
            .range(from..to)
            .filter(|(_, m)| **m != Some(mark))
            .collect();
        assert!(wrong.is_empty(), "not picture {mark}: {wrong:?}");
    }
}

#[test]
fn a_play_range_plays_only_its_part_and_ends() {
    let mut rig = rig_range(false, 100, Some(200));
    rig.run(0, 500);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    // Media 100 ms at the anchor (150 ms): the second picture and its sound only.
    rig.assert_marks(150, 500, 2);
    assert!(all(rig.block(150), 0.5));
    assert!(all(rig.block(240), 0.5));
    assert!(!rig.blocks.contains_key(&250));
    assert!(
        rig.blocks
            .values()
            .all(|b| !b.contains(&0.25) && !b.contains(&0.75))
    );
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_looping_play_range_starts_each_pass_at_its_start_with_no_gap() {
    let mut rig = rig_range(true, 100, Some(200));
    rig.run(0, 600);
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    // Passes of 100 ms from 150 ms: the second picture and 0.5 throughout.
    rig.assert_marks(150, 600, 2);
    for at in (150..540).step_by(10) {
        assert!(all(rig.block(at), 0.5), "block at {at} ms");
    }
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn a_range_start_alone_plays_to_the_end_of_the_file() {
    let mut rig = rig_range(true, 200, None);
    rig.run(0, 400);
    // The third picture from 150 ms; each pass is 100 ms long and plays only it.
    rig.assert_marks(150, 400, 3);
    for at in (150..340).step_by(10) {
        assert!(all(rig.block(at), 0.75), "block at {at} ms");
    }
    assert_eq!(rig.underruns(), 0);
}

#[test]
fn restart_from_the_end_hold_plays_from_the_range_start() {
    let mut rig = rig_range(false, 100, Some(200));
    rig.run(0, 300);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.pair.control.restart().unwrap();
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
    rig.run(300, 600);
    rig.assert_marks(300, 600, 2);
    assert!(all(rig.block(450), 0.5));
    assert!(
        rig.blocks
            .values()
            .all(|b| !b.contains(&0.25) && !b.contains(&0.75))
    );
}

#[test]
fn a_seek_is_clamped_into_the_play_range() {
    let mut rig = rig_range(true, 100, Some(200));
    rig.run(0, 210);
    rig.pair.control.pause().unwrap();
    rig.run(210, 310);
    rig.pair.control.seek(Duration::ZERO).unwrap();
    rig.run(310, 410);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
    // Past the end: the last picture before the range end.
    rig.pair.control.seek(Duration::from_millis(250)).unwrap();
    rig.run(410, 510);
    assert!(rig.pair.control.cursor() < Duration::from_millis(200));
    assert!(rig.pair.control.cursor() >= Duration::from_millis(100));
    rig.assert_marks(150, 510, 2);
}

#[test]
fn a_range_with_end_not_after_start_is_refused_and_changes_nothing() {
    let rig = rig_range(true, 100, Some(200));
    for (start, end) in [(200, 200), (200, 100)] {
        let refused = rig.pair.control.set_range(
            Duration::from_millis(start),
            Some(Duration::from_millis(end)),
        );
        assert!(refused.is_err());
    }
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
}

#[test]
fn a_new_range_moves_a_playing_cursor_outside_it_to_its_start() {
    let mut rig = rig(true);
    rig.run(0, 210);
    // At 200 ms: playing media 50 ms; the new range starts at 200 ms.
    rig.pair
        .control
        .set_range(Duration::from_millis(200), None)
        .unwrap();
    rig.run(210, 600);
    // A seek at the instant (250 ms), anchored one lead later (400 ms).
    assert_eq!(rig.marks[&400], Some(3));
    assert!(all(rig.block(400), 0.75));
    assert!(rig.blocks.range(400..550).all(|(_, b)| all(b, 0.75)));
}

#[test]
fn an_empty_range_shows_one_picture_and_ends() {
    let mut rig = rig_range(false, 120, Some(130));
    rig.run(0, 400);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.assert_marks(150, 400, 2);
}

#[test]
fn a_looping_empty_range_holds_its_picture_without_seeking_again() {
    let mut rig = rig_range(true, 120, Some(130));
    rig.run(0, 400);
    let seeks = rig.pair.picture.stats().uploaded.load(Ordering::Relaxed);
    rig.run(400, 800);
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    rig.assert_marks(150, 800, 2);
    // Nothing more was decoded or queued while held.
    assert_eq!(
        rig.pair.picture.stats().uploaded.load(Ordering::Relaxed),
        seeks
    );
}

#[test]
fn a_range_start_past_the_end_of_the_file_shows_the_last_picture() {
    let mut rig = rig_range(false, 5_000, None);
    rig.run(0, 400);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.assert_marks(150, 400, 3);
}

// Live keys (ADR-0108 decision 12).

#[test]
fn a_live_loop_on_an_ended_clip_plays_again_from_the_range_start() {
    let mut rig = rig_range(false, 100, Some(200));
    rig.run(0, 300);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    rig.pair.control.set_loop(true).unwrap();
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
    rig.run(300, 800);
    // It now loops: still playing, the second picture and its sound only.
    assert_eq!(rig.pair.control.state(), PlayState::Playing);
    rig.assert_marks(300, 800, 2);
    assert!(all(rig.block(700), 0.5));
}

#[test]
fn a_live_loop_off_ends_at_the_next_range_end() {
    let mut rig = rig(true);
    rig.run(0, 210);
    rig.pair.control.set_loop(false).unwrap();
    rig.run(210, 700);
    wait_until(|| rig.pair.picture.worker_ended());
    assert_eq!(rig.pair.control.state(), PlayState::Ended);
    // The first pass ends at 450 ms: nothing after it.
    assert!(all(rig.block(440), 0.75));
    assert!(!rig.blocks.contains_key(&450));
}

#[test]
fn a_live_restart_when_shown_applies_at_the_next_take() {
    let mut rig = rig_with(true, true, true);
    rig.run(0, 210);
    rig.pair.control.set_restart_when_shown(false);
    // Taken off air at 200 ms (media 50 ms): cued where it stopped, not at the start.
    rig.pair.control.set_on_air(false).unwrap();
    rig.run(210, 400);
    assert_eq!(rig.pair.control.state(), PlayState::Cued);
    assert_eq!(rig.pair.control.cursor(), Duration::from_millis(100));
}
