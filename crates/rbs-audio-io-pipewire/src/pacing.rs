//! Timestamps of the packets of one stream: PipeWire cycle times smoothed into runs.
//!
//! PipeWire stamps each graph cycle with the `CLOCK_MONOTONIC` time at which the
//! driver woke up. That time jitters by the scheduler's wake-up latency (tens of
//! microseconds, sometimes a millisecond), while the audio it carries is continuous.
//! Stamping every packet with its raw cycle time would make consecutive reads of one
//! run disagree by that jitter. [`Pacing`] keeps a run's timestamps continuous: each
//! packet starts where the previous one ended, pulled a sixteenth of the way towards
//! its measured time, so the device's drift against the master clock shows as small
//! steps (at most [`MAX_STEP`]) and never accumulates.
//!
//! A packet starts a new run when its measured time is far from where the run
//! predicts (a skipped cycle, a quantum or rate change), or when it had to be moved
//! more than [`MAX_STEP`] to satisfy the two hard rules: a packet never starts before
//! the previous one ended (no audio overlaps in time) and never ends after the master
//! time at which it is handled. A packet that cannot satisfy both is not stamped; the
//! caller counts it as lost.
//!
//! Pure logic on numbers; no allocation, safe on the real-time thread.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

/// Fraction of the measured error a packet's timestamp moves by.
const PULL: u128 = 16;

/// Largest move of a packet that still continues its run. Half the conformance
/// suite's default tolerance (2 ms).
pub(crate) const MAX_STEP: Duration = Duration::from_millis(1);

/// Largest measured error that is smoothed rather than starting a new run: errors up
/// to this move a packet by at most [`MAX_STEP`].
const MAX_ERROR: Duration = Duration::from_millis(16);

/// How far after the master time at which it is handled a packet may end. Cycle
/// wake-up jitter moves the handling time by a fraction of a millisecond; without
/// this slack a continuous run would lose packets to it. Well inside the port's
/// timestamp tolerance (2 ms).
pub(crate) const FUTURE_SLACK: Duration = Duration::from_micros(500);

/// Smallest measured error that starts a new run, whatever the packet size, so
/// wake-up jitter on small quanta does not break runs.
const MIN_GAP: Duration = Duration::from_millis(3);

/// Turns measured packet times into the timestamps of continuous runs.
#[derive(Debug, Clone)]
pub(crate) struct Pacing {
    rate: SampleRate,
    /// Where the next packet of the current run starts, if a run is open.
    next: Option<Timestamp>,
    /// Where the previous packet ended (also across streams, see `with_floor`).
    end: Option<Timestamp>,
}

/// The timestamp of one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// Master time of the packet's first frame.
    pub timestamp: Timestamp,
    /// The packet does not continue the previous one.
    pub new_run: bool,
}

fn distance(a: Timestamp, b: Timestamp) -> Duration {
    Duration::from_nanos(a.as_nanos().abs_diff(b.as_nanos()))
}

