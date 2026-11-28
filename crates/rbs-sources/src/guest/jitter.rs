//! The jitter buffer of one track (ADR-0139 decision 11; spec `guest-source.md`).
//!
//! A frame waits until its mapped master time plus the target. Sound leaves in capture
//! order; video leaves in the order it came (decode order), and a frame whose capture
//! time does not move forward is refused, since H.264 from RBS has no B-frames. A video
//! track starts and restarts at a keyframe: after any lost or dropped frame (a gap, a
//! frame too late, one dropped for room) it drops frames until the next keyframe and
//! asks for one, at most once a second, so the decoder never misses a reference.

use std::collections::VecDeque;
use std::time::Duration;

use rbs_core::time::Timestamp;

/// The default target delay.
pub const DEFAULT_TARGET: Duration = Duration::from_millis(150);
/// The smallest and largest target the setting allows.
pub const TARGET_RANGE: (Duration, Duration) =
    (Duration::from_millis(40), Duration::from_millis(1000));
/// Most sender time the buffer holds.
pub const MAX_HOLD: Duration = Duration::from_secs(2);
/// Most frames the buffer holds: 2 s of 120 fps video, or 1.5 s of 2.5 ms Opus
/// packets (shorter packets are bounded by count first).
pub const MAX_FRAMES: usize = 600;
/// Most frame bytes the buffer holds: 2 s at the 50 Mbit/s rate limit.
pub const MAX_BYTES: usize = 12_500_000;
/// Keyframe requests are at least this far apart.
pub const KEYFRAME_REQUEST_GAP: Duration = Duration::from_secs(1);

/// A frame in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued<T> {
    /// The sender's capture time.
    pub capture_ns: u64,
    /// When it leaves: its mapped master time plus the target.
    pub release: Timestamp,
    /// A video keyframe.
    pub keyframe: bool,
    /// Its size in bytes.
    pub bytes: usize,
    /// The frame.
    pub payload: T,
}

/// What became of a pushed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// It waits for its release.
    Held,
    /// More than one target past its release, or behind a frame already released or
    /// (video) already held.
    TooLate,
    /// A video frame after a gap, before the next keyframe.
    WaitingForKeyframe,
}

/// Frames dropped, by cause.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterCounts {
    /// Too late.
    pub late: u64,
    /// Waiting for a keyframe.
    pub before_keyframe: u64,
    /// Dropped for room.
    pub overflow: u64,
}

/// The buffer of one track.
#[derive(Debug)]
pub struct JitterBuffer<T> {
    target: Duration,
    video: bool,
    queue: VecDeque<Queued<T>>,
    need_keyframe: bool,
    last_request: Option<Timestamp>,
    released_through: Option<u64>,
    bytes: usize,
    counts: JitterCounts,
}

impl<T> JitterBuffer<T> {
    /// A buffer with `target` delay (clamped to [`TARGET_RANGE`]); `video` gates on
    /// keyframes. A video buffer waits for a keyframe first.
    pub fn new(target: Duration, video: bool) -> Self {
        Self {
            target: target.clamp(TARGET_RANGE.0, TARGET_RANGE.1),
            video,
            queue: VecDeque::new(),
            need_keyframe: video,
            last_request: None,
            released_through: None,
            bytes: 0,
            counts: JitterCounts::default(),
        }
    }

    /// The target delay.
    pub fn target(&self) -> Duration {
        self.target
    }

    /// Frames dropped so far.
    pub fn counts(&self) -> JitterCounts {
        self.counts
    }

    /// Frames waiting.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// No frame waits.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Adds a frame of `bytes` whose capture time maps to `mapped` on the master
    /// clock. `contiguous` is false when frames were lost before it.
    #[allow(clippy::too_many_arguments)] // The frame's fields, then the time.
    pub fn push(
        &mut self,
        capture_ns: u64,
        mapped: Timestamp,
        keyframe: bool,
        contiguous: bool,
        bytes: usize,
        payload: T,
        now: Timestamp,
    ) -> Pushed {
        let release = mapped + self.target;
        let held_after = self.video
            && self
                .queue
                .back()
                .is_some_and(|q| capture_ns <= q.capture_ns);
        let behind = held_after || self.released_through.is_some_and(|r| capture_ns <= r);
        if behind || now > release + self.target {
            self.counts.late += 1;
            self.lose_video();
            return Pushed::TooLate;
        }
        if !contiguous {
            self.lose_video();
        }
        if self.need_keyframe && !keyframe {
            self.counts.before_keyframe += 1;
            return Pushed::WaitingForKeyframe;
        }
        if keyframe {
            self.need_keyframe = false;
        }
        // Video keeps its arrival (decode) order; sound goes by capture time.
        let at = if self.video {
            self.queue.len()
        } else {
            self.queue
                .iter()
                .rposition(|q| q.capture_ns <= capture_ns)
                .map_or(0, |i| i + 1)
        };
        self.queue.insert(
            at,
            Queued {
                capture_ns,
                release,
                keyframe,
                bytes,
                payload,
            },
        );
        self.bytes += bytes;
        self.bound();
        Pushed::Held
    }

