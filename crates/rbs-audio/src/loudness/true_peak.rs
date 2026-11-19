//! True-peak level by polyphase oversampling (ITU-R BS.1770-4, Annex 2).
//!
//! The factor depends on the rate: 4 below 96 kHz, 2 below 192 kHz, 1 above (the
//! sample peak), so the interpolated rate is always at least 176.4 kHz. The
//! interpolation filter is a Kaiser-windowed sinc with 12 taps per phase (48 at 4x, the
//! length of the BS.1770 example filter), designed for the factor when the meter is
//! made. Each phase is normalised to unity gain at DC.

use rbs_core::media::MAX_CHANNELS;

/// Taps per phase of the interpolation filter.
pub(crate) const TAPS: usize = 12;
/// Kaiser window shape.
const BETA: f64 = 7.0;

/// The oversampling factor for `rate` Hz.
pub(crate) fn factor(rate: u32) -> usize {
    if rate < 96_000 {
        4
    } else if rate < 192_000 {
        2
    } else {
        1
    }
}

/// Oversampling true-peak detector for up to [`MAX_CHANNELS`] channels.
#[derive(Debug, Clone)]
pub(crate) struct TruePeak {
    factor: usize,
    /// `factor` phases of `TAPS` coefficients, phase-major, newest sample first.
    coeffs: Box<[f32]>,
    /// The largest gain any phase can have (the largest sum of absolute
    /// coefficients), with a margin for rounding: no interpolated value exceeds the
    /// largest absolute sample in its window times this.
    bound: f32,
    /// Per channel, the last `TAPS` samples written twice (`2 * TAPS`), so a window
    /// is always one contiguous slice.
    history: Box<[f32]>,
    pos: [usize; MAX_CHANNELS],
}

impl TruePeak {
    /// A detector for `rate` Hz. Allocates; call off the audio thread.
    pub(crate) fn new(rate: u32) -> Self {
        let factor = factor(rate);
        let n = factor * TAPS;
        let centre = (n as f64 - 1.0) / 2.0;
        let i0_beta = bessel_i0(BETA);
        let proto: Vec<f64> = (0..n)
            .map(|i| {
                let t = (i as f64 - centre) / factor as f64;
                let r = (i as f64 - centre) / centre.max(1.0);
                let w = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
                sinc(t) * w
            })
            .collect();
        let mut coeffs = vec![0.0f32; n];
        let mut l1 = 0.0f64;
        for p in 0..factor {
            let sum: f64 = (0..TAPS).map(|k| proto[p + factor * k]).sum();
            let mut abs = 0.0;
            for k in 0..TAPS {
                coeffs[p * TAPS + k] = (proto[p + factor * k] / sum) as f32;
                abs += f64::from(coeffs[p * TAPS + k].abs());
            }
            l1 = l1.max(abs);
        }
        Self {
            factor,
            coeffs: coeffs.into_boxed_slice(),
            // f32 rounding of a 12-term sum stays far below 1e-4 relative.
            bound: (l1 * (1.0 + 1e-4)) as f32,
            history: vec![0.0; MAX_CHANNELS * 2 * TAPS].into_boxed_slice(),
            pos: [0; MAX_CHANNELS],
        }
    }

    /// The oversampling factor.
    pub(crate) fn factor(&self) -> usize {
        self.factor
    }

    /// Forgets the signal history.
    pub(crate) fn clear(&mut self) {
        self.history.fill(0.0);
        self.pos = [0; MAX_CHANNELS];
    }

    /// The highest absolute interpolated value of `x` on `channel`. Real-time safe.
    #[cfg(test)]
    pub(crate) fn peak(&mut self, channel: usize, x: &[f32]) -> f32 {
        self.peak_above(channel, x, 0.0)
    }

    /// The highest absolute interpolated value of `x` on `channel`, or a value no
    /// larger than `floor` when the block cannot exceed `floor`. The interpolation
    /// runs only when the sample peak of the block and the history before it, times
    /// the filter's largest gain, is above `floor`, so a meter that keeps its
    /// maximum skips the work exactly: the skipped values could not have raised it.
    /// Real-time safe.
    #[inline]
    pub(crate) fn peak_above(&mut self, channel: usize, x: &[f32], floor: f32) -> f32 {
        if channel >= MAX_CHANNELS {
            return 0.0;
        }
        if self.factor == 1 {
            return x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        }
        let hist = &mut self.history[channel * 2 * TAPS..(channel + 1) * 2 * TAPS];
        let mut pos = self.pos[channel];
        if floor > 0.0 {
            // The first windows of the block still reach into the previous block's
            // samples, so they count towards the bound as well.
            let reach = abs_max(x).max(abs_max(&hist[..TAPS]));
            if reach * self.bound <= floor {
                // Only the history moves: the last `TAPS` samples are all a later
                // block reads.
                let skip = x.len().saturating_sub(TAPS);
                pos = (pos + TAPS - skip % TAPS) % TAPS;
                for &v in &x[skip..] {
                    pos = if pos == 0 { TAPS - 1 } else { pos - 1 };
                    hist[pos] = v;
                    hist[pos + TAPS] = v;
                }
                self.pos[channel] = pos;
                return reach;
            }
        }
        let mut peak = 0.0f32;
        for &v in x {
            pos = if pos == 0 { TAPS - 1 } else { pos - 1 };
            hist[pos] = v;
            hist[pos + TAPS] = v;
            let window = &hist[pos..pos + TAPS];
            for phase in self.coeffs.chunks_exact(TAPS) {
                let y = dot12(phase, window);
                peak = peak.max(y.abs());
            }
        }
        self.pos[channel] = pos;
        peak
    }
}

