//! The clock edge of an instant from an OS clock: OS time to master time (ADR-0004).
//!
//! Used by video capture adapters (contract: invariant 2, "Master time, at capture", in
//! `docs/specs/capture-port.md`) and by input adapters (ADR-0165, `docs/specs/input-port.md`).
//! Moved here from `rbs-capture` by ADR-0165 so that one port crate does not depend on
//! another; `rbs_capture::edge::EdgeClock` re-exports it. A frame or an input event is an
//! instant, so this does not subtract a packet duration the way the audio edge does.

use std::time::Duration;

use super::{DriftEstimator, Observation};
use crate::time::{ClockMapping, Timestamp};

/// Converts the OS capture time of a frame to master time.
///
/// The adapter feeds it `(OS time, master time)` pairs read back to back with
/// [`observe`](Self::observe), and converts each frame's capture time with
/// [`to_master`](Self::to_master), which stores the delivered stamp.
/// [`map`](Self::map) is that conversion without storing, so a frame that is not
/// published leaves the floor where it was; [`deliver`](Self::deliver) stores the
/// preview when the frame is published. A frame with no capture time is stamped as
/// arrival ([`deliver_arrival`](Self::deliver_arrival), or [`map_arrival`](Self::map_arrival)
/// when it may still be dropped) and is not fed to the estimator.
/// [`resume`](Self::resume) sets the floor from an earlier delivered stamp and does
/// not invent a drift estimate. The mapping is refreshed when the drift estimator
/// completes a bucket, so it changes at most that often. The mapped instant is clamped
/// to the master time at delivery, then kept strictly increasing by a 1 ns floor.
/// Clamping is the only future bound: a source time far from arrival is still mapped.
///
/// Fixed size, no allocation: safe on the OS callback thread.
#[derive(Debug, Clone)]
pub struct EdgeClock {
    drift: DriftEstimator,
    mapping: Option<ClockMapping>,
    buckets: usize,
    last: Option<Timestamp>,
}

impl Default for EdgeClock {
    fn default() -> Self {
        Self::new(Duration::from_secs(1))
    }
}

impl EdgeClock {
    /// A clock edge averaging observations over `bucket` of OS time.
    pub fn new(bucket: Duration) -> Self {
        Self {
            drift: DriftEstimator::new(bucket),
            mapping: None,
            buckets: 0,
            last: None,
        }
    }

    /// Records that `source_nanos` (OS clock) and `master` describe the same instant.
    pub fn observe(&mut self, source_nanos: u64, master: Timestamp) {
        let restarted = self.drift.observe(source_nanos, master) == Observation::Restarted;
        if restarted || self.mapping.is_none() || self.drift.buckets() != self.buckets {
            self.mapping = self.drift.mapping();
            self.buckets = self.drift.buckets();
        }
    }

    /// The master time of a frame captured at OS time `source_nanos`, given the master
    /// time `now` at which it is delivered.
    ///
    /// The OS timestamp is mapped, then clamped with `min(mapped, now)`, then raised
    /// with `max(previous + 1 ns, mapped)` against the previous delivered stamp. Before
    /// the first observation the mapped instant is `now`, and the same clamp and floor
    /// apply. The floor wins when the previous stamp is already `now`: the result is
    /// then one nanosecond past `now`, so delivery stays strictly increasing.
    ///
    /// Stores the result as the last delivered stamp. [`map`](Self::map) is the same
    /// conversion when the frame may still be dropped.
    pub fn to_master(&mut self, source_nanos: u64, now: Timestamp) -> Timestamp {
        let time = self.map(source_nanos, now);
        self.deliver(time);
        time
    }

    /// [`to_master`](Self::to_master) without storing the result.
    ///
    /// The next call still floors against the last delivered stamp. A dropped frame
    /// is mapped here so the monotonic floor does not move.
    pub fn map(&self, source_nanos: u64, now: Timestamp) -> Timestamp {
        let mapped = self
            .mapping
            .map_or(now, |mapping| mapping.to_master(source_nanos));
        self.clamp_floor(mapped, now)
    }

    /// [`map`](Self::map) for a frame with no OS capture time.
    ///
    /// The mapped instant is `now`. Does not record an observation.
    pub fn map_arrival(&self, now: Timestamp) -> Timestamp {
        self.clamp_floor(now, now)
    }

    /// Delivers a frame with no OS capture time and stores the stamp.
    ///
    /// The mapped instant is `now`, then the same clamp and floor as
    /// [`to_master`](Self::to_master). Does not record an observation.
    pub fn deliver_arrival(&mut self, now: Timestamp) -> Timestamp {
        let time = self.map_arrival(now);
        self.deliver(time);
        time
    }

    /// Records `delivered` as the last stamp without mapping or observing.
    ///
    /// The publish path calls this with a stamp from [`map`](Self::map) or
    /// [`map_arrival`](Self::map_arrival). A dropped preview never calls it, so the
    /// floor stays on the last published stamp.
    pub fn deliver(&mut self, delivered: Timestamp) {
        self.last = Some(delivered);
    }