    /// The oldest frame whose release time has come.
    pub fn pop_due(&mut self, now: Timestamp) -> Option<Queued<T>> {
        if self.queue.front().is_none_or(|q| q.release > now) {
            return None;
        }
        let q = self.queue.pop_front()?;
        self.bytes -= q.bytes;
        self.released_through = Some(q.capture_ns);
        Some(q)
    }

    /// When the next frame is due.
    pub fn next_release(&self) -> Option<Timestamp> {
        self.queue.front().map(|q| q.release)
    }

    /// True when a keyframe should be asked for now (a video track waiting for one,
    /// at most once per [`KEYFRAME_REQUEST_GAP`]). Asking is the caller's.
    pub fn keyframe_request(&mut self, now: Timestamp) -> bool {
        if !self.need_keyframe {
            return false;
        }
        if self
            .last_request
            .is_some_and(|last| now.saturating_since(last) < KEYFRAME_REQUEST_GAP)
        {
            return false;
        }
        self.last_request = Some(now);
        true
    }

    /// Starts over at the next keyframe (the decoder was made again, or the track was
    /// shown again). Waiting frames are dropped.
    pub fn restart(&mut self) {
        self.counts.overflow += self.queue.len() as u64;
        self.queue.clear();
        self.bytes = 0;
        self.need_keyframe = self.video;
        self.last_request = None;
    }

    /// A frame of this track was lost before it reached the buffer (an outlier the
    /// time checks dropped): a video track waits for the next keyframe.
    pub fn mark_lost(&mut self) {
        self.lose_video();
    }

    fn lose_video(&mut self) {
        if self.video {
            self.need_keyframe = true;
        }
    }

    /// Keeps at most [`MAX_HOLD`] of sender time, [`MAX_FRAMES`] and [`MAX_BYTES`]. A
    /// video track that drops a frame for room drops up to the next waiting keyframe,
    /// or waits for a new one.
    fn bound(&mut self) {
        let hold = u64::try_from(MAX_HOLD.as_nanos()).unwrap_or(u64::MAX);
        loop {
            let (Some(old), Some(new)) = (self.queue.front(), self.queue.back()) else {
                return;
            };
            let span = new.capture_ns.saturating_sub(old.capture_ns);
            if span <= hold && self.queue.len() <= MAX_FRAMES && self.bytes <= MAX_BYTES {
                return;
            }
            self.drop_front();
            if self.video {
                while self.queue.front().is_some_and(|q| !q.keyframe) {
                    self.drop_front();
                }
                if self.queue.is_empty() {
                    self.need_keyframe = true;
                }
            }
        }
    }

