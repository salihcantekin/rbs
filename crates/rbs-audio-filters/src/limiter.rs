//! Look-ahead limiter (ADR-0130 decision 7). Latency: the look-ahead `W`, 2 ms rounded
//! to whole frames (96 at 48 kHz).
//!
//! Per input sample `j`: the gain the sample needs, `g = min(1, ceiling / max_ch |x|)`;
//! a release stage `g'` that limits how fast the gain may rise and never exceeds `g`;
//! `m[j]`, the minimum of `g'` over `[j - W, j]` (a monotonic queue); and the gain for
//! output sample `n` (input `n - W`), the mean of `m` over `[n - W + 1, n]`. Every one of
//! those windows holds sample `n - W`, so the mean is at most the gain that sample needs
//! and no output sample exceeds the ceiling. A final clamp stays as a guard and counts
//! if it ever acts.

use std::sync::Arc;

use rbs_core::media::MAX_CHANNELS;
use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter, amp_to_db, db_to_amp};

/// Type id of [`Limiter`].
pub const LIMITER: &str = "org.rbs.audio-filter.limiter";

/// The look-ahead in seconds.
const LOOKAHEAD_SECONDS: f64 = 0.002;

/// The gain stays this fraction below what a sample needs, so float rounding in the
/// mean and the product never lifts an output sample over the ceiling (about
/// -0.00001 dB).
const SAFETY: f32 = 1.0 - 1.0 / 1_048_576.0;

/// A released gain this close to 1 is 1, so a recovered limiter is an exact delay.
const SNAP: f32 = 1.0e-6;

const CEILING: usize = 0;
const RELEASE: usize = 1;

const FIELDS: [Field; 2] = [
    Field::number("ceiling", "Ceiling", -30.0, 0.0, -1.0, 0.1, "dB")
        .with_help("No sample of this filter's output goes above this level."),
    Field::number("release", "Release", 1.0, 1000.0, 60.0, 1.0, "ms")
        .log()
        .with_help("How fast the level comes back after a peak. Adds 2 ms of delay."),
];

/// The look-ahead in frames at `rate` Hz.
fn lookahead(rate: u32) -> usize {
    ((f64::from(rate) * LOOKAHEAD_SECONDS).round() as usize).max(1)
}

/// The Limiter filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limiter;

impl AudioFilterType for Limiter {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(LIMITER)
    }
    fn name(&self) -> &'static str {
        "Limiter"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(LimiterDsp::new(setup, values)))
    }
    fn latency_frames(&self, setup: DspSetup) -> u32 {
        lookahead(setup.format.sample_rate.hz()) as u32
    }
}

/// The limiter DSP.
#[derive(Debug)]
pub struct LimiterDsp {
    rate: f32,
    /// The look-ahead `W` in frames.
    w: usize,
    /// The ceiling as an amplitude, times [`SAFETY`].
    ceiling: f32,
    /// The ceiling as an amplitude: what the guard clamps to.
    hard_ceiling: f32,
    /// Per sample, the factor the released gain's distance below 1 may shrink by.
    keep: f32,
    /// `1 - g'` of the previous sample.
    deficit: f32,
    /// Sample index of the next input.
    n: u64,
    /// Monotonic queue of `(index, g')`, values rising from `head` to `tail`
    /// (counters; the slot is the counter masked by the power-of-two length, which is at
    /// least `W + 1`).
    queue: Vec<(u64, f32)>,
    head: usize,
    tail: usize,
    /// The last `W` values of `m`, a ring; `pos` is the oldest.
    mins: Vec<f32>,
    pos: usize,
    /// Sum of `mins`.
    sum: f64,
    /// Frames since `sum` was recomputed from `mins`.
    since_sum: usize,
    /// Per channel, the `W` delayed input samples in order (oldest first).
    delay: Vec<Vec<f32>>,
    /// Per frame of a part: the needed gain, then the applied gain.
    work: Vec<f32>,
    /// A part of one channel's input.
    scratch: Vec<f32>,
    guard_hits: u64,
    meter: Arc<FilterMeter>,
}

