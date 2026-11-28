//! The engine's queue of received frames (peer-port.md, "Media path"): a bounded FIFO
//! that drops its oldest frame when full. A drop is counted with its bytes, and the
//! next frame of the same track that leaves the queue is marked not contiguous; a lost
//! video frame also asks for a keyframe. One gap flag per track, so a lost video frame
//! never marks a sound frame instead.

use std::collections::VecDeque;

use crate::media::{Received, Track};

fn slot(track: Track) -> usize {
    match track {
        Track::Video => 0,
        Track::Audio => 1,
    }
}

/// The queue between str0m's events and the owner thread's hand-off.
#[derive(Debug, Default)]
pub(crate) struct ReceiveQueue {
    frames: VecDeque<Received>,
    gap: [bool; 2],
    lost_video: bool,
    pub(crate) overflow: u64,
    pub(crate) overflow_bytes: u64,
}

impl ReceiveQueue {
    /// Adds a frame; drops the oldest when `capacity` frames wait.
    pub(crate) fn push(&mut self, frame: Received, capacity: usize) {
        if self.frames.len() >= capacity
            && let Some(lost) = self.frames.pop_front()
        {
            self.overflow += 1;
            self.overflow_bytes += lost.data.len() as u64;
            self.gap[slot(lost.track)] = true;
            if lost.track == Track::Video {
                self.lost_video = true;
            }
        }
        self.frames.push_back(frame);
    }

    /// The oldest frame, marked not contiguous when a frame of its track was lost
    /// before it.
    pub(crate) fn pop(&mut self) -> Option<Received> {
        let mut frame = self.frames.pop_front()?;
        if std::mem::take(&mut self.gap[slot(frame.track)]) {
            frame.contiguous = false;
        }
        Some(frame)
    }

    /// True once after a video frame was lost: ask the sharer for a keyframe.
    pub(crate) fn take_lost_video(&mut self) -> bool {
        std::mem::take(&mut self.lost_video)
    }
}

/// One gap flag per track for the guest's ring (the owner thread's side).
#[derive(Debug, Default)]
pub(crate) struct TrackGaps([bool; 2]);

impl TrackGaps {
    /// A frame of `track` was lost.
    pub(crate) fn lose(&mut self, track: Track) {
        self.0[slot(track)] = true;
    }

    /// Marks `frame` not contiguous when a frame of its track was lost before it.
    pub(crate) fn apply(&mut self, frame: &mut Received) {
        if std::mem::take(&mut self.0[slot(frame.track)]) {
            frame.contiguous = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    fn frame(track: Track, n: u8) -> Received {
        Received {
            track,
            data: Arc::from(vec![n; 10]),
            rtp_time: u64::from(n),
            capture_nanos: None,
            keyframe: false,
            contiguous: true,
            arrived: Instant::now(),
        }
    }

    #[test]
    fn a_dropped_frame_marks_the_next_frame_of_its_own_track() {
        let mut q = ReceiveQueue::default();
        q.push(frame(Track::Video, 1), 3);
        q.push(frame(Track::Audio, 2), 3);
        q.push(frame(Track::Audio, 3), 3);
        q.push(frame(Track::Video, 4), 3); // drops video 1
        assert_eq!((q.overflow, q.overflow_bytes), (1, 10));
        assert!(q.take_lost_video());
        assert!(!q.take_lost_video(), "once");
        let out: Vec<(u64, bool)> = std::iter::from_fn(|| q.pop())
            .map(|f| (f.rtp_time, f.contiguous))
            .collect();
        assert_eq!(out, [(2, true), (3, true), (4, false)]);
    }

    #[test]
    fn a_lost_sound_frame_does_not_ask_for_a_keyframe() {
        let mut q = ReceiveQueue::default();
        q.push(frame(Track::Audio, 1), 1);
        q.push(frame(Track::Video, 2), 1);
        q.push(frame(Track::Audio, 3), 1);
        assert!(q.take_lost_video(), "video 2 was dropped by audio 3");
        assert_eq!(q.pop().map(|f| f.contiguous), Some(false));

        let mut gaps = TrackGaps::default();
        gaps.lose(Track::Video);
        let mut a = frame(Track::Audio, 5);
        gaps.apply(&mut a);
        assert!(a.contiguous, "a video loss never marks sound");
        let mut v = frame(Track::Video, 6);
        gaps.apply(&mut v);
        assert!(!v.contiguous);
    }
}
