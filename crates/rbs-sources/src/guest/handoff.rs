//! The guest source's hand-off from its worker to its nodes (spec `guest-source.md`,
//! worker step 9). A hot path (`hot-paths.md`): the render thread's side of
//! [`GuestPictures`] and the sound node never wait, allocate or free.
//!
//! - **Pictures.** The worker publishes decoded pictures into three slots; the
//!   picture node takes the newest one whose stamp is due (ADR-0109 decision 8's
//!   rules). Pictures it skips or is done with go into return slots that the worker
//!   empties, so a system-memory picture's bytes are never freed on the render thread.
//!   A native picture's surface goes back to the decoder through the importer, which
//!   keeps its lease until the GPU work that reads it has finished.
//! - **Sound** goes through the browser source's sound feed and node (the same ring
//!   and `InputAligner`), stamped on the master clock already, so the feed's edge
//!   clock maps one-to-one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rbs_core::media::VideoFrame;
use rbs_core::time::{Clock, Timestamp};

use super::worker::GuestSink;
use crate::browser_sound::{BrowserSound, BrowserSoundFeed};

/// Pictures waiting for the render thread.
pub const PICTURE_SLOTS: usize = 3;
/// Pictures the render thread gave back and the worker has not dropped yet.
const RETURN_SLOTS: usize = PICTURE_SLOTS + 1;

#[derive(Default)]
struct Slots {
    waiting: [Option<VideoFrame>; PICTURE_SLOTS],
    returned: [Option<VideoFrame>; RETURN_SLOTS],
}

impl Slots {
    // The frame comes back by value: boxing it would allocate on the render thread.
    #[allow(clippy::result_large_err)]
    fn give_back(&mut self, frame: VideoFrame) -> Result<(), VideoFrame> {
        match self.returned.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(frame);
                Ok(())
            }
            None => Err(frame),
        }
    }
}

/// The three-slot picture queue between a guest source's worker and its node.
#[derive(Default)]
pub struct GuestPictures {
    slots: Mutex<Slots>,
    /// Set while a picture waits: the render thread's idle path is one load.
    fresh: AtomicBool,
    /// Pictures the worker dropped because the render thread did not take them.
    skipped: AtomicU64,
}

impl GuestPictures {
    /// Empty slots.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Worker thread: adds a picture due at its timestamp. When all slots are taken
    /// the oldest waiting picture is dropped (here, on the worker). Returned pictures
    /// are dropped here too.
    pub fn publish(&self, frame: VideoFrame) {
        let mut drop_here: [Option<VideoFrame>; RETURN_SLOTS + 1] = Default::default();
        if let Ok(mut slots) = self.slots.lock() {
            for (out, slot) in drop_here.iter_mut().zip(slots.returned.iter_mut()) {
                *out = slot.take();
            }
            let free = slots.waiting.iter().position(Option::is_none);
            let at = free.unwrap_or_else(|| {
                let oldest = oldest(&slots.waiting).unwrap_or(0);
                drop_here[RETURN_SLOTS] = slots.waiting[oldest].take();
                self.skipped.fetch_add(1, Ordering::Relaxed);
                oldest
            });
            slots.waiting[at] = Some(frame);
            self.fresh.store(true, Ordering::Release);
        }
        drop(drop_here);
    }

    /// Worker or control thread: drops every waiting and returned picture.
    pub fn clear(&self) {
        let taken = self.slots.lock().map(|mut s| std::mem::take(&mut *s));
        self.fresh.store(false, Ordering::Release);
        drop(taken);
    }

    /// Pictures dropped before the render thread took them.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// Render thread: the newest waiting picture whose stamp is not after `now`. Older
    /// due pictures go to the return slots. Never waits: a slot the worker holds right
    /// now is tried again next tick.
    pub fn take_due(&self, now: Timestamp) -> Option<VideoFrame> {
        if !self.fresh.load(Ordering::Acquire) {
            return None;
        }
        let mut slots = self.slots.try_lock().ok()?;
        let newest = slots
            .waiting
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|f| (i, f.timestamp())))
            .filter(|(_, t)| *t <= now)
            .max_by_key(|(_, t)| *t)
            .map(|(i, _)| i)?;
        let taken = slots.waiting[newest].take();
        let cutoff = taken.as_ref().map(VideoFrame::timestamp);
        for i in 0..PICTURE_SLOTS {
            let older = slots.waiting[i]
                .as_ref()
                .is_some_and(|f| cutoff.is_some_and(|c| f.timestamp() <= c));
            if older && slots.returned.iter().any(Option::is_none) {
                if let Some(f) = slots.waiting[i].take() {
                    // A free return slot was checked just above.
                    let _ = slots.give_back(f);
                }
            }
        }
        let any = slots.waiting.iter().any(Option::is_some);
        self.fresh.store(any, Ordering::Release);
        taken
    }

    /// Render thread: a picture the node is done with (its pixels uploaded). `Err`
    /// gives it back when no return slot is free; the node keeps it and tries again.
    #[allow(clippy::result_large_err)] // By value: boxing would allocate here.
    pub fn give_back(&self, frame: VideoFrame) -> Result<(), VideoFrame> {
        match self.slots.try_lock() {
            Ok(mut slots) => slots.give_back(frame),
            Err(_) => Err(frame),
        }
    }
}

