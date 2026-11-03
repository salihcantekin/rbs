//! The clock edge of an audio adapter: OS device time to master time (ADR-0004).
//!
//! Adapter-support module, like [`ring`](crate::ring). Contract: "Clock edge" in
//! `docs/specs/audio-io-port.md`.

use std::time::Duration;

use rbs_core::time::{ClockMapping, Timestamp};
use rbs_core::timing::DriftEstimator;

/// Converts the OS capture time of a packet to master time.
///
/// The adapter feeds it `(OS time, master time)` pairs read back to back with
/// [`observe`](Self::observe), and converts each packet's capture time with
/// [`to_master`](Self::to_master). The mapping is refreshed when the drift estimator
/// completes a bucket (by default once a second), so it changes at most that often.
/// Results are strictly increasing and never later than the master time passed in.
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
        let restarted =
            self.drift.observe(source_nanos, master) == rbs_core::timing::Observation::Restarted;
        if restarted || self.mapping.is_none() || self.drift.buckets() != self.buckets {
            self.mapping = self.drift.mapping();
            self.buckets = self.drift.buckets();
        }
    }

    /// The master time of a packet that lasts `duration` and whose first frame was
    /// captured at OS time `source_nanos`, given the master time `now` at which it is
    /// handled. The packet is never placed so that it ends after `now`. Before the
    /// first observation it ends at `now`.
    pub fn to_master(
        &mut self,
        source_nanos: u64,
        duration: Duration,
        now: Timestamp,
    ) -> Timestamp {
        let latest = now - duration;
        let mapped = self
            .mapping
            .map_or(latest, |mapping| mapping.to_master(source_nanos))
            .min(latest);
        let time = match self.last {
            Some(last) if mapped <= last => last + Duration::from_nanos(1),
            _ => mapped,
        };
        self.last = Some(time);
        time
    }

    /// Forgets the drift estimate (a new OS stream after `start`), keeping results
    /// increasing across it.
    pub fn restart(&mut self) {
        self.drift.reset();
        self.mapping = None;
        self.buckets = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_offset_and_drift_and_stays_monotonic() {
        let mut edge = EdgeClock::new(Duration::from_millis(100));
        // OS clock 5 s ahead, running fast by 100 ppm.
        let os = |master: u64| 5_000_000_000 + master + master / 10_000;
        let mut previous = Timestamp::ZERO;
        for k in 1..=300_u64 {
            let now = k * 10_000_000;
            edge.observe(os(now), Timestamp::from_nanos(now));
            // A packet captured 4 ms before now.
            let captured = now - 4_000_000;
            let t = edge.to_master(
                os(captured),
                Duration::from_millis(1),
                Timestamp::from_nanos(now),
            );
            assert!(t > previous);
            assert!(t.as_nanos() + 1_000_000 <= now);
            if k > 50 {
                let error = t.as_nanos().abs_diff(captured);
                assert!(error < 2_000, "error {error} ns at packet {k}");
            }
            previous = t;
        }
        edge.restart();
        let t = edge.to_master(0, Duration::ZERO, Timestamp::from_nanos(1));
        assert!(t > previous, "monotonic across restart");
    }
}
