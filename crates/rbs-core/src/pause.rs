//! Pause windows of a recording (ADR-0124 decisions 2 to 4).
//!
//! A recording that is paused and resumed keeps one continuous file: what lies in a
//! pause window is left out and everything after it is stamped earlier by the time
//! left out. [`PauseWindow`] holds the current window as one record that the control
//! thread writes and the media nodes of the recording read (export, audio encoder,
//! output), so every file of the recording is cut at the same instants.
//!
//! The record is a sequence lock over fixed fields: no allocation, no lock, and a
//! reader never sees the fields of two different writes together. There is one
//! writer (the control thread); readers retry a bounded number of times and keep
//! their last good [`Window`] when a write is in progress.

use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::timing::SampleRate;

/// Nanoseconds per second, for sample arithmetic.
const NANOS_PER_SEC: u128 = 1_000_000_000;

/// How often a reader tries before keeping its last good window.
const READ_TRIES: usize = 4;

/// One pause window as the nodes see it. Times are master-clock nanoseconds.
///
/// Media at a time `t` is kept and shifted by [`Window::before`] when `t < pause_at`,
/// left out when `pause_at <= t < resume_at`, and kept and shifted by
/// `before + (resume_at - pause_at)` when `t >= resume_at`. `resume_at` is
/// `u64::MAX` while paused. A window with `pause_at == u64::MAX` cuts nothing (no
/// pause yet, or a pause cancelled before it began).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Counts the writes; a node compares it to see a new window.
    pub epoch: u64,
    /// Time left out by every earlier window, in nanoseconds.
    pub before: u64,
    /// First time left out.
    pub pause_at: u64,
    /// First time kept again; `u64::MAX` while paused.
    pub resume_at: u64,
}

/// Where media at one time goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Kept, stamped earlier by this many nanoseconds.
    Kept(u64),
    /// Left out (inside the window).
    Cut,
}

/// How a block of audio samples starting at one time is cut by a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCut {
    /// Samples `0..head` are before the window, kept and shifted by `head_shift`.
    pub head: usize,
    /// Shift of the head, in nanoseconds.
    pub head_shift: u64,
    /// Samples `tail..frames` are after the window, kept and shifted by `tail_shift`.
    /// `head <= tail`; samples `head..tail` are left out.
    pub tail: usize,
    /// Shift of the tail, in nanoseconds.
    pub tail_shift: u64,
}

impl BlockCut {
    /// True when the block passes whole with one shift.
    pub fn is_whole(&self, frames: usize) -> bool {
        (self.head == frames) || (self.tail == 0)
    }
}

impl Window {
    /// The window of a recording that was never paused.
    pub const NONE: Self = Self {
        epoch: 0,
        before: 0,
        pause_at: u64::MAX,
        resume_at: u64::MAX,
    };

    /// Time left out once this window is over (or so far, while it is open).
    pub fn length(&self) -> u64 {
        if self.pause_at == u64::MAX {
            0
        } else {
            self.resume_at.saturating_sub(self.pause_at)
        }
    }

    /// True while the window has no end yet.
    pub fn is_open(&self) -> bool {
        self.pause_at != u64::MAX && self.resume_at == u64::MAX
    }

    /// Time left out by this and every earlier window, once this one is over.
    pub fn total(&self) -> u64 {
        self.before.saturating_add(self.length())
    }

    /// Where media at time `t` goes.
    pub fn place(&self, t: u64) -> Place {
        if t < self.pause_at {
            Place::Kept(self.before)
        } else if t < self.resume_at {
            Place::Cut
        } else {
            Place::Kept(self.total())
        }
    }

    /// True once nothing at or after `t` can fall inside this window: it is over.
    pub fn is_over_at(&self, t: u64) -> bool {
        self.pause_at == u64::MAX || t >= self.resume_at
    }