fn oldest(waiting: &[Option<VideoFrame>]) -> Option<usize> {
    waiting
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|f| (i, f.timestamp())))
        .min_by_key(|(_, t)| *t)
        .map(|(i, _)| i)
}

/// The worker's sink for one guest source: pictures into [`GuestPictures`], sound into
/// the sound feed.
pub struct NodeSink {
    pictures: Arc<GuestPictures>,
    feed: BrowserSoundFeed,
    clock: Arc<dyn Clock>,
}

impl NodeSink {
    /// A sink onto `pictures` and the sound node of `sound`.
    pub fn new(pictures: Arc<GuestPictures>, sound: Arc<BrowserSound>) -> Self {
        let clock = sound.clock();
        Self {
            pictures,
            feed: BrowserSoundFeed::new(sound),
            clock,
        }
    }
}

impl GuestSink for NodeSink {
    fn picture(&mut self, frame: VideoFrame) {
        self.pictures.publish(frame);
    }

    fn sound(&mut self, at: Timestamp, samples: &[f32], channels: u16) {
        // The stamps are master time already: the edge maps master to master.
        self.feed.observe(self.clock.now().as_nanos());
        self.feed.packet(at.as_nanos(), 48_000, channels, samples);
        self.feed.pump();
    }

    fn sound_restart(&mut self) {
        // The feed starts a new run of blocks at the next packet whose time does not
        // continue the last one; nothing else to do.
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use rbs_core::media::{
        Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat,
    };

    use super::*;

    fn frame(at_ms: u64) -> VideoFrame {
        let format = VideoFormat::new(Size::new(2, 2), PixelFormat::Rgba8, ColorSpace::SRGB);
        let planes = [PlaneLayout {
            offset: 0,
            stride: 8,
        }];
        let f = SystemFrame::new(format, Bytes::from(vec![0u8; 16]), &planes).unwrap();
        VideoFrame::system(Timestamp::from_nanos(at_ms * 1_000_000), f, None)
    }

    fn ms(n: u64) -> Timestamp {
        Timestamp::from_nanos(n * 1_000_000)
    }

    #[test]
    fn the_newest_due_picture_is_taken_and_older_ones_go_back() {
        let q = GuestPictures::new();
        assert!(q.take_due(ms(100)).is_none());
        for t in [10, 20, 30] {
            q.publish(frame(t));
        }
        assert!(q.take_due(ms(5)).is_none(), "nothing due yet");
        let f = q.take_due(ms(25)).unwrap();
        assert_eq!(f.timestamp(), ms(20));
        // 10 went to the return slots; 30 still waits.
        let f2 = q.take_due(ms(40)).unwrap();
        assert_eq!(f2.timestamp(), ms(30));
        assert!(q.take_due(ms(50)).is_none());
        q.give_back(f).unwrap();
        q.give_back(f2).unwrap();
        // The next publish drops what was returned, on the worker.
        q.publish(frame(60));
        assert_eq!(q.take_due(ms(60)).unwrap().timestamp(), ms(60));
    }

    #[test]
    fn a_full_queue_drops_its_oldest_picture_on_the_worker() {
        let q = GuestPictures::new();
        for t in [10, 20, 30, 40] {
            q.publish(frame(t));
        }
        assert_eq!(q.skipped(), 1);
        assert!(q.take_due(ms(15)).is_none(), "10 was dropped");
        assert_eq!(q.take_due(ms(45)).unwrap().timestamp(), ms(40));
        q.clear();
        assert!(q.take_due(ms(100)).is_none());
    }
}
