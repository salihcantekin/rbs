//! The shared session hand-off (spec "Session" invariants 1-3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use bytes::Bytes;
use rbs_core::media::{
    ColorSpace, MemoryKind, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_vcam::{CameraFormat, Handoff, VcamError, VirtualCameraConfig};

const W: u32 = 16;
const H: u32 = 8;

fn config() -> VirtualCameraConfig {
    VirtualCameraConfig::new(
        CameraFormat::new(W, H, FrameRate::FPS_30, PixelFormat::Nv12),
        MemoryKind::System,
    )
}

fn nv12(width: u32, height: u32, data: Bytes, n: u64) -> VideoFrame {
    let format = VideoFormat::new(
        Size { width, height },
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let w = width as usize;
    let planes = [
        PlaneLayout {
            offset: 0,
            stride: w,
        },
        PlaneLayout {
            offset: w * height as usize,
            stride: w,
        },
    ];
    let frame = SystemFrame::new(format, data, &planes).unwrap();
    VideoFrame::system(Timestamp::from_nanos(n), frame, None).with_sequence(n)
}

fn frame(n: u64) -> VideoFrame {
    nv12(W, H, Bytes::from(vec![0u8; (W * H * 3 / 2) as usize]), n)
}

#[test]
fn the_sender_gets_the_newest_frame_and_drops_the_older_ones() {
    let (h, rx) = Handoff::new(config());
    for n in 1..=3 {
        let slot = h.reserve().unwrap();
        h.send(slot, frame(n)).unwrap();
    }
    // A stalled sender: every slot is full, so the node must skip.
    assert!(h.reserve().is_none());
    assert_eq!(rx.take_newest().unwrap().sequence(), 3);
    let s = h.stats();
    assert_eq!(s.replaced(), 2);
    assert_eq!(s.consumer_busy(), 1);
    assert!(h.reserve().is_some());
}

#[test]
fn a_dropped_token_frees_its_slot() {
    let (h, _rx) = Handoff::new(config());
    let tokens: Vec<_> = (0..3).map(|_| h.reserve().unwrap()).collect();
    assert!(h.reserve().is_none());
    drop(tokens);
    assert!(h.reserve().is_some());
}

#[test]
fn frames_that_break_the_config_are_refused_and_free_their_slot() {
    let (h, _rx) = Handoff::new(config());
    let slot = h.reserve().unwrap();
    let wrong = nv12(
        W * 2,
        H,
        Bytes::from(vec![0u8; (W * 2 * H * 3 / 2) as usize]),
        1,
    );
    assert!(matches!(
        h.send(slot, wrong),
        Err(VcamError::InvalidFrame(_))
    ));
    let (other, _orx) = Handoff::new(config());
    let foreign = other.reserve().unwrap();
    assert!(matches!(
        h.send(foreign, frame(2)),
        Err(VcamError::InvalidFrame(_))
    ));
    assert_eq!(h.stats().refused(), 2);
    let tokens: Vec<_> = (0..3).map(|_| h.reserve()).collect();
    assert!(tokens.iter().all(Option::is_some));
}

#[test]
fn reserve_and_send_do_not_allocate_in_steady_state() {
    let (mut h, rx) = Handoff::new(config());
    h.set_sender(std::thread::current());
    // Frames are made before measuring: their buffers come from the producer's pool.
    let mut frames: Vec<VideoFrame> = (1..=100).map(frame).collect();
    // Warm up once.
    let slot = h.reserve().unwrap();
    h.send(slot, frames.pop().unwrap()).unwrap();
    drop(rx.take_newest());
    let mut held = Vec::with_capacity(100);
    let info = allocation_counter::measure(|| {
        for _ in 0..50 {
            let slot = h.reserve().unwrap();
            h.send(slot, frames.pop().unwrap()).unwrap();
            // The consumer side runs elsewhere; keep its frame so its drop is not
            // measured here.
            held.push(rx.take_newest().unwrap());
        }
    });
    assert_eq!(info.count_total, 0, "{info:?}");
}

#[test]
fn send_wakes_the_sender_thread() {
    let (mut h, rx) = Handoff::new(config());
    let sender = std::thread::spawn(move || {
        // Bounded: a hang guard, not a timing assumption.
        for _ in 0..10_000 {
            if let Some(f) = rx.take_newest() {
                return f.sequence();
            }
            rx.wait(std::time::Duration::from_millis(100));
        }
        0
    });
    h.set_sender(sender.thread().clone());
    let slot = h.reserve().unwrap();
    h.send(slot, frame(7)).unwrap();
    assert_eq!(sender.join().unwrap(), 7);
}
