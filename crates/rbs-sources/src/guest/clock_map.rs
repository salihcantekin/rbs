//! The sender-to-master clock map (ADR-0139 decision 11, Sync; spec `guest-source.md`).
//!
//! Each frame carries the sender's master-clock capture time. The gap between that
//! time and this side's arrival time is the clock offset plus the network delay plus
//! jitter. The smallest gap of each second is the frame that met the least delay, so
//! the map is `gap(sender) = offset + drift * (sender - fit_at)`, refitted once a
//! second: the drift is a least-squares line through the minimums of the last 60
//! completed seconds (bounded to +-500 ppm, minimums far above the line left out),
//! the offset the median of the last 10 minimums carried along it. The map in use
//! moves toward a new fit by at most 1 ms per second of sender time, so a route
//! change does not jump every release time; a change above 50 ms is a resync, applied
//! at once and counted. One map serves picture and sound, so both keep their sync,
//! and the sound's `InputAligner` sees a steady map, not a second drift correction.

use rbs_core::time::Timestamp;

/// One slot per second of sender time.
const SLOT_NS: u64 = 1_000_000_000;
/// Seconds the fit looks back.
pub const FIT_SECONDS: usize = 60;
/// Largest drift the fit accepts, in parts per million.
pub const MAX_DRIFT_PPM: f64 = 500.0;
/// The map in use moves toward a new fit by at most this much per second.
pub const SLEW_NS_PER_S: f64 = 1_000_000.0;
/// A change larger than this is applied at once and counted as a resync.
pub const RESYNC_NS: f64 = 50_000_000.0;
/// A second's minimum this far above the first fit is left out of the drift fit.
const OUTLIER_NS: f64 = 10_000_000.0;
/// The offset is the median of this many most recent seconds.
const OFFSET_SLOTS: usize = 10;

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The slot's second, counted from `base`.
    second: u64,
    /// The smallest gap (arrival - sender) seen in it, in nanoseconds.
    min_gap: i128,
}

/// A line `gap = offset + drift * (rel - at)`, with `rel` sender time from `base`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Line {
    offset: f64,
    drift: f64,
    at: f64,
}

impl Line {
    fn gap(&self, rel: f64) -> f64 {
        self.offset + self.drift * (rel - self.at)
    }
}

/// Maps sender capture times to this side's master clock. Fixed memory: one ring of
/// [`FIT_SECONDS`] slots.
#[derive(Debug, Clone)]
pub struct ClockMap {
    base: Option<u64>,
    /// The second frames are being collected in.
    current: u64,
    slots: [Option<Slot>; FIT_SECONDS],
    /// The map in use.
    applied: Option<Line>,
    resyncs: u64,
}

impl Default for ClockMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockMap {
    /// An empty map, not valid.
    pub fn new() -> Self {
        Self {
            base: None,
            current: 0,
            slots: [None; FIT_SECONDS],
            applied: None,
            resyncs: 0,
        }
    }

    /// Adds one frame's sender capture time and its arrival on this side's master
    /// clock. The caller has checked the capture time (`timing.rs`); a time before the
    /// first one starts the map over.
    pub fn observe(&mut self, sender_ns: u64, arrived: Timestamp) {
        let base = *self.base.get_or_insert(sender_ns);
        if sender_ns < base {
            *self = Self {
                resyncs: self.resyncs + 1,
                ..Self::new()
            };
            self.observe(sender_ns, arrived);
            return;
        }
        let second = (sender_ns - base) / SLOT_NS;
        if second > self.current {
            let closed = self.current;
            self.current = second;
            self.refit(closed);
        }
        let gap = i128::from(arrived.as_nanos()) - i128::from(sender_ns);
        let at = usize::try_from(second % FIT_SECONDS as u64).unwrap_or(0);
        match &mut self.slots[at] {
            Some(slot) if slot.second == second => slot.min_gap = slot.min_gap.min(gap),
            Some(slot) if slot.second > second => {} // older than the window
            entry => {
                *entry = Some(Slot {
                    second,
                    min_gap: gap,
                });
            }
        }
    }

    /// The map has a fit: at least one whole second of frames was seen.
    pub fn is_valid(&self) -> bool {
        self.applied.is_some()
    }

    /// The master time of a sender time, once valid.
    pub fn map(&self, sender_ns: u64) -> Option<Timestamp> {
        let line = self.applied?;
        let rel = sender_ns.checked_sub(self.base?)? as f64;
        let master = sender_ns as f64 + line.gap(rel);
        (master >= 0.0).then(|| Timestamp::from_nanos(master.round() as u64))
    }