/// Product sum of one 12-tap phase with the sample window, in f32. Both slices are
/// `TAPS` long (`chunks_exact` and the mirrored history). A short slice adds nothing.
#[inline]
fn dot12(weights: &[f32], input: &[f32]) -> f32 {
    if weights.len() < TAPS || input.len() < TAPS {
        return 0.0;
    }
    let mut acc = [0.0f32; 4];
    for k in 0..3 {
        let i = k * 4;
        acc[0] += weights[i] * input[i];
        acc[1] += weights[i + 1] * input[i + 1];
        acc[2] += weights[i + 2] * input[i + 2];
        acc[3] += weights[i + 3] * input[i + 3];
    }
    (acc[0] + acc[1]) + (acc[2] + acc[3])
}

/// The largest absolute value of `x` (NaN ignored), in 8 lanes so it vectorises.
#[inline]
fn abs_max(x: &[f32]) -> f32 {
    let mut lanes = [0.0f32; 8];
    let mut chunks = x.chunks_exact(8);
    for c in &mut chunks {
        for (m, v) in lanes.iter_mut().zip(c) {
            let a = v.abs();
            *m = if a > *m { a } else { *m };
        }
    }
    let rest = chunks
        .remainder()
        .iter()
        .fold(0.0f32, |m, v| m.max(v.abs()));
    lanes.into_iter().fold(rest, f32::max)
}

fn sinc(t: f64) -> f64 {
    if t.abs() < 1e-12 {
        1.0
    } else {
        let x = std::f64::consts::PI * t;
        x.sin() / x
    }
}

/// The modified Bessel function of the first kind, order 0 (series; converges fast for
/// the window's arguments).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_factor_keeps_the_interpolated_rate_high() {
        assert_eq!(factor(44_100), 4);
        assert_eq!(factor(48_000), 4);
        assert_eq!(factor(96_000), 2);
        assert_eq!(factor(192_000), 1);
    }

    #[test]
    fn each_phase_has_unity_gain_at_dc() {
        let mut tp = TruePeak::new(48_000);
        let dc = [0.5f32; 64];
        // The step into DC rings; once the history holds only DC, every phase is 0.5.
        let _ = tp.peak(0, &dc);
        let p = tp.peak(0, &dc);
        assert!((p - 0.5).abs() < 1e-5, "{p}");
    }

    #[test]
    fn a_sample_rate_of_192_khz_reads_the_sample_peak() {
        let mut tp = TruePeak::new(192_000);
        assert_eq!(tp.peak(1, &[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(tp.peak(MAX_CHANNELS, &[1.0]), 0.0);
    }

    /// A deterministic test signal with inter-sample peaks (a tone near fs/4).
    fn tone(len: usize, amp: f32, phase: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (i as f32 * 1.4 + phase).sin())
            .collect()
    }

    #[test]
    fn a_skipped_block_keeps_the_history_of_a_computed_one() {
        let mut full = TruePeak::new(48_000);
        let mut skipping = TruePeak::new(48_000);
        for (len, amp) in [
            (480, 0.9),
            (7, 0.1),
            (480, 0.2),
            (300, 0.95),
            (5, 0.3),
            (480, 0.99),
        ] {
            let x = tone(len, amp, len as f32);
            let want = full.peak(0, &x);
            // A high floor skips the quiet blocks; the loud ones still compute.
            let got = skipping.peak_above(0, &x, 0.5);
            if want > 0.5 {
                assert_eq!(got, want, "len {len} amp {amp}");
            } else {
                assert!(got <= 0.5, "len {len} amp {amp}: {got}");
            }
            assert_eq!(full.history, skipping.history);
            assert_eq!(full.pos, skipping.pos);
        }
    }

    #[test]
    fn the_bound_covers_every_interpolated_value() {
        let mut tp = TruePeak::new(44_100);
        for seed in 0..20 {
            let x = tone(480, 1.0, seed as f32 * 0.37);
            let sample_peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let p = tp.peak(0, &x);
            assert!(
                p <= sample_peak * tp.bound,
                "{p} > {sample_peak} * {}",
                tp.bound
            );
        }
    }
}