    /// How a block of `frames` samples at `rate`, whose first sample is at `start`,
    /// is cut. Sample `n` is at `start + floor(n * 1e9 / rate)`, as
    /// [`SampleRate::duration_of`] computes it.
    pub fn cut_block(&self, start: u64, frames: usize, rate: SampleRate) -> BlockCut {
        let index = |t: u64| first_at_or_after(start, t, frames, rate);
        BlockCut {
            head: index(self.pause_at),
            head_shift: self.before,
            tail: index(self.resume_at),
            tail_shift: self.total(),
        }
    }
}

/// The index of the first of `frames` samples (from `start`, at `rate`) whose time
/// is at or after `t`; `frames` when none is.
fn first_at_or_after(start: u64, t: u64, frames: usize, rate: SampleRate) -> usize {
    if t <= start {
        return 0;
    }
    if t == u64::MAX {
        return frames;
    }
    // floor(n * 1e9 / r) < d  <=>  n * 1e9 < d * r  <=>  n < d * r / 1e9.
    let d = u128::from(t - start);
    let n = (d * u128::from(rate.hz())).div_ceil(NANOS_PER_SEC);
    usize::try_from(n).map_or(frames, |n| n.min(frames))
}

/// The pause window of one recording, shared by the control thread (the one writer)
/// and the recording's nodes.
#[derive(Debug)]
pub struct PauseWindow {
    seq: AtomicU64,
    before: AtomicU64,
    pause_at: AtomicU64,
    resume_at: AtomicU64,
}

impl Default for PauseWindow {
    fn default() -> Self {
        Self {
            seq: AtomicU64::new(0),
            before: AtomicU64::new(0),
            pause_at: AtomicU64::new(u64::MAX),
            resume_at: AtomicU64::new(u64::MAX),
        }
    }
}

/// Why the control thread could not change the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseError {
    /// A pause is open (asked or running) and was not resumed.
    AlreadyPaused,
    /// No pause is open.
    NotPaused,
    /// The time is before the last window's end: windows follow each other.
    TooEarly,
}

impl std::fmt::Display for PauseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AlreadyPaused => "the recording is already paused",
            Self::NotPaused => "the recording is not paused",
            Self::TooEarly => "a pause cannot begin before the last one ended",
        })
    }
}

impl std::error::Error for PauseError {}