    fn drop_front(&mut self) {
        if let Some(q) = self.queue.pop_front() {
            self.bytes -= q.bytes;
            self.counts.overflow += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn t(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * MS)
    }

    /// Capture time maps to master time one to one in these tests.
    fn push(b: &mut JitterBuffer<u32>, ms: u64, key: bool, cont: bool, now: u64) -> Pushed {
        b.push(ms * MS, t(ms), key, cont, 100, ms as u32, t(now))
    }

    fn drain(b: &mut JitterBuffer<u32>, now: u64) -> Vec<u32> {
        std::iter::from_fn(|| b.pop_due(t(now)).map(|q| q.payload)).collect()
    }

    #[test]
    fn sound_leaves_in_capture_order_at_the_target() {
        let mut b = JitterBuffer::new(DEFAULT_TARGET, false);
        assert_eq!(push(&mut b, 20, false, true, 25), Pushed::Held);
        assert_eq!(push(&mut b, 0, false, true, 25), Pushed::Held, "reordered");
        assert_eq!(push(&mut b, 40, false, true, 45), Pushed::Held);
        assert!(drain(&mut b, 149).is_empty());
        assert_eq!(drain(&mut b, 150), [0]);
        assert_eq!(b.next_release(), Some(t(170)));
        assert_eq!(drain(&mut b, 200), [20, 40]);
    }

    #[test]
    fn late_frames_are_dropped() {
        let mut b = JitterBuffer::new(DEFAULT_TARGET, false);
        assert_eq!(push(&mut b, 0, false, true, 301), Pushed::TooLate);
        assert_eq!(push(&mut b, 100, false, true, 110), Pushed::Held);
        assert_eq!(drain(&mut b, 250), [100]);
        assert_eq!(
            push(&mut b, 90, false, true, 250),
            Pushed::TooLate,
            "behind"
        );
        assert_eq!(b.counts().late, 2);
    }

    #[test]
    fn video_starts_and_restarts_at_a_keyframe_and_asks_once_a_second() {
        let mut b = JitterBuffer::new(DEFAULT_TARGET, true);
        assert!(b.keyframe_request(t(0)), "a new video track asks at once");
        assert_eq!(push(&mut b, 0, false, true, 0), Pushed::WaitingForKeyframe);
        assert!(!b.keyframe_request(t(500)), "not again within a second");
        assert!(b.keyframe_request(t(1000)));
        assert_eq!(push(&mut b, 33, true, true, 33), Pushed::Held);
        assert_eq!(push(&mut b, 66, false, true, 66), Pushed::Held);
        assert!(!b.keyframe_request(t(3000)), "nothing to ask for");
        // A gap: drop until the next keyframe.
        assert_eq!(
            push(&mut b, 133, false, false, 133),
            Pushed::WaitingForKeyframe
        );
        assert!(b.keyframe_request(t(3000)));
        assert_eq!(push(&mut b, 166, true, true, 166), Pushed::Held);
        assert_eq!(drain(&mut b, 1000), [33, 66, 166]);
        assert_eq!(b.counts().before_keyframe, 2);
    }

    #[test]
    fn video_keeps_its_order_and_any_drop_waits_for_a_keyframe() {
        let mut v = JitterBuffer::new(DEFAULT_TARGET, true);
        push(&mut v, 0, true, true, 0);
        push(&mut v, 33, false, true, 33);
        assert_eq!(
            push(&mut v, 20, false, true, 40),
            Pushed::TooLate,
            "not forward"
        );
        assert_eq!(
            push(&mut v, 66, false, true, 66),
            Pushed::WaitingForKeyframe
        );
        // A frame too late breaks the chain too.
        let mut w = JitterBuffer::new(DEFAULT_TARGET, true);
        push(&mut w, 0, true, true, 0);
        assert_eq!(push(&mut w, 33, false, true, 400), Pushed::TooLate);
        assert_eq!(
            push(&mut w, 66, false, true, 70),
            Pushed::WaitingForKeyframe
        );
        assert!(w.keyframe_request(t(70)));
    }

    #[test]
    fn frames_and_bytes_are_bounded() {
        let mut b = JitterBuffer::new(DEFAULT_TARGET, false);
        for i in 0..(MAX_FRAMES as u64 + 10) {
            b.push(i * 1000, t(0), false, true, 10, 0, t(0));
        }
        assert_eq!(b.len(), MAX_FRAMES);
        let mut big = JitterBuffer::new(DEFAULT_TARGET, false);
        for i in 0..5_u64 {
            big.push(i * MS, t(0), false, true, MAX_BYTES / 4, 0, t(0));
        }
        assert_eq!(big.len(), 4);
    }

    #[test]
    fn it_holds_at_most_two_seconds() {
        let mut b = JitterBuffer::new(Duration::from_millis(1000), false);
        for ms in (0..=2500).step_by(100) {
            push(&mut b, ms, false, true, 0);
        }
        assert_eq!(b.len(), 21, "2000 ms of frames, every 100 ms");
        assert_eq!(b.counts().overflow, 5);

        // Video drops up to the next keyframe it holds.
        let mut v = JitterBuffer::new(Duration::from_millis(1000), true);
        for ms in (0..=2100).step_by(100) {
            push(&mut v, ms, ms % 1000 == 0, true, 0);
        }
        assert_eq!(v.pop_due(t(5000)).map(|q| q.capture_ns), Some(1000 * MS));
    }

    #[test]
    fn the_target_is_clamped_and_restart_waits_for_a_keyframe() {
        assert_eq!(
            JitterBuffer::<u32>::new(Duration::ZERO, false).target(),
            TARGET_RANGE.0
        );
        let mut v = JitterBuffer::new(DEFAULT_TARGET, true);
        push(&mut v, 0, true, true, 0);
        v.restart();
        assert!(v.is_empty());
        assert_eq!(
            push(&mut v, 33, false, true, 33),
            Pushed::WaitingForKeyframe
        );
    }
}
