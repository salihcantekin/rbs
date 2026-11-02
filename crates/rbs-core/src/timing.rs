//! Exact media timing on top of master-clock [`Timestamp`]s.
//!
//! - [`Rational`]: a positive exact ratio, the basis of rates and timebases.
//! - [`FrameRate`] and [`SampleRate`]: typed rates for video and audio.
//! - [`Cadence`]: the timestamps of a regular series (canvas render ticks, audio
//!   blocks, sample positions), computed from the index so error never accumulates.
//! - [`Timebase`]: conversion between durations and integer ticks of a container or
//!   protocol timebase (for example 1/90000 for MPEG-TS, 1/1000 for FLV).
//! - [`DriftEstimator`]: estimates a [`ClockMapping`] from observed pairs of
//!   source-clock and master-clock times.
//!
//! Everything here is allocation free and safe to call from real-time threads
//! (`docs/architecture/hot-paths.md`). See `docs/specs/media-types.md`.

use std::fmt;
use std::time::Duration;

use crate::time::{ClockMapping, Timestamp};

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Saturating conversion from a nanosecond count to `u64`.
fn saturate_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// A positive rational number `num / den`, both non-zero.
///
/// Values are kept as given (not reduced) so that `30000/1001` stays readable in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    num: u32,
    den: u32,
}

impl Rational {
    /// Creates `num / den`. Returns `None` if either part is zero.
    pub const fn new(num: u32, den: u32) -> Option<Self> {
        if num == 0 || den == 0 {
            None
        } else {
            Some(Self { num, den })
        }
    }

    /// The numerator.
    pub const fn num(self) -> u32 {
        self.num
    }

    /// The denominator.
    pub const fn den(self) -> u32 {
        self.den
    }

    /// The value as a float, for display and diagnostics only.
    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// Video frames per second, as an exact ratio (for example `60000/1001`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRate(Rational);

impl FrameRate {
    /// 24 frames per second.
    pub const FPS_24: Self = Self(Rational { num: 24, den: 1 });
    /// 25 frames per second.
    pub const FPS_25: Self = Self(Rational { num: 25, den: 1 });
    /// 29.97 frames per second (`30000/1001`).
    pub const FPS_29_97: Self = Self(Rational {
        num: 30_000,
        den: 1_001,
    });
    /// 30 frames per second.
    pub const FPS_30: Self = Self(Rational { num: 30, den: 1 });
    /// 50 frames per second.
    pub const FPS_50: Self = Self(Rational { num: 50, den: 1 });
    /// 59.94 frames per second (`60000/1001`).
    pub const FPS_59_94: Self = Self(Rational {
        num: 60_000,
        den: 1_001,
    });
    /// 60 frames per second.
    pub const FPS_60: Self = Self(Rational { num: 60, den: 1 });

    /// Creates a rate of `num / den` frames per second. `None` if either part is zero.
    pub const fn new(num: u32, den: u32) -> Option<Self> {
        match Rational::new(num, den) {
            Some(r) => Some(Self(r)),
            None => None,
        }
    }

    /// Frames per second as an exact ratio.
    pub const fn per_second(self) -> Rational {
        self.0
    }

    /// The duration of one frame, rounded down to whole nanoseconds. For positions in
    /// a series use [`Cadence`], which does not accumulate this rounding.
    pub fn frame_duration(self) -> Duration {
        let nanos = u128::from(self.0.den) * NANOS_PER_SEC / u128::from(self.0.num);
        Duration::from_nanos(saturate_u64(nanos))
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fps", self.0)
    }
}

/// Audio samples per second per channel (sample frames per second).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SampleRate(u32);

impl SampleRate {
    /// 44.1 kHz.
    pub const HZ_44100: Self = Self(44_100);
    /// 48 kHz, the default rate of the audio graph.
    pub const HZ_48000: Self = Self(48_000);
    /// 96 kHz.
    pub const HZ_96000: Self = Self(96_000);

    /// Creates a sample rate. `None` for zero.
    pub const fn new(hz: u32) -> Option<Self> {
        if hz == 0 { None } else { Some(Self(hz)) }
    }

    /// The rate in hertz.
    pub const fn hz(self) -> u32 {
        self.0
    }

    /// Exact duration of `frames` sample frames, rounded down to whole nanoseconds.
    pub fn duration_of(self, frames: u64) -> Duration {
        let nanos = u128::from(frames) * NANOS_PER_SEC / u128::from(self.0);
        Duration::from_nanos(saturate_u64(nanos))
    }
}

impl fmt::Display for SampleRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.0)
    }
}

