//! ADR-0108 decision 10. The fake adapter is `rbs-media-decode-test`. No FFmpeg and no sleep.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_core::time::{TestClock, Timestamp};
use rbs_media_decode_test::{ScriptClip, ScriptFactory};
use rbs_nodes::{MediaPhase, MediaSourceNode, media_descriptor};

fn node(
    clip: ScriptClip,
    loop_playback: bool,
    restart: bool,
) -> (MediaSourceNode, Arc<ScriptFactory>) {
    let factory = Arc::new(ScriptFactory::new(clip));
    let clock = Arc::new(TestClock::new());
    let node = MediaSourceNode::new(
        factory.clone(),
        std::path::PathBuf::from(r"C:\clips\silent.mp4"),
        loop_playback,
        restart,
        clock,
        48_000,
        2,
        480,
    );
    (node, factory)
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

#[test]
fn the_audio_pin_is_always_present() {
    let descriptor = media_descriptor();
    let kinds: Vec<_> = descriptor.outputs.iter().map(|pin| pin.kind).collect();
    assert_eq!(
        kinds,
        vec![
            rbs_graph::node::LinkKind::Video,
            rbs_graph::node::LinkKind::Audio
        ]
    );
}

#[test]
fn a_file_with_no_audio_stream_emits_silence() {
    let (mut node, factory) = node(ScriptClip::silent_frames(), true, true);
    assert_eq!(factory.counts.file_bytes.load(Ordering::Relaxed), 0);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.enqueued() >= 1);
    let mut audio = vec![1.0f32; 8];
    node.write_audio(&mut audio);
    assert_eq!(audio, vec![0.0; 8]);
    assert!(node.stats().audio_underruns.load(Ordering::Relaxed) >= 1);
    node.standby();
}

#[test]
fn video_drops_and_repeats_on_the_test_clock() {
    let (mut node, _) = node(ScriptClip::silent_frames(), true, true);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.enqueued() >= 3);
    let first = node.dequeue(Timestamp::ZERO);
    assert!(first.is_some());
    assert_eq!(node.last_mark(), Some(1));
    let repeated = node.dequeue(Timestamp::from_nanos(50_000_000));
    assert!(repeated.is_some());
    assert_eq!(node.last_mark(), Some(1));
    assert!(node.stats().repeated_video.load(Ordering::Relaxed) >= 1);
    let later = node.dequeue(Timestamp::from_nanos(250_000_000));
    assert!(later.is_some());
    assert_eq!(node.last_mark(), Some(3));
    assert!(node.stats().dropped_video.load(Ordering::Relaxed) >= 1);
    node.standby();
}

#[test]
fn end_of_file_holds_the_last_frame_releases_the_worker_and_emits_silence() {
    let (mut node, factory) = node(ScriptClip::silent_frames(), false, true);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.worker_ended());
    wait_until(|| node.enqueued() >= 3);
    let _ = node.dequeue(Timestamp::from_nanos(250_000_000));
    assert_eq!(node.last_mark(), Some(3));
    node.service();
    assert_eq!(node.phase(), MediaPhase::Hold);
    assert!(node.worker_joined());
    assert!(!node.queues_live());
    assert!(!node.decoder_held());
    assert_eq!(factory.counts.alive.load(Ordering::Relaxed), 0);
    let held = node.dequeue(Timestamp::from_nanos(400_000_000));
    assert!(held.is_some());
    assert_eq!(node.last_mark(), Some(3));
    let mut audio = vec![1.0f32; 4];
    node.write_audio(&mut audio);
    assert_eq!(audio, vec![0.0; 4]);
    node.standby();
}

#[test]
fn dequeue_does_not_allocate() {
    let (mut node, _) = node(ScriptClip::silent_frames(), true, true);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.enqueued() >= 3);
    let _ = node.dequeue(Timestamp::ZERO);
    let mut audio = vec![0.0f32; 16];
    let info = allocation_counter::measure(|| {
        let _ = node.dequeue(Timestamp::from_nanos(50_000_000));
        node.write_audio(&mut audio);
    });
    assert_eq!(info.count_total, 0, "process dequeue allocated");
    node.standby();
}

#[test]
fn standby_holds_no_queue_and_no_decoder() {
    let (mut node, factory) = node(ScriptClip::silent_frames(), true, true);
    assert!(!node.queues_live());
    assert!(!node.decoder_held());
    assert_eq!(node.queue_allocations(), 0);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.decoder_held() || node.enqueued() >= 1);
    assert!(node.queue_allocations() >= 1);
    node.standby();
    assert_eq!(node.phase(), MediaPhase::Standby);
    assert!(!node.queues_live());
    assert!(!node.decoder_held());
    assert!(node.worker_joined());
    assert_eq!(factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(node.cursor(), Duration::ZERO);
}

#[test]
fn an_oversized_side_allocates_no_queue() {
    let (mut node, factory) = node(ScriptClip::oversized_side(), true, true);
    let error = node.wake(Timestamp::ZERO).unwrap_err();
    assert!(matches!(error, rbs_nodes::MediaNodeError::Invalid(_)));
    assert_eq!(node.queue_allocations(), 0);
    assert!(!node.queues_live());
    assert!(!node.decoder_held());
    assert_eq!(factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(factory.counts.file_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn restart_when_shown_false_seeks_to_the_stored_cursor() {
    let (mut node, factory) = node(ScriptClip::silent_frames(), false, true);
    node.wake(Timestamp::ZERO).unwrap();
    wait_until(|| node.enqueued() >= 3);
    let _ = node.dequeue(Timestamp::from_nanos(250_000_000));
    assert_eq!(node.cursor(), Duration::from_millis(200));
    node.standby();
    assert!(!node.decoder_held());
    node.set_restart_when_shown(false);
    *factory.counts.last_seek.lock().unwrap() = None;
    node.wake(Timestamp::from_nanos(1_000_000_000)).unwrap();
    wait_until(|| {
        factory.counts.last_seek.lock().ok().and_then(|slot| *slot)
            == Some(Duration::from_millis(200))
    });
    node.standby();
}

#[test]
fn loop_can_change_without_replacing_the_node() {
    let (mut node, _) = node(ScriptClip::silent_frames(), true, true);
    assert!(node.loops());
    node.set_loop(false);
    assert!(!node.loops());
    assert_eq!(node.phase(), MediaPhase::Standby);
}