impl Pacing {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self {
            rate,
            next: None,
            end: None,
        }
    }

    /// No packet will start before `floor` (where the last packet of an earlier stream
    /// on the same endpoint ended). A zero floor means none.
    pub(crate) fn with_floor(mut self, floor: Timestamp) -> Self {
        if floor > Timestamp::ZERO {
            self.end = Some(floor);
        }
        self
    }

    /// Where the previous packet ended, if any.
    pub(crate) fn end(&self) -> Option<Timestamp> {
        self.end
    }

    /// The next packet starts a new run (audio was lost before it).
    pub(crate) fn break_run(&mut self) {
        self.next = None;
    }

    /// Stamps a packet of `frames` frames measured to start at `measured`, handled at
    /// master time `now`. The packet starts at or after the previous packet's end and
    /// ends at or before `now` plus [`FUTURE_SLACK`]. Returns `None`, and breaks the run, when no such
    /// placement exists; the caller drops the packet and counts it as lost.
    pub(crate) fn stamp(
        &mut self,
        measured: Timestamp,
        frames: usize,
        now: Timestamp,
    ) -> Option<Stamp> {
        let duration = self.rate.duration_of(frames as u64);
        let gap = (duration / 2).clamp(MIN_GAP, MAX_ERROR);
        let wanted = match self.next {
            Some(expected) if distance(measured, expected) <= gap => {
                let step = (distance(measured, expected).as_nanos() / PULL) as u64;
                let step = Duration::from_nanos(step);
                if measured > expected {
                    expected + step
                } else {
                    expected - step
                }
            }
            _ => measured,
        };
        let mut timestamp = wanted;
        if let Some(end) = self.end
            && timestamp < end
        {
            timestamp = end;
        }
        let latest = (now + FUTURE_SLACK)
            .as_nanos()
            .checked_sub(duration.as_nanos() as u64)?;
        let latest = Timestamp::from_nanos(latest);
        if timestamp > latest {
            timestamp = latest;
        }
        if self.end.is_some_and(|end| timestamp < end) {
            // Cannot both follow the previous packet and end by `now`.
            self.next = None;
            return None;
        }
        let new_run = match self.next {
            Some(expected) => {
                distance(wanted, expected) > MAX_STEP || distance(timestamp, expected) > MAX_STEP
            }
            None => true,
        };
        let end = timestamp + duration;
        self.end = Some(end);
        self.next = Some(end);
        Some(Stamp { timestamp, new_run })
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

    fn stamp(pacing: &mut Pacing, measured: Timestamp, frames: usize, now: Timestamp) -> Stamp {
        match pacing.stamp(measured, frames, now) {
            Some(stamp) => stamp,
            None => panic!("{frames} frames at {measured:?} (now {now:?}) not stamped"),
        }
    }

    /// Checks the hard rules over a sequence of stamps: no overlap, never in the
    /// future, continued packets within `MAX_STEP` of the previous end.
    struct Checker {
        end: Option<Timestamp>,
    }

    impl Checker {
        fn check(&mut self, stamp: Stamp, frames: usize, now: Timestamp) {
            let duration = RATE.duration_of(frames as u64);
            assert!(
                stamp.timestamp + duration <= now + FUTURE_SLACK,
                "{stamp:?} ends after {now:?}"
            );
            if let Some(end) = self.end {
                assert!(stamp.timestamp >= end, "{stamp:?} overlaps {end:?}");
                if !stamp.new_run {
                    assert!(
                        distance(stamp.timestamp, end) <= MAX_STEP,
                        "{stamp:?} continues a run ending at {end:?}"
                    );
                }
            }
            self.end = Some(stamp.timestamp + duration);
        }
    }

    #[test]
    fn jitter_is_smoothed_within_a_run() {
        let mut pacing = Pacing::new(RATE);
        let mut checker = Checker { end: None };
        for k in 0..200_u64 {
            // Cycle times jitter by up to 0.9 ms around a 10 ms grid.
            let jitter = (k * 7_919 % 1_800) as i64 * 500 - 450_000;
            let measured = (1_000_000_000 + k * 10_000_000)
                .checked_add_signed(jitter)
                .map_or(Timestamp::ZERO, Timestamp::from_nanos);
            let now = measured + Duration::from_millis(12);
            let s = stamp(&mut pacing, measured, FRAMES, now);
            assert_eq!(s.new_run, k == 0, "packet {k}");
            checker.check(s, FRAMES, now);
        }
    }

    #[test]
    fn drift_is_followed_without_accumulating() {
        let mut pacing = Pacing::new(RATE);
        let mut last = None;
        // The device runs 0.1% slow: each 10 ms packet arrives 10 us later.
        for k in 0..2_000_u64 {
            let measured = Timestamp::from_nanos(k * 10_010_000);
            let s = stamp(
                &mut pacing,
                measured,
                FRAMES,
                measured + Duration::from_millis(20),
            );
            assert_eq!(s.new_run, k == 0);
            last = Some(s.timestamp);
        }
        let last = last.map_or(0, Timestamp::as_nanos);
        let error = last.abs_diff(1_999 * 10_010_000);
        assert!(error < 200_000, "accumulated {error} ns");
    }

    #[test]
    fn a_skipped_cycle_starts_a_new_run() {
        let mut pacing = Pacing::new(RATE);
        assert!(stamp(&mut pacing, ms(100), FRAMES, ms(120)).new_run);
        assert!(!stamp(&mut pacing, ms(110), FRAMES, ms(130)).new_run);
        let after_gap = stamp(&mut pacing, ms(130), FRAMES, ms(150));
        assert!(after_gap.new_run);
        assert_eq!(after_gap.timestamp, ms(130));
        pacing.break_run();
        assert!(stamp(&mut pacing, ms(140), FRAMES, ms(160)).new_run);
    }

    #[test]
    fn quantum_changes_never_overlap_or_run_ahead() {
        // Cycles of 256 frames, then 2048, then 64, then 1024, each measured from the
        // cycle time, which jumps when the quantum changes; packets handled 0.3 ms
        // after their cycle started.
        let mut pacing = Pacing::new(RATE);
        let mut checker = Checker { end: None };
        let mut cycle = 1_000_000_000_u64;
        let mut runs = 0;
        let mut lost = 0;
        for (quantum, count) in [(256_usize, 50), (2_048, 10), (64, 200), (1_024, 20)] {
            let period = RATE.duration_of(quantum as u64).as_nanos() as u64;
            for _ in 0..count {
                cycle += period;
                let now = Timestamp::from_nanos(cycle + 300_000);
                // PipeWire's cycle time: the end of the packet.
                let measured = Timestamp::from_nanos(cycle - period);
                match pacing.stamp(measured, quantum, now) {
                    Some(s) => {
                        runs += usize::from(s.new_run);
                        checker.check(s, quantum, now);
                    }
                    None => lost += 1,
                }
            }
        }
        assert!(runs <= 4, "{runs} runs for 3 quantum changes");
        assert!(lost <= 3, "{lost} packets lost");
    }

    #[test]
    fn a_jump_inside_a_run_starts_a_new_one_without_overlap() {
        // The delay reported for a packet jumps by 8 ms (a rate change adds resampler
        // delay): the packet is placed at the run's end, not over the previous one,
        // and flagged as a new run.
        let mut pacing = Pacing::new(RATE);
        let mut checker = Checker { end: None };
        for k in 0..10_u64 {
            let now = ms(110 + k * 10);
            let s = stamp(&mut pacing, ms(100 + k * 10), FRAMES, now);
            checker.check(s, FRAMES, now);
        }
        let now = ms(215);
        let s = stamp(&mut pacing, ms(192), FRAMES, now);
        checker.check(s, FRAMES, now);
        assert!(s.new_run);
        assert_eq!(s.timestamp, ms(200));
        // A packet that would have to end 4 ms after it is handled is lost.
        assert_eq!(pacing.stamp(ms(210), FRAMES, ms(216)), None);
        assert!(stamp(&mut pacing, ms(220), FRAMES, ms(231)).new_run);
    }

    #[test]
    fn packets_longer_than_the_clock_are_lost_not_stamped_in_the_future() {
        let mut pacing = Pacing::new(RATE);
        assert_eq!(pacing.stamp(ms(0), FRAMES, ms(8)), None);
        let s = stamp(&mut pacing, ms(5), FRAMES, ms(15));
        assert!(s.new_run);
        assert_eq!(s.timestamp, ms(5));
    }

    #[test]
    fn floor_keeps_restarts_later() {
        let mut pacing = Pacing::new(RATE).with_floor(ms(100));
        let s = stamp(&mut pacing, ms(95), FRAMES, ms(120));
        assert_eq!(s.timestamp, ms(100));
        assert_eq!(pacing.end(), Some(ms(110)));
        // A packet that cannot start after the floor and end by now is lost.
        let mut pacing = Pacing::new(RATE).with_floor(ms(100));
        assert_eq!(pacing.stamp(ms(95), FRAMES, ms(105)), None);
    }
}