/// The timestamps of a regular series that starts at `origin` and has `rate` items
/// per second: canvas render ticks, audio blocks, or individual samples.
///
/// Item `n` is at `origin + floor(n * den * 1e9 / num)` nanoseconds. Each position is
/// computed from its index, so rounding never accumulates over long sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    origin: Timestamp,
    /// Items per second.
    rate: Rational,
}

impl Cadence {
    /// A series with `rate` items per second starting at `origin`.
    pub const fn new(origin: Timestamp, rate: Rational) -> Self {
        Self { origin, rate }
    }

    /// Render ticks of a canvas running at `rate`.
    pub const fn frames(origin: Timestamp, rate: FrameRate) -> Self {
        Self::new(origin, rate.per_second())
    }

    /// Positions of individual sample frames at `rate`.
    pub const fn samples(origin: Timestamp, rate: SampleRate) -> Self {
        Self::new(
            origin,
            Rational {
                num: rate.0,
                den: 1,
            },
        )
    }

    /// Audio blocks of `block_frames` samples at `rate`. `None` if `block_frames` is 0.
    pub const fn blocks(origin: Timestamp, rate: SampleRate, block_frames: u32) -> Option<Self> {
        match Rational::new(rate.0, block_frames) {
            Some(r) => Some(Self::new(origin, r)),
            None => None,
        }
    }

    /// The time of item 0.
    pub const fn origin(&self) -> Timestamp {
        self.origin
    }

    /// Items per second.
    pub const fn rate(&self) -> Rational {
        self.rate
    }

    /// Nanoseconds per item, as `(numerator, denominator)` of the exact value.
    fn period(&self) -> (u128, u128) {
        (
            u128::from(self.rate.den) * NANOS_PER_SEC,
            u128::from(self.rate.num),
        )
    }

    /// The time of item `index`. Saturates at the maximum timestamp.
    pub fn time_of(&self, index: u64) -> Timestamp {
        let (p_num, p_den) = self.period();
        let offset = u128::from(index) * p_num / p_den;
        Timestamp::from_nanos(saturate_u64(
            u128::from(self.origin.as_nanos()).saturating_add(offset),
        ))
    }

    /// The index of the last item at or before `t`, or `None` if `t` is before the origin.
    pub fn index_at_or_before(&self, t: Timestamp) -> Option<u64> {
        if t < self.origin {
            return None;
        }
        let elapsed = u128::from(t.as_nanos() - self.origin.as_nanos());
        let (p_num, p_den) = self.period();
        // Largest n with floor(n * p_num / p_den) <= elapsed,
        // i.e. n * p_num < (elapsed + 1) * p_den.
        Some(saturate_u64(((elapsed + 1) * p_den - 1) / p_num))
    }

    /// The first item strictly after `t`, as `(index, time)`.
    pub fn next_after(&self, t: Timestamp) -> (u64, Timestamp) {
        let index = match self.index_at_or_before(t) {
            Some(i) => i.saturating_add(1),
            None => 0,
        };
        (index, self.time_of(index))
    }
}

/// The time unit of a container or protocol: one tick is `num / den` seconds
/// (for example `1/90000` for MPEG-TS, `1/1000` for FLV, `1/48000` for Opus).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timebase(Rational);

impl Timebase {
    /// 1/1000 s (FLV, RTMP).
    pub const MILLIS: Self = Self(Rational { num: 1, den: 1_000 });
    /// 1/90000 s (MPEG-TS, RTP video).
    pub const MPEG_90K: Self = Self(Rational {
        num: 1,
        den: 90_000,
    });
    /// 1/1000000000 s (master-clock resolution).
    pub const NANOS: Self = Self(Rational {
        num: 1,
        den: 1_000_000_000,
    });

    /// A timebase where one tick is `num / den` seconds. `None` if either part is zero.
    pub const fn new(num: u32, den: u32) -> Option<Self> {
        match Rational::new(num, den) {
            Some(r) => Some(Self(r)),
            None => None,
        }
    }

    /// The tick length in seconds as an exact ratio.
    pub const fn tick(self) -> Rational {
        self.0
    }

    /// The number of whole ticks in `d`, rounded to the nearest tick (ties up).
    /// Saturates at `u64::MAX`.
    pub fn ticks_in(self, d: Duration) -> u64 {
        let num = d.as_nanos() * u128::from(self.0.den);
        let den = NANOS_PER_SEC * u128::from(self.0.num);
        saturate_u64((num + den / 2) / den)
    }

