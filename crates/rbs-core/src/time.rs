//! Master-clock time.
//!
//! Every frame, audio block and packet in RBS carries a [`Timestamp`] in master-clock
//! time. Sources convert from their own clock at the edge, using [`ClockMapping`].
//! See `docs/adr/0004-single-master-clock.md`.

use std::ops::{Add, Sub};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A point in master-clock time, in nanoseconds since the clock's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The clock origin.
    pub const ZERO: Self = Self(0);

    /// Creates a timestamp from nanoseconds since the origin.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the origin.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Time elapsed from `earlier` to `self`, or zero if `earlier` is later.
    pub const fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Timestamp {
    type Output = Self;

    /// Adds a duration, saturating at the maximum representable time.
    fn add(self, rhs: Duration) -> Self {
        let nanos = u64::try_from(rhs.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(nanos))
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Self;

    /// Subtracts a duration, saturating at the origin.
    fn sub(self, rhs: Duration) -> Self {
        let nanos = u64::try_from(rhs.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_sub(nanos))
    }
}

/// A monotonic source of master-clock time.
///
/// Implementations must be monotonic (never go backwards) and cheap enough to call
/// from real-time threads.
pub trait Clock: Send + Sync {
    /// The current master-clock time.
    fn now(&self) -> Timestamp;
}

/// The production master clock, based on the platform's monotonic clock
/// (QPC, `mach_absolute_time`, `CLOCK_MONOTONIC` through [`Instant`]).
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    /// Creates a clock whose origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Timestamp {
        Timestamp::ZERO + self.origin.elapsed()
    }
}

/// A manually driven clock for deterministic tests. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct TestClock {
    nanos: Arc<AtomicU64>,
}

impl TestClock {
    /// Creates a test clock at the origin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        // Relaxed is enough: tests only need each read to observe a monotonic value.
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Sets the clock to `to`. Panics in debug builds if this would move time backwards.
    pub fn set(&self, to: Timestamp) {
        let previous = self.nanos.swap(to.as_nanos(), Ordering::Relaxed);
        debug_assert!(previous <= to.as_nanos(), "TestClock must not go backwards");
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(self.nanos.load(Ordering::Relaxed))
    }
}

/// Converts timestamps from a source's own clock domain to master-clock time.
///
/// The mapping is linear: `master = anchor_master + (source - anchor_source) * rate`,
/// where `rate` corrects for drift between the two clocks (1.0 means no drift).
/// Drift estimation itself belongs to the audio and capture layers; this type only
/// applies the current estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockMapping {
    anchor_source_nanos: u64,
    anchor_master: Timestamp,
    rate: f64,
}

impl ClockMapping {
    /// Creates a mapping where `source_nanos` in the source domain equals `master`.
    pub fn anchored(source_nanos: u64, master: Timestamp) -> Self {
        Self {
            anchor_source_nanos: source_nanos,
            anchor_master: master,
            rate: 1.0,
        }
    }

    /// Returns a copy with a drift correction rate. Rates outside `0.9..=1.1` are clamped,
    /// as larger values indicate a broken estimate rather than real drift.
    pub fn with_rate(mut self, rate: f64) -> Self {
        self.rate = if rate.is_finite() {
            rate.clamp(0.9, 1.1)
        } else {
            1.0
        };
        self
    }

    /// The drift correction rate in use.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Maps a source-domain time in nanoseconds to master-clock time.
    /// Times before the anchor are mapped backwards and saturate at the origin.
    pub fn to_master(&self, source_nanos: u64) -> Timestamp {
        let delta = source_nanos as i128 - self.anchor_source_nanos as i128;
        let scaled = (delta as f64 * self.rate).round() as i128;
        let master = self.anchor_master.as_nanos() as i128 + scaled;
        Timestamp::from_nanos(master.clamp(0, u64::MAX as i128) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_arithmetic_saturates() {
        let t = Timestamp::from_nanos(10);
        assert_eq!(t - Duration::from_nanos(20), Timestamp::ZERO);
        assert_eq!(
            Timestamp::from_nanos(u64::MAX) + Duration::from_secs(1),
            Timestamp::from_nanos(u64::MAX)
        );
        assert_eq!(
            Timestamp::from_nanos(30).saturating_since(t),
            Duration::from_nanos(20)
        );
        assert_eq!(
            t.saturating_since(Timestamp::from_nanos(30)),
            Duration::ZERO
        );
    }

    #[test]
    fn monotonic_clock_does_not_go_backwards() {
        let clock = MonotonicClock::new();
        let mut last = clock.now();
        for _ in 0..1000 {
            let now = clock.now();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn test_clock_is_shared_between_clones() {
        let clock = TestClock::new();
        let other = clock.clone();
        clock.advance(Duration::from_millis(5));
        assert_eq!(other.now(), Timestamp::from_nanos(5_000_000));
        other.set(Timestamp::from_nanos(6_000_000));
        assert_eq!(clock.now(), Timestamp::from_nanos(6_000_000));
    }

    #[test]
    fn clock_mapping_applies_offset_and_rate() {
        let mapping = ClockMapping::anchored(1_000, Timestamp::from_nanos(50_000));
        assert_eq!(mapping.to_master(1_000), Timestamp::from_nanos(50_000));
        assert_eq!(mapping.to_master(2_000), Timestamp::from_nanos(51_000));
        assert_eq!(mapping.to_master(0), Timestamp::from_nanos(49_000));

        let fast = mapping.with_rate(1.001);
        assert_eq!(fast.to_master(1_001_000), Timestamp::from_nanos(1_051_000));
    }

    #[test]
    fn clock_mapping_clamps_bad_rates_and_saturates() {
        let mapping = ClockMapping::anchored(1_000_000, Timestamp::from_nanos(10));
        assert_eq!(mapping.with_rate(5.0).rate(), 1.1);
        assert_eq!(mapping.with_rate(f64::NAN).rate(), 1.0);
        assert_eq!(mapping.to_master(0), Timestamp::ZERO);
    }
}