impl PauseWindow {
    /// Reads the current window. `None` only while a write is in progress on every
    /// try; the caller then keeps its last good window for this tick. Never blocks.
    pub fn read(&self) -> Option<Window> {
        for _ in 0..READ_TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let before = self.before.load(Ordering::Relaxed);
            let pause_at = self.pause_at.load(Ordering::Relaxed);
            let resume_at = self.resume_at.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(Window {
                    epoch: s1 / 2,
                    before,
                    pause_at,
                    resume_at,
                });
            }
        }
        None
    }

    /// Writes a whole window. One writer only (the control thread).
    fn write(&self, before: u64, pause_at: u64, resume_at: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.before.store(before, Ordering::Relaxed);
        self.pause_at.store(pause_at, Ordering::Relaxed);
        self.resume_at.store(resume_at, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// The window as the writer last wrote it (control thread).
    fn current(&self) -> Window {
        Window {
            epoch: self.seq.load(Ordering::Relaxed) / 2,
            before: self.before.load(Ordering::Relaxed),
            pause_at: self.pause_at.load(Ordering::Relaxed),
            resume_at: self.resume_at.load(Ordering::Relaxed),
        }
    }

    /// Opens a pause at `at` (a render tick time chosen by the caller far enough
    /// ahead that no node has passed it). Control thread only.
    pub fn pause(&self, at: u64) -> Result<Window, PauseError> {
        let w = self.current();
        if w.is_open() {
            return Err(PauseError::AlreadyPaused);
        }
        if w.pause_at != u64::MAX && at < w.resume_at {
            return Err(PauseError::TooEarly);
        }
        self.write(w.total(), at, u64::MAX);
        Ok(self.current())
    }

    /// Ends the open pause at `at`. When `at` is at or before the pause's start, the
    /// pause is cancelled: nothing is left out. Control thread only.
    pub fn resume(&self, at: u64) -> Result<Window, PauseError> {
        let w = self.current();
        if !w.is_open() {
            return Err(PauseError::NotPaused);
        }
        if at <= w.pause_at {
            self.write(w.before, u64::MAX, u64::MAX);
        } else {
            self.write(w.before, w.pause_at, at);
        }
        Ok(self.current())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    const R48: SampleRate = SampleRate::HZ_48000;
    /// One 48 kHz sample, rounded down.
    const SAMPLE: u64 = 20_833;

    #[test]
    fn a_recording_never_paused_keeps_everything_unshifted() {
        let w = PauseWindow::default().read().unwrap();
        assert_eq!(w, Window::NONE);
        assert_eq!(w.place(0), Place::Kept(0));
        assert_eq!(w.place(u64::MAX - 1), Place::Kept(0));
        assert!(w.is_over_at(0));
    }

    #[test]
    fn a_window_cuts_its_span_and_shifts_what_follows() {
        let p = PauseWindow::default();
        p.pause(1_000).unwrap();
        let open = p.read().unwrap();
        assert!(open.is_open());
        assert_eq!(open.place(999), Place::Kept(0));
        assert_eq!(open.place(1_000), Place::Cut);
        assert_eq!(open.place(u64::MAX - 1), Place::Cut);
        p.resume(1_500).unwrap();
        let w = p.read().unwrap();
        assert_eq!(w.place(1_499), Place::Cut);
        assert_eq!(w.place(1_500), Place::Kept(500));
        assert!(w.epoch > open.epoch);
        assert!(!w.is_over_at(1_499) && w.is_over_at(1_500));
    }

    #[test]
    fn windows_add_up() {
        let p = PauseWindow::default();
        p.pause(1_000).unwrap();
        p.resume(1_500).unwrap();
        p.pause(3_000).unwrap();
        p.resume(3_200).unwrap();
        let w = p.read().unwrap();
        assert_eq!(w.before, 500);
        assert_eq!(w.place(2_999), Place::Kept(500));
        assert_eq!(w.place(3_100), Place::Cut);
        assert_eq!(w.place(3_200), Place::Kept(700));
        assert_eq!(w.total(), 700);
    }

    #[test]
    fn a_resume_before_the_pause_began_cancels_it() {
        let p = PauseWindow::default();
        p.pause(1_000).unwrap();
        p.resume(1_500).unwrap();
        p.pause(3_000).unwrap();
        p.resume(3_000).unwrap();
        let w = p.read().unwrap();
        assert_eq!(
            w.place(5_000),
            Place::Kept(500),
            "only the first window counts"
        );
        assert_eq!(w.length(), 0);
        // After a cancel the next pause may begin at any time after the last real one.
        p.pause(2_000).unwrap();
        assert_eq!(p.read().unwrap().before, 500);
    }

    #[test]
    fn the_writer_refuses_what_does_not_follow() {
        let p = PauseWindow::default();
        assert_eq!(p.resume(10), Err(PauseError::NotPaused));
        p.pause(1_000).unwrap();
        assert_eq!(p.pause(2_000), Err(PauseError::AlreadyPaused));
        p.resume(1_500).unwrap();
        assert_eq!(p.pause(1_200), Err(PauseError::TooEarly));
        assert!(p.pause(1_500).is_ok());
    }

    #[test]
    fn a_block_is_split_at_the_first_sample_of_each_instant() {
        let p = PauseWindow::default();
        // Pause 10 samples into the block, resume 30 samples in.
        p.pause(10 * SAMPLE + 1).unwrap();
        p.resume(30 * SAMPLE + 1).unwrap();
        let w = p.read().unwrap();
        let cut = w.cut_block(0, 480, R48);
        // Sample n is at floor(n * 1e9 / 48000): sample 10 at 208_333 ns, the first at
        // or after 208_331; sample 30 at 625_000, the first at or after 624_991.
        assert_eq!(cut.head, 10);
        assert_eq!(cut.tail, 30);
        assert_eq!(cut.head_shift, 0);
        assert_eq!(cut.tail_shift, 20 * SAMPLE);
        assert!(!cut.is_whole(480));
    }

    #[test]
    fn blocks_away_from_a_window_pass_whole() {
        let p = PauseWindow::default();
        p.pause(1_000_000_000).unwrap();
        p.resume(2_000_000_000).unwrap();
        let w = p.read().unwrap();
        let before = w.cut_block(0, 480, R48);
        assert_eq!((before.head, before.head_shift), (480, 0));
        assert!(before.is_whole(480));
        let inside = w.cut_block(1_500_000_000, 480, R48);
        assert_eq!((inside.head, inside.tail), (0, 480));
        let after = w.cut_block(2_000_000_000, 480, R48);
        assert_eq!((after.tail, after.tail_shift), (0, 1_000_000_000));
        assert!(after.is_whole(480));
    }

    #[test]
    fn the_samples_left_out_match_the_window_within_one_sample() {
        // Over many block starts and window edges, the samples left out equal the
        // window's length in samples, give or take one (the edges' rounding).
        let rate = SampleRate::new(44_100).unwrap();
        for k in 0..200u64 {
            let pause_at = 1_000_000_000 + k * 7_919;
            let resume_at = pause_at + 333_333_333 + k * 104_729;
            let p = PauseWindow::default();
            p.pause(pause_at).unwrap();
            p.resume(resume_at).unwrap();
            let w = p.read().unwrap();
            let mut left_out = 0u64;
            let mut n = 0u64;
            loop {
                let start = 900_000_000 + rate.duration_of(n).as_nanos() as u64;
                if start > resume_at + 20_000_000 {
                    break;
                }
                // Blocks of 441 samples on the graph's sample grid.
                let cut = w.cut_block(start, 441, rate);
                left_out += (cut.tail - cut.head) as u64;
                n += 441;
            }
            let expected = u128::from(resume_at - pause_at) * 44_100 / NANOS_PER_SEC;
            let diff = (u128::from(left_out)).abs_diff(expected);
            assert!(
                diff <= 1,
                "window {k}: left out {left_out}, expected {expected}"
            );
        }
    }

    #[test]
    fn reads_never_see_two_writes_mixed() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let p = Arc::new(PauseWindow::default());
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let p = Arc::clone(&p);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(w) = p.read() {
                        // Every write keeps `before + length` a multiple of 1_000 and
                        // `pause_at` 1_000 past a multiple of 10_000.
                        if w.pause_at != u64::MAX {
                            assert_eq!(w.pause_at % 10_000, 1_000, "{w:?}");
                            assert_eq!(w.before % 1_000, 0, "{w:?}");
                            if !w.is_open() {
                                assert_eq!(w.resume_at - w.pause_at, 1_000, "{w:?}");
                            }
                        }
                        seen += 1;
                    }
                }
                seen
            })
        };
        for i in 0..20_000u64 {
            p.pause(i * 10_000 + 1_000).unwrap();
            p.resume(i * 10_000 + 2_000).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(p.read().unwrap().total(), 20_000 * 1_000);
    }

    #[test]
    fn reading_does_not_allocate() {
        let p = PauseWindow::default();
        p.pause(1_000).unwrap();
        let counted = allocation_counter::measure(|| {
            for _ in 0..1_000 {
                let w = p.read().unwrap();
                std::hint::black_box(w.cut_block(900, 480, R48));
                std::hint::black_box(w.place(1_200));
            }
        });
        assert_eq!(counted.count_total, 0);
    }
}