    /// The duration of `ticks` ticks, rounded to the nearest nanosecond (ties up).
    pub fn duration_of(self, ticks: u64) -> Duration {
        let num = u128::from(ticks) * u128::from(self.0.num) * NANOS_PER_SEC;
        let den = u128::from(self.0.den);
        Duration::from_nanos(saturate_u64((num + den / 2) / den))
    }

    /// Ticks from `origin` to `t`: the usual way to turn a packet timestamp into a
    /// container timestamp. Zero if `t` is before `origin`.
    pub fn ticks_since(self, origin: Timestamp, t: Timestamp) -> u64 {
        self.ticks_in(t.saturating_since(origin))
    }
}

/// Number of averaged points [`DriftEstimator`] keeps.
pub const DRIFT_WINDOW: usize = 64;

/// Estimates the mapping from a source clock (an audio device, a capture API) to the
/// master clock, from observed pairs `(source time, master time)` of the same instant.
///
/// Observations are averaged into buckets that each span `bucket` of source time, which
/// removes most of the scheduling jitter in the master-clock reads. A least-squares
/// line is fitted over the last [`DRIFT_WINDOW`] buckets plus the bucket being filled,
/// so with the default one-second bucket the estimate covers about a minute. The
/// result is a [`ClockMapping`] anchored at the newest observation's fitted point.
///
/// Fixed size, no allocation: safe to feed from a real-time thread.
#[derive(Debug, Clone)]
pub struct DriftEstimator {
    bucket_nanos: u64,
    /// First observation; all stored values are relative to it to keep floats precise.
    base: Option<(u64, Timestamp)>,
    /// Averaged `(source - base_source, master - base_master)` points in nanoseconds.
    points: [(f64, f64); DRIFT_WINDOW],
    len: usize,
    next: usize,
    /// The bucket being filled: start in source time, sums and count.
    open_start: u64,
    open_sum: (f64, f64),
    open_count: u32,
    latest_source: u64,
}

impl Default for DriftEstimator {
    fn default() -> Self {
        Self::new(Duration::from_secs(1))
    }
}

impl DriftEstimator {
    /// An estimator that averages observations over `bucket` of source time
    /// (at least 1 ns).
    pub fn new(bucket: Duration) -> Self {
        Self {
            bucket_nanos: saturate_u64(bucket.as_nanos()).max(1),
            base: None,
            points: [(0.0, 0.0); DRIFT_WINDOW],
            len: 0,
            next: 0,
            open_start: 0,
            open_sum: (0.0, 0.0),
            open_count: 0,
            latest_source: 0,
        }
    }

    /// Forgets all observations, for example after a device restart or a clock jump.
    pub fn reset(&mut self) {
        *self = Self::new(Duration::from_nanos(self.bucket_nanos));
    }

    /// The number of completed buckets in the window.
    pub fn buckets(&self) -> usize {
        self.len
    }

    /// True before the first observation.
    pub fn is_empty(&self) -> bool {
        self.base.is_none()
    }

    /// Records that `source_nanos` on the source clock and `master` on the master clock
    /// describe the same instant. Source times are expected to increase.
    pub fn observe(&mut self, source_nanos: u64, master: Timestamp) {
        let (base_source, base_master) = *self.base.get_or_insert((source_nanos, master));
        if self.open_count > 0 && source_nanos.saturating_sub(self.open_start) >= self.bucket_nanos
        {
            self.close_bucket();
        }
        if self.open_count == 0 {
            self.open_start = source_nanos;
        }
        let x = (i128::from(source_nanos) - i128::from(base_source)) as f64;
        let y = (i128::from(master.as_nanos()) - i128::from(base_master.as_nanos())) as f64;
        self.open_sum = (self.open_sum.0 + x, self.open_sum.1 + y);
        self.open_count += 1;
        self.latest_source = source_nanos;
    }

    fn close_bucket(&mut self) {
        let n = f64::from(self.open_count);
        self.points[self.next] = (self.open_sum.0 / n, self.open_sum.1 / n);
        self.next = (self.next + 1) % DRIFT_WINDOW;
        self.len = (self.len + 1).min(DRIFT_WINDOW);
        self.open_sum = (0.0, 0.0);
        self.open_count = 0;
    }