    /// The drift in use, in parts per million (positive: this side's clock runs fast).
    pub fn drift_ppm(&self) -> Option<f64> {
        self.applied.map(|l| l.drift * 1e6)
    }

    /// How often the map jumped instead of slewing.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Fits the completed slots up to `closed` and moves the map in use toward it.
    fn refit(&mut self, closed: u64) {
        let oldest = closed.saturating_sub(FIT_SECONDS as u64 - 1);
        let live = |s: &&Slot| s.second >= oldest && s.second <= closed;
        let slots = || self.slots.iter().flatten().filter(live);
        let Some(first) = fit(slots(), None) else {
            return;
        };
        // The drift from the whole window, without seconds far above the first line.
        let drift = fit(slots(), Some(first)).unwrap_or(first).drift;
        // The offset now: the median of the last seconds' minimums, carried to now
        // along the drift, so one far-off second does not move it.
        let now = (closed as f64 + 1.0) * SLOT_NS as f64;
        let mut recent = [0.0_f64; OFFSET_SLOTS];
        let mut n = 0;
        for s in slots().filter(|s| s.second + OFFSET_SLOTS as u64 > closed) {
            let x = (s.second as f64 + 0.5) * SLOT_NS as f64;
            if n < OFFSET_SLOTS {
                recent[n] = s.min_gap as f64 + drift * (now - x);
                n += 1;
            }
        }
        let recent = &mut recent[..n];
        recent.sort_by(f64::total_cmp);
        let Some(&offset) = recent.get(n / 2) else {
            return;
        };
        let target = Line {
            offset,
            drift,
            at: now,
        };
        let Some(applied) = self.applied else {
            self.applied = Some(target);
            return;
        };
        let step = offset - applied.gap(now);
        if step.abs() > RESYNC_NS {
            self.resyncs += 1;
            self.applied = Some(target);
            return;
        }
        // One second passed since the last refit: move at most one slew step.
        self.applied = Some(Line {
            offset: applied.gap(now) + step.clamp(-SLEW_NS_PER_S, SLEW_NS_PER_S),
            drift,
            at: now,
        });
    }
}