impl LimiterDsp {
    /// A limiter with `values` for `setup`, at rest (no reduction, silent delay).
    pub fn new(setup: DspSetup, values: &Values) -> Self {
        let rate = setup.format.sample_rate.hz();
        let w = lookahead(rate);
        let channels = setup.format.channels.channels().clamp(1, MAX_CHANNELS);
        let part = (setup.block_frames as usize).max(1);
        let mut dsp = Self {
            rate: rate as f32,
            w,
            ceiling: 1.0,
            hard_ceiling: 1.0,
            keep: 0.0,
            deficit: 0.0,
            n: 0,
            queue: vec![(0, 1.0); (w + 1).next_power_of_two()],
            head: 0,
            tail: 0,
            mins: vec![1.0; w],
            pos: 0,
            sum: w as f64,
            since_sum: 0,
            delay: vec![vec![0.0; w]; channels],
            work: vec![0.0; part],
            scratch: vec![0.0; part],
            guard_hits: 0,
            meter: Arc::new(FilterMeter::default()),
        };
        dsp.set(values);
        dsp
    }

    /// How often the guard clamp acted since the DSP was made (tests require 0).
    pub fn guard_hits(&self) -> u64 {
        self.guard_hits
    }

    /// The look-ahead in frames.
    pub fn lookahead_frames(&self) -> usize {
        self.w
    }

    /// The gain applied to `w` frames, from their needed gains, in place.
    fn gains(&mut self, w: &mut [f32]) {
        let window = self.w as u64;
        if self.deficit == 0.0 && self.sum == window as f64 && w.iter().all(|&d| d >= 1.0) {
            // At rest and nothing to limit in this part: every gain is 1 and the state
            // stays at rest (the queue holds only the newest sample, `mins` all 1).
            w.fill(1.0);
            let last = self.n + w.len() as u64 - 1;
            self.head = 0;
            self.tail = 1;
            self.queue[0] = (last, 1.0);
            self.n = last + 1;
            return;
        }
        let mask = self.queue.len() - 1;
        let inv = 1.0 / self.w as f64;
        let (keep, mut deficit_state) = (self.keep, self.deficit);
        let (mut head, mut tail) = (self.head, self.tail);
        let (mut pos, mut sum, mut since) = (self.pos, self.sum, self.since_sum);
        let mut n = self.n;
        for d in w.iter_mut() {
            // Release: rise toward 1 by at most `1 - keep` of the way, never above `g`.
            // Kept as the distance below 1, which decays without stalling in f32
            // rounding.
            let mut deficit = deficit_state * keep;
            if deficit < SNAP {
                deficit = 0.0;
            }
            let released = 1.0 - deficit;
            // The needed gain itself when it is lower, not `1 - (1 - g)`, which would
            // lose precision for a deep reduction.
            let g = if *d < released {
                deficit_state = 1.0 - *d;
                *d
            } else {
                deficit_state = deficit;
                released
            };
            // Sliding minimum over [n - W, n] in a ring of at least W + 1 entries:
            // drop what left the window, then larger values from the back.
            while head != tail && self.queue[head & mask].0 + window < n {
                head = head.wrapping_add(1);
            }
            while head != tail && self.queue[tail.wrapping_sub(1) & mask].1 >= g {
                tail = tail.wrapping_sub(1);
            }
            self.queue[tail & mask] = (n, g);
            tail = tail.wrapping_add(1);
            let m = self.queue[head & mask].1;
            // Mean of m over [n - W + 1, n].
            sum += f64::from(m) - f64::from(self.mins[pos]);
            self.mins[pos] = m;
            pos += 1;
            if pos == self.w {
                pos = 0;
            }
            since += 1;
            if since >= self.w {
                // Recompute so rounding never accumulates.
                since = 0;
                sum = self.mins.iter().map(|&v| f64::from(v)).sum();
            }
            *d = (sum * inv) as f32;
            n += 1;
        }
        self.deficit = deficit_state;
        (self.head, self.tail) = (head, tail);
        (self.pos, self.sum, self.since_sum) = (pos, sum, since);
        self.n = n;
    }