    /// Sets the monotonic floor to `last` without recording an observation.
    ///
    /// A new producer calls this with the session's last delivered stamp so later
    /// stamps keep increasing across `stop` and `start`. The drift estimate is
    /// untouched: a fresh clock stays empty, and this does not invent one.
    pub fn resume(&mut self, last: Timestamp) {
        self.last = Some(last);
    }

    /// The last delivered stamp, if one has been stored or resumed.
    pub fn last(&self) -> Option<Timestamp> {
        self.last
    }

    /// Forgets the drift estimate (a new OS stream after `start`), keeping results
    /// increasing across it.
    pub fn restart(&mut self) {
        self.drift.reset();
        self.mapping = None;
        self.buckets = 0;
    }

    /// Clamps `mapped` to `now`, then applies the 1 ns floor against the last
    /// delivered stamp. Does not store.
    fn clamp_floor(&self, mapped: Timestamp, now: Timestamp) -> Timestamp {
        let mapped = mapped.min(now);
        match self.last {
            Some(last) => mapped.max(last + Duration::from_nanos(1)),
            None => mapped,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::time::{Clock, TestClock, Timestamp};

    use super::*;

    #[test]
    fn delivers_now_before_the_first_observation() {
        let clock = TestClock::new();
        clock.advance(Duration::from_millis(40));
        let now = clock.now();
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        assert_eq!(edge.to_master(0, now), now);
    }

    #[test]
    fn mapped_time_behind_previous_stamp_becomes_previous_plus_one_nanosecond() {
        let clock = TestClock::new();
        clock.set(Timestamp::from_nanos(1_000_000_000));
        let now = clock.now();
        let mut edge = EdgeClock::new(Duration::from_secs(1));
        edge.observe(now.as_nanos(), now);

        let captured = now.as_nanos() - 50_000;
        let first = edge.to_master(captured, now);
        assert_eq!(first, Timestamp::from_nanos(captured));

        let earlier = captured - 80_000;
        let second = edge.to_master(earlier, now);
        assert_eq!(second, first + Duration::from_nanos(1));
    }

    #[test]
    fn mapped_time_past_now_is_clamped_to_now() {
        let clock = TestClock::new();
        clock.set(Timestamp::from_nanos(2_000_000_000));
        let now = clock.now();
        let mut edge = EdgeClock::new(Duration::from_secs(1));
        edge.observe(now.as_nanos(), now);

        // Already behind `now`, so the 1 ns floor does not decide this delivery.
        let first = edge.to_master(now.as_nanos() - 20_000_000, now);
        assert!(first < now);

        let ahead = edge.to_master(now.as_nanos() + 5_000_000_000, now);
        assert_eq!(ahead, now);
    }

    /// The previous stamp is already `now`, so `min(mapped, now)` and the 1 ns floor
    /// disagree. The floor wins: the stamp is `now + 1 ns`.
    #[test]
    fn monotonic_floor_wins_when_previous_stamp_is_already_now() {
        let clock = TestClock::new();
        clock.set(Timestamp::from_nanos(2_000_000_000));
        let now = clock.now();
        let mut edge = EdgeClock::new(Duration::from_secs(1));
        edge.observe(now.as_nanos(), now);

        let first = edge.to_master(now.as_nanos(), now);
        assert_eq!(first, now);

        let second = edge.to_master(now.as_nanos() + 5_000_000_000, now);
        assert_eq!(second, first + Duration::from_nanos(1));
        assert!(second > now);
    }

    #[test]
    fn source_clock_jump_beyond_one_second_is_mapped_and_clamped() {
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        let clock = TestClock::new();
        let origin = 10_000_000_000;
        clock.set(Timestamp::from_nanos(origin));
        // OS time tracks master time, so the fitted line is the identity.
        for step in 0..=30 {
            let t = origin + step * 10_000_000;
            clock.set(Timestamp::from_nanos(t));
            edge.observe(t, clock.now());
        }
        let now = clock.now();

        // Five seconds behind arrival. Mapped and kept, not replaced by arrival.
        let jumped_back = now.as_nanos() - 5_000_000_000;
        let back = edge.to_master(jumped_back, now);
        assert_eq!(back, Timestamp::from_nanos(jumped_back));
        assert!(now.as_nanos() - back.as_nanos() > 1_000_000_000);

        // Five seconds ahead of arrival. Clamped to `now`.
        let forward = edge.to_master(now.as_nanos() + 5_000_000_000, now);
        assert_eq!(forward, now);
    }

    #[test]
    fn restart_forgets_the_estimate_and_keeps_stamps_increasing() {
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(2));
        let now = clock.now();
        edge.observe(now.as_nanos(), now);
        let first = edge.to_master(now.as_nanos() - 1_000_000, now);
        assert_eq!(first, Timestamp::from_nanos(now.as_nanos() - 1_000_000));

        edge.restart();
        // The same OS time would map 1 ms earlier if the estimate had survived.
        let after = edge.to_master(now.as_nanos() - 1_000_000, now);
        assert_eq!(after, now);
        assert!(after > first);
    }

    #[test]
    fn resume_sets_the_floor_without_inventing_an_estimate() {
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        let last = Timestamp::from_nanos(4_000_000_000);
        edge.resume(last);
        let now = Timestamp::from_nanos(4_100_000_000);
        // No observation: a source time is arrival. An invented identity line would
        // have mapped `last` back to itself.
        assert_eq!(edge.map(last.as_nanos(), now), now);
        assert_eq!(edge.last(), Some(last));

        let delivered = edge.to_master(last.as_nanos(), now);
        assert_eq!(delivered, now);

        let mut next = EdgeClock::new(Duration::from_millis(100));
        next.resume(delivered);
        assert_eq!(
            next.to_master(0, delivered),
            delivered + Duration::from_nanos(1)
        );
    }

    #[test]
    fn map_does_not_advance_the_delivered_floor() {
        let mut edge = EdgeClock::new(Duration::from_secs(1));
        let now = Timestamp::from_nanos(2_000_000_000);
        edge.observe(now.as_nanos(), now);
        let first = edge.to_master(now.as_nanos() - 20_000_000, now);
        let preview = edge.map(now.as_nanos() - 80_000_000, now);
        assert_eq!(preview, first + Duration::from_nanos(1));
        assert_eq!(edge.last(), Some(first));

        edge.deliver(preview);
        assert_eq!(edge.last(), Some(preview));
        let second = edge.to_master(now.as_nanos() - 80_000_000, now);
        assert_eq!(second, preview + Duration::from_nanos(1));
    }

    #[test]
    fn missing_presentation_time_is_arrival_and_does_not_observe() {
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        let now = Timestamp::from_nanos(3_000_000_000);
        assert_eq!(edge.deliver_arrival(now), now);
        assert_eq!(edge.last(), Some(now));

        // A real observation of identity would map a source 5 ms behind `later`
        // to that source. Arrival still means the estimator was not fed.
        let later = Timestamp::from_nanos(now.as_nanos() + 25_000_000);
        assert_eq!(edge.map(later.as_nanos() - 5_000_000, later), later);
        assert_eq!(edge.map_arrival(now), now + Duration::from_nanos(1));
        assert_eq!(edge.last(), Some(now));
    }

    /// A source fast or slow by a few hundred ppm, checked against one bucket of that
    /// drift. `DriftEstimator` publishes a new line when a bucket completes, so a held
    /// mapping can be stale by one bucket. A rate left at 1.0 is wrong by the drift
    /// over the frame's age (three buckets) and fails this ceiling.
    #[test]
    fn steady_drift_stays_within_one_bucket() {
        for ppm in [400_i64, -350] {
            let bucket = Duration::from_millis(100);
            let mut edge = EdgeClock::new(bucket);
            let clock = TestClock::new();
            clock.advance(Duration::from_secs(1));

            let ppm_abs = u128::from(ppm.unsigned_abs());
            let bucket_ns = bucket.as_nanos();
            let bound = bucket_ns * ppm_abs / 1_000_000;
            let age_ns = u64::try_from(bucket_ns)
                .unwrap_or(u64::MAX)
                .saturating_mul(3);
            let stuck_rate_error = u128::from(age_ns) * ppm_abs / 1_000_000;
            assert!(bound < stuck_rate_error);

            let mut previous = Timestamp::ZERO;
            for step in 0..200 {
                clock.advance(Duration::from_millis(10));
                let now = clock.now();
                edge.observe(source_nanos(now.as_nanos(), ppm), now);
                let captured = now.as_nanos() - age_ns;
                let stamp = edge.to_master(source_nanos(captured, ppm), now);
                assert!(stamp > previous);
                assert!(stamp <= now);
                // One second of observations is ten buckets, past the rate-1 start.
                if step > 100 {
                    let error = u128::from(stamp.as_nanos().abs_diff(captured));
                    assert!(
                        error <= bound,
                        "ppm {ppm} error {error} ns bound {bound} at step {step}"
                    );
                }
                previous = stamp;
            }
        }
    }

    /// `source = master + master * ppm / 1_000_000`. Positive `ppm` means the source
    /// runs fast.
    fn source_nanos(master_ns: u64, ppm: i64) -> u64 {
        let master = i128::from(master_ns);
        let source = master + master * i128::from(ppm) / 1_000_000;
        if source <= 0 {
            0
        } else if source >= i128::from(u64::MAX) {
            u64::MAX
        } else {
            // `source` is inside `u64`.
            source as u64
        }
    }
}