/// A least-squares line through the slots' minimums at their centres; with `around`,
/// minimums more than [`OUTLIER_NS`] above that line are left out.
fn fit<'a>(slots: impl Iterator<Item = &'a Slot>, around: Option<Line>) -> Option<Line> {
    let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for s in slots {
        let x = (s.second as f64 + 0.5) * SLOT_NS as f64;
        let y = s.min_gap as f64;
        if around.is_some_and(|l| y - l.gap(x) > OUTLIER_NS) {
            continue;
        }
        n += 1.0;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    if n == 0.0 {
        return None;
    }
    let spread = n * sxx - sx * sx;
    let limit = MAX_DRIFT_PPM * 1e-6;
    let drift = if n >= 3.0 && spread > 0.0 {
        ((n * sxy - sx * sy) / spread).clamp(-limit, limit)
    } else {
        0.0
    };
    Some(Line {
        offset: sy / n,
        drift,
        at: sx / n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic jitter source (xorshift).
    struct Noise(u64);

    impl Noise {
        fn next(&mut self, max_ns: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % (max_ns + 1)
        }
    }

    const FRAME_NS: u64 = 33_333_333;
    const START: u64 = 5_000_000_000_000;
    const OFFSET: u64 = 70_000_000_000; // this side's clock is 70 s ahead
    const DELAY: u64 = 30_000_000; // the least network delay

    fn feed(m: &mut ClockMap, noise: &mut Noise, from: u64, to: u64, extra: u64) {
        for i in from..to {
            let s = START + i * FRAME_NS;
            let arrived = s + OFFSET + DELAY + extra + noise.next(40_000_000);
            m.observe(s, Timestamp::from_nanos(arrived));
        }
    }

    #[test]
    fn nothing_maps_until_a_whole_second_of_frames() {
        let mut m = ClockMap::new();
        let mut noise = Noise(7);
        feed(&mut m, &mut noise, 0, 30, 0);
        assert!(!m.is_valid());
        assert_eq!(m.map(START), None);
        feed(&mut m, &mut noise, 30, 31, 0);
        assert!(m.is_valid());
    }

    #[test]
    fn it_finds_offset_plus_least_delay_within_a_millisecond_in_ten_seconds() {
        let mut m = ClockMap::new();
        let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
        feed(&mut m, &mut noise, 0, 300, 0);
        let s = START + 300 * FRAME_NS;
        let mapped = m.map(s).unwrap().as_nanos();
        let want = s + OFFSET + DELAY;
        let off = mapped.abs_diff(want);
        // Each second's minimum of 30 frames is about 40 ms / 31 above the least delay.
        assert!(off < 2_500_000, "off by {off} ns");
    }

    #[test]
    fn it_follows_a_200_ppm_drift_for_an_hour_within_a_millisecond() {
        let mut m = ClockMap::new();
        let mut noise = Noise(0x2545_F491_4F6C_DD1D);
        let mut worst = 0_u64;
        let frames = 3600 * 30;
        for i in 0..frames {
            let rel = i * FRAME_NS;
            let s = START + rel;
            // This side's clock runs 200 ppm fast against the sender's.
            let truth = s + OFFSET + DELAY + rel / 5_000;
            m.observe(s, Timestamp::from_nanos(truth + noise.next(2_000_000)));
            if i > 30 * 120 && i % 300 == 0 {
                let next = s + FRAME_NS;
                let want = next + OFFSET + DELAY + (rel + FRAME_NS) / 5_000;
                worst = worst.max(m.map(next).unwrap().as_nanos().abs_diff(want));
            }
        }
        assert!(worst < 1_000_000, "worst error {worst} ns");
        let ppm = m.drift_ppm().unwrap();
        assert!((ppm - 200.0).abs() < 20.0, "drift {ppm} ppm");
        assert_eq!(m.resyncs(), 0);
    }

    #[test]
    fn a_route_change_slews_and_a_large_one_resyncs() {
        let mut m = ClockMap::new();
        let mut noise = Noise(11);
        feed(&mut m, &mut noise, 0, 900, 0);
        let probe = START + 900 * FRAME_NS;
        let before = m.map(probe).unwrap().as_nanos();
        // The least delay grows by 20 ms: once most of the last 10 seconds show it,
        // the map moves 1 ms per second.
        feed(&mut m, &mut noise, 900, 1200, 20_000_000);
        let after = m.map(probe).unwrap().as_nanos();
        let moved = after.abs_diff(before);
        assert!((3_000_000..=8_000_000).contains(&moved), "moved {moved} ns in 10 s");
        assert_eq!(m.resyncs(), 0);
        // A 2 s step is a resync, applied at once.
        let mut big = ClockMap::new();
        feed(&mut big, &mut noise, 0, 900, 0);
        feed(&mut big, &mut noise, 900, 4000, 2_000_000_000);
        assert!(big.resyncs() >= 1);
        let s = START + 4000 * FRAME_NS;
        let off = big.map(s).unwrap().as_nanos().abs_diff(s + OFFSET + DELAY + 2_000_000_000);
        assert!(off < 5_000_000, "off by {off} ns");
    }

    #[test]
    fn one_far_off_second_does_not_move_the_map() {
        let mut m = ClockMap::new();
        let mut noise = Noise(5);
        feed(&mut m, &mut noise, 0, 600, 0);
        let probe = START + 600 * FRAME_NS;
        let before = m.map(probe).unwrap().as_nanos();
        // One second whose frames all arrive 300 ms late, then normal again.
        feed(&mut m, &mut noise, 600, 630, 300_000_000);
        feed(&mut m, &mut noise, 630, 700, 0);
        let after = m.map(probe).unwrap().as_nanos();
        assert!(after.abs_diff(before) < 1_000_000, "moved {} ns", after.abs_diff(before));
    }

    #[test]
    fn the_drift_is_bounded_and_a_restarted_sender_starts_over() {
        let mut m = ClockMap::new();
        for i in 0..10 {
            let s = START + i * SLOT_NS;
            // 10 ms more each second: 10 000 ppm, far beyond the bound.
            m.observe(s, Timestamp::from_nanos(s + OFFSET + i * 10_000_000));
        }
        assert!((m.drift_ppm().unwrap() - MAX_DRIFT_PPM).abs() < 1e-6);
        m.observe(1_000, Timestamp::from_nanos(OFFSET));
        assert!(!m.is_valid(), "a sender time before the base starts over");
        assert_eq!(m.resyncs(), 1);
    }
}
