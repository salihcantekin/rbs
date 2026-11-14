//! Loudness metering after ITU-R BS.1770-4 and EBU R 128 (ADR-0092 stage 1).
//!
//! [`LoudnessMeter`] is the measurement itself, plain DSP with no graph types: it takes
//! planar blocks of any length and keeps
//!
//! - **momentary** loudness (400 ms window) and **short-term** loudness (3 s window),
//!   ungated sliding windows updated every 10 ms of samples, whatever the block size
//!   (fine enough that the maxima of EBU Tech 3341 cases 11 and 14 read within
//!   0.1 LU);
//! - **integrated** loudness over the 400 ms gating blocks (75 % overlap, so one per
//!   100 ms) above the absolute gate of -70 LUFS and the relative gate 10 LU below
//!   their power mean;
//! - **loudness range** (EBU Tech 3342): the short-term values, one per 100 ms, above
//!   -70 LUFS and 20 LU below their power mean, from the 10th to the 95th percentile;
//! - the **maximum true peak** (dBTP), by oversampling (module `true_peak`), and the
//!   maximum momentary and short-term loudness.
//!
//! Integrated loudness and range need the history of every block since the reset; it
//! is kept in two fixed histograms of 0.1 LU bins from -70 to +30 LUFS that hold the
//! count **and the energy sum** of their blocks, so a mean over bins is exact and only
//! the bin that straddles a relative gate is decided by its mean (well inside the
//! tolerances of Tech 3341 and 3342, tested). Everything is allocated in
//! [`LoudnessMeter::new`]; [`LoudnessMeter::process`] does not allocate, lock or block.
//!
//! [`LoudnessTapNode`] runs a meter as a sink in the audio partition and publishes the
//! values through a lock-free [`LoudnessReadout`].

mod filter;
mod node;
mod true_peak;

pub use node::{
    LOUDNESS_TAP_NODE_TYPE, LoudnessReadout, LoudnessTapNode, LoudnessTapStats,
    loudness_tap_descriptor,
};

use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};

use filter::{Biquad, KState};
use true_peak::TruePeak;

/// The absolute gate of BS.1770 and Tech 3342, in LUFS.
pub const ABSOLUTE_GATE: f64 = -70.0;
/// The relative gate of integrated loudness (BS.1770-4), in LU.
pub const RELATIVE_GATE: f64 = -10.0;
/// The relative gate of loudness range (Tech 3342), in LU.
pub const RANGE_RELATIVE_GATE: f64 = -20.0;

/// Steps (10 ms each) per second.
const STEPS_PER_SECOND: u32 = 100;
/// Steps in the momentary window (400 ms).
const MOMENTARY_STEPS: usize = 40;
/// Steps in the short-term window (3 s).
const SHORT_TERM_STEPS: usize = 300;
/// Steps between two gating blocks and two loudness-range values (100 ms: the 75 %
/// overlap of BS.1770-4 and the 10 Hz of Tech 3342).
const GATING_STEPS: usize = 10;
/// Width of a histogram bin in LU.
const BIN_WIDTH: f64 = 0.1;
/// Histogram bins: -70 to +30 LUFS.
const BINS: usize = 1000;

/// Loudness values in LUFS, LU and dBTP. A value that has no data yet (silence, or
/// less than one window since the reset) is negative infinity, never NaN.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// Momentary loudness (400 ms), LUFS.
    pub momentary: f32,
    /// Short-term loudness (3 s), LUFS.
    pub short_term: f32,
    /// Integrated (programme) loudness since the reset, LUFS.
    pub integrated: f32,
    /// Loudness range since the reset, LU. 0 before two short-term values pass the
    /// gates; negative infinity before the first short-term value.
    pub range: f32,
    /// Highest true peak since the reset, dBTP.
    pub true_peak: f32,
    /// Highest momentary loudness since the reset, LUFS.
    pub max_momentary: f32,
    /// Highest short-term loudness since the reset, LUFS.
    pub max_short_term: f32,
}

impl Loudness {
    /// Every value unknown.
    pub const NONE: Self = Self {
        momentary: f32::NEG_INFINITY,
        short_term: f32::NEG_INFINITY,
        integrated: f32::NEG_INFINITY,
        range: f32::NEG_INFINITY,
        true_peak: f32::NEG_INFINITY,
        max_momentary: f32::NEG_INFINITY,
        max_short_term: f32::NEG_INFINITY,
    };
}

/// The BS.1770-4 channel weights of a layout: 1.0 for front and discrete channels,
/// 1.41 (+1.5 dB) for side surrounds, 0 for LFE (not measured).
pub fn channel_weights(layout: ChannelLayout) -> [f64; MAX_CHANNELS] {
    let mut w = [0.0; MAX_CHANNELS];
    let n = layout.channels().min(MAX_CHANNELS);
    w[..n].fill(1.0);
    match layout {
        ChannelLayout::Surround3_1 => w[3] = 0.0,
        ChannelLayout::Surround5_1 => {
            w[3] = 0.0;
            w[4] = 1.41;
            w[5] = 1.41;
        }
        ChannelLayout::Surround7_1 => {
            w[3] = 0.0;
            w[4] = 1.41;
            w[5] = 1.41;
        }
        _ => {}
    }
    w
}