    /// The current mapping estimate, or `None` before the first observation.
    ///
    /// Until two buckets exist the rate is 1.0. Rates are clamped by
    /// [`ClockMapping::with_rate`].
    pub fn mapping(&self) -> Option<ClockMapping> {
        let (base_source, base_master) = self.base?;
        let open = (self.open_count > 0).then(|| {
            let n = f64::from(self.open_count);
            (self.open_sum.0 / n, self.open_sum.1 / n)
        });
        let points = || self.points[..self.len].iter().copied().chain(open);
        let n = (self.len + usize::from(open.is_some())) as f64;
        let (sum_x, sum_y) = points().fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
        let (mean_x, mean_y) = (sum_x / n, sum_y / n);
        let (sxx, sxy) = points().fold((0.0, 0.0), |(sxx, sxy), (x, y)| {
            let dx = x - mean_x;
            (sxx + dx * dx, sxy + dx * (y - mean_y))
        });
        let rate = if sxx > 0.0 { sxy / sxx } else { 1.0 };
        let rate = ClockMapping::anchored(0, Timestamp::ZERO)
            .with_rate(rate)
            .rate();

        // Fitted master time at the newest source time, relative to the base.
        let latest_x = (i128::from(self.latest_source) - i128::from(base_source)) as f64;
        let fitted_y = mean_y + rate * (latest_x - mean_x);
        let master = (i128::from(base_master.as_nanos()) + fitted_y.round() as i128)
            .clamp(0, i128::from(u64::MAX));
        let master = Timestamp::from_nanos(u64::try_from(master).unwrap_or(u64::MAX));
        Some(ClockMapping::anchored(self.latest_source, master).with_rate(rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic generator so tests need no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn rational_rejects_zero() {
        assert!(Rational::new(0, 1).is_none());
        assert!(Rational::new(1, 0).is_none());
        assert_eq!(
            Rational::new(30_000, 1_001)
                .map(|r| r.to_string())
                .as_deref(),
            Some("30000/1001")
        );
        assert!(FrameRate::new(0, 1).is_none());
        assert!(SampleRate::new(0).is_none());
        assert!(Timebase::new(1, 0).is_none());
    }

    #[test]
    fn frame_and_sample_durations() {
        assert_eq!(
            FrameRate::FPS_60.frame_duration(),
            Duration::from_nanos(16_666_666)
        );
        assert_eq!(
            FrameRate::FPS_59_94.frame_duration(),
            Duration::from_nanos(16_683_333)
        );
        assert_eq!(
            SampleRate::HZ_48000.duration_of(480),
            Duration::from_millis(10)
        );
        assert_eq!(
            SampleRate::HZ_44100.duration_of(44_100),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn cadence_does_not_drift_over_long_runs() {
        let origin = Timestamp::from_nanos(5);
        let ntsc = Cadence::frames(origin, FrameRate::FPS_59_94);
        // After exactly 60000 frames, 1001 seconds have passed.
        assert_eq!(ntsc.time_of(60_000), origin + Duration::from_secs(1_001));
        // 24 hours of 48 kHz audio lands exactly on the second.
        let samples = Cadence::samples(origin, SampleRate::HZ_48000);
        assert_eq!(
            samples.time_of(48_000 * 86_400),
            origin + Duration::from_secs(86_400)
        );
        let blocks = Cadence::blocks(origin, SampleRate::HZ_48000, 480);
        assert_eq!(
            blocks.map(|b| b.time_of(100)),
            Some(origin + Duration::from_secs(1))
        );
        assert!(Cadence::blocks(origin, SampleRate::HZ_48000, 0).is_none());
    }

    #[test]
    fn cadence_index_is_consistent_with_time() {
        let mut rng = Lcg(42);
        let rates = [
            Rational::new(60_000, 1_001),
            Rational::new(30, 1),
            Rational::new(48_000, 1),
            Rational::new(44_100, 441),
            Rational::new(7, 3),
            Rational::new(1_000_000_000, 1),
        ];
        for rate in rates.into_iter().flatten() {
            let origin = Timestamp::from_nanos(rng.next() % 1_000_000);
            let cadence = Cadence::new(origin, rate);
            assert_eq!(cadence.index_at_or_before(origin), Some(0));
            if origin.as_nanos() > 0 {
                assert_eq!(cadence.index_at_or_before(Timestamp::ZERO), None);
                assert_eq!(cadence.next_after(Timestamp::ZERO), (0, origin));
            }
            for _ in 0..2_000 {
                let t = Timestamp::from_nanos(origin.as_nanos() + rng.next() % 10_000_000_000);
                let Some(i) = cadence.index_at_or_before(t) else {
                    panic!("t is after the origin");
                };
                assert!(
                    cadence.time_of(i) <= t,
                    "item {i} must be at or before {t:?}"
                );
                assert!(
                    cadence.time_of(i + 1) > t,
                    "item {} must be after {t:?}",
                    i + 1
                );
                let (next, at) = cadence.next_after(t);
                assert_eq!(next, i + 1);
                assert!(at > t);
                // Every item time maps back to its own index.
                assert_eq!(cadence.index_at_or_before(cadence.time_of(i)), Some(i));
            }
        }
    }

    #[test]
    fn timebase_rescales_with_rounding() {
        assert_eq!(Timebase::MPEG_90K.ticks_in(Duration::from_secs(1)), 90_000);
        assert_eq!(
            Timebase::MPEG_90K.ticks_in(FrameRate::FPS_59_94.frame_duration()),
            1_501
        );
        assert_eq!(Timebase::MILLIS.ticks_in(Duration::from_micros(1_499)), 1);
        assert_eq!(Timebase::MILLIS.ticks_in(Duration::from_micros(1_500)), 2);
        assert_eq!(
            Timebase::MILLIS.duration_of(1_500),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            Timebase::MPEG_90K.duration_of(1),
            Duration::from_nanos(11_111)
        );
        let origin = Timestamp::from_nanos(1_000_000_000);
        assert_eq!(
            Timebase::MILLIS.ticks_since(origin, origin + Duration::from_millis(40)),
            40
        );
        assert_eq!(Timebase::MILLIS.ticks_since(origin, Timestamp::ZERO), 0);
        assert_eq!(Timebase::NANOS.ticks_in(Duration::from_nanos(123)), 123);
    }

    #[test]
    fn timebase_round_trip_is_within_half_a_tick() {
        let mut rng = Lcg(7);
        for tb in [
            Some(Timebase::MILLIS),
            Some(Timebase::MPEG_90K),
            Timebase::new(1, 48_000),
        ]
        .into_iter()
        .flatten()
        {
            let half_tick = tb.duration_of(1) / 2 + Duration::from_nanos(1);
            for _ in 0..1_000 {
                let d = Duration::from_nanos(rng.next() % 100_000_000_000);
                let back = tb.duration_of(tb.ticks_in(d));
                let error = back.abs_diff(d);
                assert!(error <= half_tick, "{d:?} -> {back:?} in {:?}", tb.tick());
            }
        }
    }

    #[test]
    fn drift_estimator_starts_empty_then_tracks_offset() {
        let mut est = DriftEstimator::default();
        assert!(est.mapping().is_none());
        assert!(est.is_empty());
        est.observe(1_000, Timestamp::from_nanos(50_000));
        let Some(m) = est.mapping() else {
            panic!("one observation gives a mapping");
        };
        assert_eq!(m.rate(), 1.0);
        assert_eq!(m.to_master(2_000), Timestamp::from_nanos(51_000));
    }

    #[test]
    fn drift_estimator_finds_rate_through_jitter() {
        let mut rng = Lcg(99);
        let mut est = DriftEstimator::default();
        let true_rate = 1.000_25; // 250 ppm, a cheap USB device
        let origin_master = 3_000_000_000_u64;
        let truth = |source: u64| origin_master + (source as f64 * true_rate) as u64;
        // One observation per 10 ms block for 20 s, with up to 0.5 ms read jitter.
        for i in 0..2_000_u64 {
            let source = 7_000 + i * 10_000_000;
            let jitter = rng.next() % 500_000;
            est.observe(source, Timestamp::from_nanos(truth(source) + jitter));
        }
        assert_eq!(est.buckets(), 19);
        let Some(m) = est.mapping() else {
            panic!("estimator has observations");
        };
        assert!((m.rate() - true_rate).abs() < 2e-5, "rate {}", m.rate());
        // Predict 100 ms past the last observation within 1 ms.
        let source = 7_000 + 2_010 * 10_000_000;
        let predicted = m.to_master(source).as_nanos() as i128;
        let expected = truth(source) as i128 + 250_000;
        assert!(
            (predicted - expected).abs() < 1_000_000,
            "{predicted} vs {expected}"
        );

        est.reset();
        assert!(est.mapping().is_none());
    }
}
