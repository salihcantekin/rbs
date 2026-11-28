//! Checks of a track's untrusted frame times (spec `guest-source.md`).
//!
//! The sender stamps each frame with its capture time and derives the RTP time from
//! the same capture time (peer-port.md, "Media path"), so the two must agree. A frame
//! whose capture time disagrees with its RTP time, or does not move forward, is an
//! outlier: it is dropped and counted, and many in a row refuse the peer
//! (`guest_limits`). A frame without a capture time gets one from its RTP time and
//! the last good frame.

/// Largest disagreement between a capture time and its RTP time.
pub const MAX_SKEW_NS: u64 = 20_000_000;
/// Outliers in a row that refuse the peer.
pub const MAX_OUTLIERS_IN_ROW: u32 = 50;

/// What a frame's times came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timed {
    /// Its capture time, checked or derived.
    At(u64),
    /// Dropped: its times disagree, go back, or cannot be derived yet.
    Outlier,
}

/// The time checks of one track.
#[derive(Debug, Clone)]
pub struct TrackTiming {
    clock_rate: u64,
    /// The last good frame's `(rtp time, capture time)`.
    anchor: Option<(u64, u64)>,
    in_row: u32,
    outliers: u64,
}

impl TrackTiming {
    /// A track whose RTP clock runs at `clock_rate` Hz (90 000 video, 48 000 Opus).
    pub fn new(clock_rate: u64) -> Self {
        Self {
            clock_rate: clock_rate.max(1),
            anchor: None,
            in_row: 0,
            outliers: 0,
        }
    }

    /// Checks one frame's times.
    pub fn check(&mut self, rtp_time: u64, capture_ns: Option<u64>) -> Timed {
        let expected = self.anchor.and_then(|(rtp, at)| {
            let ticks = rtp_time.checked_sub(rtp)?;
            let ns = u128::from(ticks) * 1_000_000_000 / u128::from(self.clock_rate);
            at.checked_add(u64::try_from(ns).ok()?)
        });
        let after_anchor = |t: u64| self.anchor.is_none_or(|(_, at)| t > at);
        let time = match (capture_ns, expected) {
            (Some(c), Some(e)) if c.abs_diff(e) <= MAX_SKEW_NS && after_anchor(c) => Some(c),
            (Some(c), None) if self.anchor.is_none() => Some(c),
            (None, Some(e)) if after_anchor(e) => Some(e),
            _ => None,
        };
        match time {
            Some(t) => {
                self.anchor = Some((rtp_time, t));
                self.in_row = 0;
                Timed::At(t)
            }
            None => {
                self.in_row = self.in_row.saturating_add(1);
                self.outliers += 1;
                Timed::Outlier
            }
        }
    }

    /// Too many outliers in a row: the peer is refused with `guest_limits`.
    pub fn refused(&self) -> bool {
        self.in_row >= MAX_OUTLIERS_IN_ROW
    }

    /// Outliers so far.
    pub fn outliers(&self) -> u64 {
        self.outliers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn agreeing_times_pass_and_a_missing_one_is_derived() {
        let mut t = TrackTiming::new(90_000);
        assert_eq!(t.check(9_000, Some(1_000 * MS)), Timed::At(1_000 * MS));
        assert_eq!(t.check(12_000, Some(1_033 * MS)), Timed::At(1_033 * MS));
        // 3000 ticks at 90 kHz: 33.3 ms after the last good frame.
        assert_eq!(t.check(15_000, None), Timed::At(1_033 * MS + 33_333_333));
    }

    #[test]
    fn far_off_backwards_and_unanchored_times_are_outliers() {
        let mut t = TrackTiming::new(48_000);
        assert_eq!(t.check(0, None), Timed::Outlier, "nothing to derive from");
        assert_eq!(t.check(0, Some(500 * MS)), Timed::At(500 * MS));
        // A far-future capture time against its RTP time.
        assert_eq!(t.check(960, Some(90_000 * MS)), Timed::Outlier);
        // The same RTP time with an earlier capture time: not forward.
        assert_eq!(t.check(0, Some(499 * MS)), Timed::Outlier);
        // The next good frame still passes against the last good one.
        assert_eq!(t.check(1_920, Some(540 * MS)), Timed::At(540 * MS));
        // RTP time going back.
        assert_eq!(t.check(960, Some(520 * MS)), Timed::Outlier);
        assert_eq!(t.outliers(), 4);
        assert!(!t.refused());
    }

    #[test]
    fn many_outliers_in_a_row_refuse_the_peer() {
        let mut t = TrackTiming::new(90_000);
        t.check(0, Some(MS));
        for i in 1..u64::from(MAX_OUTLIERS_IN_ROW) {
            t.check(i * 3_000, Some(u64::MAX / 2));
        }
        assert!(!t.refused(), "one fewer is not refused");
        t.check(1_000_000, Some(u64::MAX / 2));
        assert!(t.refused());
    }
}