/// One histogram bin: how many blocks fell in it and the sum of their energies.
#[derive(Debug, Clone, Copy, Default)]
struct Bin {
    count: u64,
    energy: f64,
}

/// Blocks above the absolute gate, binned by loudness.
#[derive(Debug, Clone)]
struct Histogram {
    bins: Box<[Bin]>,
    count: u64,
    energy: f64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            bins: vec![Bin::default(); BINS].into_boxed_slice(),
            count: 0,
            energy: 0.0,
        }
    }

    fn clear(&mut self) {
        self.bins.fill(Bin::default());
        self.count = 0;
        self.energy = 0.0;
    }

    /// Adds a block of mean-square `energy` (weighted) unless it is gated out.
    fn add(&mut self, energy: f64) {
        let l = lufs(energy);
        if l <= ABSOLUTE_GATE {
            return;
        }
        let i = (((l - ABSOLUTE_GATE) / BIN_WIDTH) as usize).min(BINS - 1);
        self.bins[i].count += 1;
        self.bins[i].energy += energy;
        self.count += 1;
        self.energy += energy;
    }

    /// The first bin at or above the relative gate `gate` LU below the power mean, and
    /// whether the bin holding the gate itself is kept (its mean is above the gate).
    fn gated_from(&self, gate: f64) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let threshold = lufs(self.energy / self.count as f64) + gate;
        let at = ((threshold - ABSOLUTE_GATE) / BIN_WIDTH).floor();
        if at < 0.0 {
            return Some(0);
        }
        let at = at as usize;
        if at >= BINS {
            return Some(BINS);
        }
        let b = self.bins[at];
        let keep = b.count > 0 && lufs(b.energy / b.count as f64) > threshold;
        Some(if keep { at } else { at + 1 })
    }

    /// Integrated loudness: power mean of the blocks above the relative gate.
    fn integrated(&self) -> f64 {
        let Some(from) = self.gated_from(RELATIVE_GATE) else {
            return f64::NEG_INFINITY;
        };
        let (mut n, mut e) = (0u64, 0.0);
        for b in &self.bins[from.min(BINS)..] {
            n += b.count;
            e += b.energy;
        }
        if n == 0 {
            f64::NEG_INFINITY
        } else {
            lufs(e / n as f64)
        }
    }

    /// Loudness range: 95th minus 10th percentile of the values above the relative
    /// gate (Tech 3342). Negative infinity without any value.
    fn range(&self) -> f64 {
        let Some(from) = self.gated_from(RANGE_RELATIVE_GATE) else {
            return f64::NEG_INFINITY;
        };
        let bins = &self.bins[from.min(BINS)..];
        let n: u64 = bins.iter().map(|b| b.count).sum();
        if n == 0 {
            return 0.0;
        }
        // Nearest-rank on the sorted values, as the Tech 3342 MATLAB code.
        let rank = |p: f64| ((n - 1) as f64 * p).round() as u64;
        let value_at = |r: u64| {
            let mut seen = 0;
            for b in bins {
                seen += b.count;
                if seen > r {
                    return lufs(b.energy / b.count as f64);
                }
            }
            f64::NEG_INFINITY
        };
        (value_at(rank(0.95)) - value_at(rank(0.10))).max(0.0)
    }
}

