//! The media picture node and sound node (ADR-0106, ADR-0108 decisions 5 and 13). The
//! fake adapter is `rbs-media-decode-test`. No FFmpeg, no sleep: the test moves the
//! `TestClock` and waits until the worker has settled at that time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::AudioFormat;
use rbs_core::time::{TestClock, Timestamp};
use rbs_graph::node::LinkKind;
use rbs_media_decode::InterleavedPcm;
use rbs_media_decode_test::{ScriptClip, ScriptFactory};
use rbs_nodes::{MediaPair, MediaPhase, media_descriptor, media_pair, media_sound_descriptor};

const BLOCK: usize = 480;
const LATENCY: Duration = Duration::from_millis(50);
const LEAD: Duration = Duration::from_millis(150);

struct Rig {
    pair: MediaPair,
    clock: Arc<TestClock>,
    factory: Arc<ScriptFactory>,
}

fn config() -> AudioGraphConfig {
    AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap()
}

fn rig(clip: ScriptClip, loop_playback: bool, restart: bool) -> Rig {
    let factory = Arc::new(ScriptFactory::new(clip));
    let clock = Arc::new(TestClock::new());
    let pair = media_pair(
        factory.clone(),
        std::path::PathBuf::from(r"C:\clips\clip.mp4"),
        loop_playback,
        restart,
        clock.clone(),
        config(),
    );
    Rig {
        pair,
        clock,
        factory,
    }
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

impl Rig {
    /// Attaches the sound node and tells it the block grid (block times at multiples
    /// of 10 ms), as its first `process` would.
    fn attach_sound(&mut self) {
        self.pair.sound.attach();
        let mut l = [0.0f32; BLOCK];
        let mut r = [0.0f32; BLOCK];
        self.pair
            .sound
            .fill(Timestamp::ZERO, &mut [&mut l, &mut r], BLOCK);
    }

    /// Moves the clock to `t` and waits until the worker has queued everything due by
    /// `t` plus the lead (or has ended).
    fn settle(&mut self, t: Timestamp) {
        self.clock.set(t);
        let picture = &self.pair.picture;
        wait_until(|| {
            picture.worker_ended() || picture.worker_settled_at().is_some_and(|s| s >= t)
        });
    }

    /// The sound node's block for the tick at `t` (block time `t` minus the latency).
    fn block_at(&mut self, t: Timestamp) -> (bool, Vec<f32>) {
        let mut l = vec![0.0f32; BLOCK];
        let mut r = vec![0.0f32; BLOCK];
        let written = self
            .pair
            .sound
            .fill(t - LATENCY, &mut [&mut l, &mut r], BLOCK);
        (written, l)
    }
}

/// Three pictures (marks 1, 2, 3) at 0, 100, and 200 ms, with 0.3 s of stereo sound
/// at `rate`: 0.25 under the first picture, 0.5 under the second, 0.75 under the third.
fn clip_with_steps(rate: u32) -> ScriptClip {
    let per = rate as usize / 10;
    let mut samples = Vec::with_capacity(per * 3 * 2);
    for level in [0.25f32, 0.5, 0.75] {
        samples.extend(std::iter::repeat_n(level, per * 2));
    }
    ScriptClip {
        audio: Some(InterleavedPcm {
            sample_rate: rate,
            channels: 2,
            samples,
        }),
        ..ScriptClip::silent_frames()
    }
}

fn clip_with_constant(rate: u32, level: f32) -> ScriptClip {
    ScriptClip {
        audio: Some(InterleavedPcm {
            sample_rate: rate,
            channels: 2,
            samples: vec![level; rate as usize * 3 / 10 * 2],
        }),
        ..ScriptClip::silent_frames()
    }
}

#[test]
fn the_picture_node_has_one_video_pin_and_the_sound_node_one_audio_pin() {
    let picture: Vec<_> = media_descriptor().outputs.iter().map(|p| p.kind).collect();
    let sound: Vec<_> = media_sound_descriptor()
        .outputs
        .iter()
        .map(|p| p.kind)
        .collect();
    assert_eq!(picture, [LinkKind::Video]);
    assert_eq!(sound, [LinkKind::Audio]);
}

#[test]
fn a_file_with_no_audio_stream_gives_a_silent_sound_node_without_underruns() {
    let mut rig = rig(ScriptClip::silent_frames(), true, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..400).step_by(10) {
        rig.settle(ms(t));
        let (written, block) = rig.block_at(ms(t));
        assert!(!written);
        assert!(block.iter().all(|s| *s == 0.0));
    }
    let stats = rig.pair.picture.stats();
    assert_eq!(stats.sound_underruns.load(Ordering::Relaxed), 0);
    rig.pair.picture.standby();
}

#[test]
fn the_cued_picture_shows_at_once_and_motion_starts_one_lead_later() {
    let mut rig = rig(ScriptClip::silent_frames(), true, true);
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    rig.settle(ms(0));
    // The first picture is on air before its stamp (150 ms).
    assert!(rig.pair.picture.dequeue(ms(0)).is_some());
    assert_eq!(rig.pair.picture.last_mark(), Some(1));
    rig.settle(ms(200));
    let _ = rig.pair.picture.dequeue(ms(200));
    assert_eq!(
        rig.pair.picture.last_mark(),
        Some(1),
        "second picture is due at 250 ms"
    );
    assert!(
        rig.pair
            .picture
            .stats()
            .repeated_video
            .load(Ordering::Relaxed)
            >= 1
    );
    rig.settle(ms(260));
    let _ = rig.pair.picture.dequeue(ms(260));
    assert_eq!(rig.pair.picture.last_mark(), Some(2));
    rig.pair.picture.standby();
}

#[test]
fn a_late_render_skips_to_the_due_picture_and_counts_the_drop() {
    let mut rig = rig(ScriptClip::silent_frames(), true, true);
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    rig.settle(ms(0));
    assert!(rig.pair.picture.dequeue(ms(0)).is_some());
    rig.settle(ms(200));
    // The worker has queued up to 350 ms; the render comes back at 360 ms.
    rig.settle(ms(360));
    let _ = rig.pair.picture.dequeue(ms(360));
    assert_eq!(rig.pair.picture.last_mark(), Some(3));
    assert!(
        rig.pair
            .picture
            .stats()
            .dropped_video
            .load(Ordering::Relaxed)
            >= 1
    );
    rig.pair.picture.standby();
}

#[test]
fn a_picture_and_its_sound_reach_the_program_at_the_same_master_time() {
    let mut rig = rig(clip_with_steps(48_000), false, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    let mut picture_two_at = None;
    let mut blocks = BTreeMap::new();
    for t in (0..600).step_by(10) {
        rig.settle(ms(t));
        let _ = rig.pair.picture.dequeue(ms(t));
        if picture_two_at.is_none() && rig.pair.picture.last_mark() == Some(2) {
            picture_two_at = Some(t);
        }
        if t >= 50 {
            let (written, block) = rig.block_at(ms(t));
            if written {
                blocks.insert(t - 50, block);
            }
        }
    }
    // Picture two (media 100 ms) is due at the anchor (150 ms) plus 100 ms.
    assert_eq!(picture_two_at, Some(250));
    // Its sound starts exactly at the block stamped 250 ms.
    assert_eq!(blocks[&250][0], 0.5);
    assert_eq!(blocks[&240][BLOCK - 1], 0.25);
    // The first sound is at the anchor: the block at 150 ms starts with it.
    assert_eq!(blocks[&150][0], 0.25);
    assert!(!blocks.contains_key(&140));
    // The last sound ends at 450 ms.
    assert_eq!(blocks[&440][BLOCK - 1], 0.75);
    assert!(!blocks.contains_key(&450));
    let stats = rig.pair.picture.stats();
    assert_eq!(stats.sound_underruns.load(Ordering::Relaxed), 0);
    assert_eq!(stats.worker_dropped_blocks.load(Ordering::Relaxed), 0);
    rig.pair.picture.standby();
}

#[test]
fn a_44_1_khz_file_plays_at_the_right_speed() {
    let mut rig = rig(clip_with_constant(44_100, 0.5), false, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    let mut heard = 0usize;
    let mut last = 0u64;
    for t in (0..700).step_by(10) {
        rig.settle(ms(t));
        if t >= 50 {
            let (written, block) = rig.block_at(ms(t));
            if written {
                last = t - 50;
                heard += block.iter().filter(|s| (**s - 0.5).abs() < 0.01).count();
            }
        }
    }
    // 0.3 s at 48 kHz, minus the resampler's edges.
    assert!((14_400 - 100..=14_400).contains(&heard), "{heard} frames");
    assert_eq!(last, 440, "the sound ends 0.3 s after the anchor");
    rig.pair.picture.standby();
}

#[test]
fn a_loop_boundary_leaves_no_gap_in_the_sound() {
    let mut rig = rig(clip_with_constant(48_000, 0.5), true, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..1_100).step_by(10) {
        rig.settle(ms(t));
        let _ = rig.pair.picture.dequeue(ms(t));
        if t >= 200 {
            let (written, block) = rig.block_at(ms(t));
            assert!(written, "block at {} ms", t - 50);
            assert!(
                block.iter().all(|s| *s == 0.5),
                "a gap in the block at {} ms",
                t - 50
            );
        }
    }
    let stats = rig.pair.picture.stats();
    assert_eq!(stats.sound_underruns.load(Ordering::Relaxed), 0);
    // The pictures loop too: mark 1 again at 450 ms.
    rig.settle(ms(1_100));
    rig.pair.picture.standby();
}

#[test]
fn the_picture_plays_on_while_the_sound_node_is_not_scheduled() {
    let mut rig = rig(clip_with_steps(48_000), false, true);
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    let mut marks = Vec::new();
    for t in (0..500).step_by(10) {
        rig.settle(ms(t));
        let _ = rig.pair.picture.dequeue(ms(t));
        let mark = rig.pair.picture.last_mark();
        if marks.last() != Some(&mark) {
            marks.push(mark);
        }
    }
    assert_eq!(marks, [Some(1), Some(2), Some(3)]);
    assert!(!rig.pair.sound.ring_live());
    rig.pair.picture.standby();
}

#[test]
fn end_of_file_holds_the_last_picture_releases_the_worker_and_writes_silence() {
    let mut rig = rig(clip_with_steps(48_000), false, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..700).step_by(10) {
        rig.settle(ms(t));
        let _ = rig.pair.picture.dequeue(ms(t));
        if t >= 50 {
            let _ = rig.block_at(ms(t));
        }
        rig.pair.picture.service();
    }
    assert_eq!(rig.pair.picture.phase(), MediaPhase::Hold);
    assert_eq!(rig.pair.picture.last_mark(), Some(3));
    assert!(rig.pair.picture.worker_joined());
    assert!(!rig.pair.picture.queues_live());
    assert!(!rig.pair.picture.decoder_held());
    assert_eq!(rig.factory.counts.alive.load(Ordering::Relaxed), 0);
    assert!(rig.pair.picture.dequeue(ms(800)).is_some());
    assert_eq!(rig.pair.picture.last_mark(), Some(3));
    let (written, block) = rig.block_at(ms(800));
    assert!(!written);
    assert!(block.iter().all(|s| *s == 0.0));
    let stats = rig.pair.picture.stats();
    assert_eq!(stats.sound_underruns.load(Ordering::Relaxed), 0);
    rig.pair.picture.standby();
}

#[test]
fn neither_node_allocates_or_frees_while_it_plays() {
    let mut rig = rig(clip_with_steps(48_000), true, true);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    let mut l = vec![0.0f32; BLOCK];
    let mut r = vec![0.0f32; BLOCK];
    for t in (0..600).step_by(10) {
        rig.settle(ms(t));
        let picture = &mut rig.pair.picture;
        let sound = &mut rig.pair.sound;
        let info = allocation_counter::measure(|| {
            let frame = picture.dequeue(ms(t));
            std::mem::forget(frame);
            if t >= 50 {
                sound.fill(ms(t) - LATENCY, &mut [&mut l, &mut r], BLOCK);
            }
        });
        assert_eq!(info.count_total, 0, "allocated at {t} ms");
        assert!(info.count_current >= 0, "freed at {t} ms");
    }
    rig.pair.picture.standby();
}

#[test]
fn standby_holds_no_ring_and_no_decoder() {
    let mut rig = rig(ScriptClip::silent_frames(), true, true);
    assert!(!rig.pair.picture.queues_live());
    assert!(!rig.pair.sound.ring_live());
    assert!(!rig.pair.picture.decoder_held());
    assert_eq!(rig.pair.picture.queue_allocations(), 0);
    rig.attach_sound();
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    assert!(rig.pair.picture.queue_allocations() >= 1);
    rig.pair.picture.standby();
    rig.pair.sound.detach();
    assert_eq!(rig.pair.picture.phase(), MediaPhase::Standby);
    assert!(!rig.pair.picture.queues_live());
    assert!(!rig.pair.sound.ring_live());
    assert!(!rig.pair.picture.decoder_held());
    assert!(rig.pair.picture.worker_joined());
    assert_eq!(rig.factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(rig.pair.picture.cursor(), Duration::ZERO);
}

#[test]
fn an_oversized_side_allocates_no_ring() {
    let mut rig = rig(ScriptClip::oversized_side(), true, true);
    let error = rig.pair.picture.wake(Timestamp::ZERO).unwrap_err();
    assert!(matches!(error, rbs_nodes::MediaNodeError::Invalid(_)));
    assert_eq!(rig.pair.picture.queue_allocations(), 0);
    assert!(!rig.pair.picture.queues_live());
    assert!(!rig.pair.picture.decoder_held());
    assert_eq!(rig.factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(rig.factory.counts.file_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn restart_when_shown_false_seeks_to_the_stored_cursor() {
    let mut rig = rig(ScriptClip::silent_frames(), false, true);
    rig.pair.picture.wake(Timestamp::ZERO).unwrap();
    for t in (0..400).step_by(10) {
        rig.settle(ms(t));
        let _ = rig.pair.picture.dequeue(ms(t));
    }
    assert_eq!(rig.pair.picture.cursor(), Duration::from_millis(200));
    rig.pair.picture.standby();
    assert!(!rig.pair.picture.decoder_held());
    rig.pair.picture.set_restart_when_shown(false);
    *rig.factory.counts.last_seek.lock().unwrap() = None;
    rig.pair.picture.wake(ms(1_000)).unwrap();
    wait_until(|| {
        rig.factory
            .counts
            .last_seek
            .lock()
            .ok()
            .and_then(|slot| *slot)
            == Some(Duration::from_millis(200))
    });
    rig.pair.picture.standby();
}

#[test]
fn loop_can_change_without_replacing_the_node() {
    let mut rig = rig(ScriptClip::silent_frames(), true, true);
    assert!(rig.pair.picture.loops());
    rig.pair.picture.set_loop(false);
    assert!(!rig.pair.picture.loops());
    assert_eq!(rig.pair.picture.phase(), MediaPhase::Standby);
    let _ = LEAD;
}