    fn run(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        let mut work = std::mem::take(&mut self.work);
        let mut scratch = std::mem::take(&mut self.scratch);
        let part = work.len().min(scratch.len()).max(1);
        let w_frames = self.w;
        let mut peak_bits = 0u32;
        let mut lowest_bits = 1.0f32.to_bits();
        let mut start = 0;
        while start < frames {
            let end = (start + part).min(frames);
            let n = end - start;
            let wk = &mut work[..n];
            // The largest magnitude per frame; NaN and infinity count as silence.
            wk.fill(0.0);
            for ch in channels.iter() {
                let ch = ch.get(start..end.min(ch.len())).unwrap_or(&[]);
                for (d, x) in wk.iter_mut().zip(ch) {
                    let a = x.abs();
                    let a = if a < f32::INFINITY { a } else { 0.0 };
                    *d = d.max(a);
                }
            }
            // Non-negative floats order like their bits; an integer max vectorises.
            peak_bits = wk.iter().fold(peak_bits, |m, d| m.max(d.to_bits()));
            for d in wk.iter_mut() {
                *d = if *d > self.ceiling {
                    self.ceiling / *d
                } else {
                    1.0
                };
            }
            self.gains(wk);
            lowest_bits = wk.iter().fold(lowest_bits, |m, g| m.min(g.to_bits()));
            // Delay each channel by W and apply the gain.
            for (ci, ch) in channels.iter_mut().enumerate() {
                let len = ch.len();
                let ch = ch.get_mut(start..end.min(len)).unwrap_or(&mut []);
                let m = ch.len();
                let Some(line) = self.delay.get_mut(ci) else {
                    // More channels than the setup: never happens (the node checks the
                    // format); keep them silent rather than undelayed.
                    ch.fill(0.0);
                    continue;
                };
                let tmp = &mut scratch[..m];
                for (t, x) in tmp.iter_mut().zip(ch.iter()) {
                    *t = if x.is_finite() { *x } else { 0.0 };
                }
                if m >= w_frames {
                    ch[..w_frames].copy_from_slice(line);
                    ch[w_frames..].copy_from_slice(&tmp[..m - w_frames]);
                    line.copy_from_slice(&tmp[m - w_frames..]);
                } else {
                    ch.copy_from_slice(&line[..m]);
                    line.rotate_left(m);
                    line[w_frames - m..].copy_from_slice(tmp);
                }
                let mut top = 0u32;
                for (x, g) in ch.iter_mut().zip(wk.iter()) {
                    let y = *x * g;
                    let y = if y.is_normal() { y } else { 0.0 };
                    top = top.max(y.abs().to_bits());
                    *x = y;
                }
                // The guard: never expected to act (tests require 0); only a part with a
                // sample over the ceiling pays for clamping and counting.
                let limit = self.hard_ceiling;
                if top > limit.to_bits() {
                    for x in ch.iter_mut() {
                        let c = x.clamp(-limit, limit);
                        self.guard_hits += u64::from(c != *x);
                        *x = c;
                    }
                }
            }
            start = end;
        }
        self.work = work;
        self.scratch = scratch;
        let (peak, lowest) = (f32::from_bits(peak_bits), f32::from_bits(lowest_bits));
        self.meter.store(amp_to_db(peak), -amp_to_db(lowest));
    }
}

impl AudioDsp for LimiterDsp {
    fn set(&mut self, values: &Values) {
        let ceiling = db_to_amp(values.number(CEILING).clamp(-30.0, 0.0));
        self.hard_ceiling = ceiling;
        self.ceiling = ceiling * SAFETY;
        let release_s = values.number(RELEASE).clamp(1.0, 1000.0) * 0.001;
        self.keep = (-1.0 / (release_s * self.rate)).exp();
    }

    fn reset(&mut self) {
        self.deficit = 0.0;
        self.n = 0;
        self.head = 0;
        self.tail = 0;
        self.mins.fill(1.0);
        self.pos = 0;
        self.sum = self.w as f64;
        self.since_sum = 0;
        for line in &mut self.delay {
            line.fill(0.0);
        }
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        self.run(channels, frames);
    }

    fn meter(&self) -> Option<Arc<FilterMeter>> {
        Some(Arc::clone(&self.meter))
    }

    fn latency_frames(&self) -> u32 {
        self.w as u32
    }
}