/// BS.1770 loudness of a weighted mean square, in LUFS.
#[inline]
fn lufs(energy: f64) -> f64 {
    if energy > 0.0 {
        -0.691 + 10.0 * energy.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// A BS.1770-4 / EBU R 128 loudness meter for one audio format. See the module docs.
#[derive(Debug, Clone)]
pub struct LoudnessMeter {
    channels: usize,
    weights: [f64; MAX_CHANNELS],
    filter: [Biquad; 2],
    state: [KState; MAX_CHANNELS],
    true_peak: TruePeak,
    /// Frames per 10 ms step.
    step_frames: usize,
    /// Frames of the current step seen so far.
    step_pos: usize,
    /// Sum of squares of the current step, per channel.
    step_sum: [f64; MAX_CHANNELS],
    /// Weighted sums of squares of the last [`SHORT_TERM_STEPS`] steps (ring).
    steps: [f64; SHORT_TERM_STEPS],
    /// Next slot of `steps`.
    head: usize,
    /// Steps since the meter was made or its windows cleared (saturates).
    filled: usize,
    /// Steps since the last gating block (0 to `GATING_STEPS - 1`).
    since_gate: usize,
    gating: Histogram,
    short_terms: Histogram,
    peak: f32,
    values: Loudness,
}

impl LoudnessMeter {
    /// A meter for `format`. Allocates its histograms (about 32 KiB) and filter
    /// tables; call off the audio thread.
    pub fn new(format: AudioFormat) -> Self {
        let rate = format.sample_rate.hz();
        let channels = format.channels.channels().clamp(1, MAX_CHANNELS);
        Self {
            channels,
            weights: channel_weights(format.channels),
            filter: filter::k_weighting(f64::from(rate)),
            state: [KState::default(); MAX_CHANNELS],
            true_peak: TruePeak::new(rate),
            step_frames: ((f64::from(rate) / f64::from(STEPS_PER_SECOND)).round() as usize).max(1),
            step_pos: 0,
            step_sum: [0.0; MAX_CHANNELS],
            steps: [0.0; SHORT_TERM_STEPS],
            head: 0,
            filled: 0,
            since_gate: 0,
            gating: Histogram::new(),
            short_terms: Histogram::new(),
            peak: 0.0,
            values: Loudness::NONE,
        }
    }

    /// The number of channels measured.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The true-peak oversampling factor in use.
    pub fn oversampling(&self) -> usize {
        self.true_peak.factor()
    }

    /// Measures one block of planar channels, `frames` long. Channels beyond the
    /// meter's count are ignored; a missing or short channel reads as silence for the
    /// frames it lacks. Real-time safe.
    pub fn process(&mut self, channels: &[&[f32]], frames: usize) {
        let mut done = 0;
        while done < frames {
            let n = (self.step_frames - self.step_pos).min(frames - done);
            for c in 0..self.channels {
                let x = channels
                    .get(c)
                    .and_then(|ch| ch.get(done..(done + n).min(ch.len())))
                    .unwrap_or(&[]);
                self.step_sum[c] += self.state[c].sum_squares(&self.filter, x);
                let p = self.true_peak.peak(c, x);
                if p > self.peak {
                    self.peak = p;
                }
            }
            done += n;
            self.step_pos += n;
            if self.step_pos == self.step_frames {
                self.end_step();
            }
        }
        self.values.true_peak = db(self.peak);
    }

    fn end_step(&mut self) {
        let mut weighted = 0.0;
        for c in 0..self.channels {
            weighted += self.weights[c] * self.step_sum[c];
            self.step_sum[c] = 0.0;
        }
        self.step_pos = 0;
        self.steps[self.head] = weighted;
        self.head = (self.head + 1) % SHORT_TERM_STEPS;
        self.filled = (self.filled + 1).min(SHORT_TERM_STEPS);
        self.since_gate = (self.since_gate + 1) % GATING_STEPS;
        // Gating blocks and range values start one window after a clear and then
        // follow every 100 ms.
        let gate = self.since_gate == 0;
        let frames = self.step_frames as f64;
        if self.filled >= MOMENTARY_STEPS {
            let e = self.window(MOMENTARY_STEPS) / (MOMENTARY_STEPS as f64 * frames);
            let m = lufs(e) as f32;
            self.values.momentary = m;
            self.values.max_momentary = self.values.max_momentary.max(m);
            if gate {
                self.gating.add(e);
                self.values.integrated = self.gating.integrated() as f32;
            }
        }
        if self.filled >= SHORT_TERM_STEPS {
            let e = self.window(SHORT_TERM_STEPS) / (SHORT_TERM_STEPS as f64 * frames);
            let s = lufs(e) as f32;
            self.values.short_term = s;
            self.values.max_short_term = self.values.max_short_term.max(s);
            if gate {
                self.short_terms.add(e);
                self.values.range = self.short_terms.range() as f32;
            }
        }
    }

    /// Sum of the last `steps` step energies.
    fn window(&self, steps: usize) -> f64 {
        (1..=steps)
            .map(|k| self.steps[(self.head + SHORT_TERM_STEPS - k) % SHORT_TERM_STEPS])
            .sum()
    }

    /// The values now.
    pub fn loudness(&self) -> Loudness {
        self.values
    }

    /// Restarts the measurement: integrated loudness, range, maximum true peak, the
    /// maxima and the windows, so nothing heard before the reset counts (momentary
    /// and short-term read again once their windows have filled). The K-weighting
    /// states stay, as the signal itself continues; the true-peak interpolator starts
    /// empty, so samples before the reset cannot raise the new maximum. Real-time
    /// safe: clears in place.
    pub fn reset(&mut self) {
        self.true_peak.clear();
        self.step_pos = 0;
        self.step_sum = [0.0; MAX_CHANNELS];
        self.steps = [0.0; SHORT_TERM_STEPS];
        self.head = 0;
        self.filled = 0;
        self.since_gate = 0;
        self.values.momentary = f32::NEG_INFINITY;
        self.values.short_term = f32::NEG_INFINITY;
        self.gating.clear();
        self.short_terms.clear();
        self.peak = 0.0;
        self.values.integrated = f32::NEG_INFINITY;
        self.values.range = f32::NEG_INFINITY;
        self.values.true_peak = f32::NEG_INFINITY;
        self.values.max_momentary = f32::NEG_INFINITY;
        self.values.max_short_term = f32::NEG_INFINITY;
    }

    /// Resets everything, the filter states too, as for a new signal.
    pub fn clear(&mut self) {
        self.reset();
        self.state = [KState::default(); MAX_CHANNELS];
    }
}

/// Linear amplitude to dB; negative infinity for 0.
fn db(amplitude: f32) -> f32 {
    if amplitude > 0.0 {
        20.0 * amplitude.log10()
    } else {
        f32::NEG_INFINITY
    }
}
