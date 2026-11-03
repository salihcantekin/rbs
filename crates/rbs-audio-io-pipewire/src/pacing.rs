//! Timestamps of the packets of one stream: PipeWire cycle times smoothed into runs.
//!
//! PipeWire stamps each graph cycle with the `CLOCK_MONOTONIC` time at which the
//! driver woke up. That time jitters by the scheduler's wake-up latency (tens of
//! microseconds, sometimes a millisecond), while the audio it carries is continuous.
//! Stamping every packet with its raw cycle time would make consecutive reads of one
//! run disagree by that jitter. [`Pacing`] keeps a run's timestamps continuous: each
//! packet starts where the previous one ended, pulled a sixteenth of the way towards
//! its measured time, so the device's drift against the master clock shows as small
//! steps and never accumulates. A packet far from where its run predicts (a skipped
//! cycle, an xrun) starts a new run.
//!
//! Pure logic on numbers; no allocation, safe on the real-time thread.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

/// Fraction of the measured error a packet's timestamp moves by.
const PULL: i128 = 16;

/// Smallest error that starts a new run, whatever the packet size.
const MIN_GAP: Duration = Duration::from_millis(3);

/// Turns measured packet times into the timestamps of continuous runs.
#[derive(Debug, Clone)]
pub(crate) struct Pacing {
    rate: SampleRate,
    /// Where the next packet of the current run starts, if a run is open.
    next: Option<Timestamp>,
    last: Option<Timestamp>,
}

/// The timestamp of one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// Master time of the packet's first frame.
    pub timestamp: Timestamp,
    /// The packet does not continue the previous one.
    pub new_run: bool,
}

impl Pacing {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self {
            rate,
            next: None,
            last: None,
        }
    }

    /// Every stamp will be later than `floor` (the last stamp of an earlier stream on
    /// the same endpoint). A zero floor means none.
    pub(crate) fn with_floor(mut self, floor: Timestamp) -> Self {
        if floor > Timestamp::ZERO {
            self.last = Some(floor);
        }
        self
    }

    /// The next packet starts a new run (audio was lost before it).
    pub(crate) fn break_run(&mut self) {
        self.next = None;
    }

    /// Stamps a packet of `frames` frames measured to start at `measured`, handled at
    /// master time `now`. The result is strictly later than the previous packet's and
    /// never lets the packet end after `now`.
    pub(crate) fn stamp(&mut self, measured: Timestamp, frames: usize, now: Timestamp) -> Stamp {
        let duration = self.rate.duration_of(frames as u64);
        let gap = (duration / 2).max(MIN_GAP);
        let (mut timestamp, new_run) = match self.next {
            Some(expected) => {
                let error = i128::from(measured.as_nanos()) - i128::from(expected.as_nanos());
                if error.unsigned_abs() > gap.as_nanos() {
                    (measured, true)
                } else {
                    let step = error / PULL;
                    let nanos = i128::from(expected.as_nanos()) + step;
                    (
                        Timestamp::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(0)),
                        false,
                    )
                }
            }
            None => (measured, true),
        };
        let latest = now - duration;
        if timestamp > latest {
            timestamp = latest;
        }
        if let Some(last) = self.last
            && timestamp <= last
        {
            timestamp = last + Duration::from_nanos(1);
        }
        self.last = Some(timestamp);
        self.next = Some(timestamp + duration);
        Stamp { timestamp, new_run }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: SampleRate = SampleRate::HZ_48000;
    const FRAMES: usize = 480; // 10 ms

    fn ms(m: u64) -> Timestamp {
        Timestamp::from_nanos(m * 1_000_000)
    }

    #[test]
    fn jitter_is_smoothed_within_a_run() {
        let mut pacing = Pacing::new(RATE);
        let mut previous: Option<Stamp> = None;
        for k in 0..200_u64 {
            // Cycle times jitter by up to 0.9 ms around a 10 ms grid.
            let jitter = (k * 7_919 % 1_800) as i64 * 500 - 450_000;
            let measured = Timestamp::from_nanos(1_000_000_000 + k * 10_000_000)
                .as_nanos()
                .checked_add_signed(jitter)
                .map(Timestamp::from_nanos)
                .unwrap_or(Timestamp::ZERO);
            let now = measured + Duration::from_millis(12);
            let stamp = pacing.stamp(measured, FRAMES, now);
            if let Some(previous) = previous {
                assert!(!stamp.new_run, "packet {k} started a run");
                let expected = previous.timestamp + Duration::from_millis(10);
                let error = stamp.timestamp.as_nanos().abs_diff(expected.as_nanos());
                assert!(error <= 60_000, "packet {k}: step of {error} ns");
            } else {
                assert!(stamp.new_run);
            }
            previous = Some(stamp);
        }
    }

    #[test]
    fn drift_is_followed_without_accumulating() {
        let mut pacing = Pacing::new(RATE);
        let mut stamp = None;
        // The device runs 0.1% slow: each 10 ms packet arrives 10 us later.
        for k in 0..2_000_u64 {
            let measured = Timestamp::from_nanos(k * 10_010_000);
            stamp = Some(pacing.stamp(measured, FRAMES, measured + Duration::from_millis(20)));
        }
        let last = stamp.map_or(0, |s| s.timestamp.as_nanos());
        let error = last.abs_diff(1_999 * 10_010_000);
        assert!(error < 200_000, "accumulated {error} ns");
    }

    #[test]
    fn a_skipped_cycle_starts_a_new_run() {
        let mut pacing = Pacing::new(RATE);
        assert!(pacing.stamp(ms(100), FRAMES, ms(120)).new_run);
        assert!(!pacing.stamp(ms(110), FRAMES, ms(130)).new_run);
        let after_gap = pacing.stamp(ms(130), FRAMES, ms(150));
        assert!(after_gap.new_run);
        assert_eq!(after_gap.timestamp, ms(130));
        pacing.break_run();
        assert!(pacing.stamp(ms(140), FRAMES, ms(160)).new_run);
    }

    #[test]
    fn never_ends_in_the_future_and_never_moves_back() {
        let mut pacing = Pacing::new(RATE);
        let first = pacing.stamp(ms(100), FRAMES, ms(105));
        assert_eq!(first.timestamp, ms(95), "clamped to end at now");
        // A packet measured before the previous one.
        let second = pacing.stamp(ms(50), FRAMES, ms(106));
        assert!(second.timestamp > first.timestamp);
        assert!(second.timestamp + Duration::from_millis(10) <= ms(106));
    }
}
