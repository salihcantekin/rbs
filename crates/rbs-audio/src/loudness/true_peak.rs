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
/// The most phases (the factor below 96 kHz).
const LANES: usize = 4;
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
    /// Per tap (newest sample first), the coefficient of each phase; lanes past
    /// `factor` are 0. Tap-major, so one sample feeds all phases in one vector step.
    coeffs: [[f32; LANES]; TAPS],
    /// The largest gain any phase can have (the largest sum of absolute
    /// coefficients), with a margin for rounding: no interpolated value of a block
    /// exceeds its sample peak times this.
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
        let mut coeffs = [[0.0f32; LANES]; TAPS];
        let mut l1 = 0.0f64;
        for p in 0..factor.min(LANES) {
            let sum: f64 = (0..TAPS).map(|k| proto[p + factor * k]).sum();
            let mut abs = 0.0;
            for (k, tap) in coeffs.iter_mut().enumerate() {
                tap[p] = (proto[p + factor * k] / sum) as f32;
                abs += f64::from(tap[p].abs());
            }
            l1 = l1.max(abs);
        }
        Self {
            factor,
            coeffs,
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
    /// the filter's largest gain, is above `floor`, so a meter that keeps its maximum skips the work exactly: the
    /// skipped values could not have raised it. Real-time safe.
    #[inline]
    pub(crate) fn peak_above(&mut self, channel: usize, x: &[f32], floor: f32) -> f32 {
        if channel >= MAX_CHANNELS {
            return 0.0;
        }
        let sample_peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        if self.factor == 1 {
            return sample_peak;
        }
        let hist = &mut self.history[channel * 2 * TAPS..(channel + 1) * 2 * TAPS];
        let mut pos = self.pos[channel];
        // The first windows of the block still reach into the previous block's
        // samples, so they count towards the bound as well.
        let reach = hist[..TAPS].iter().fold(sample_peak, |m, v| m.max(v.abs()));
        if reach * self.bound <= floor {
            // Only the history moves: the last `TAPS` samples are all a later block
            // reads.
            let skip = x.len().saturating_sub(TAPS);
            pos = (pos + TAPS - skip % TAPS) % TAPS;
            for &v in &x[skip..] {
                pos = if pos == 0 { TAPS - 1 } else { pos - 1 };
                hist[pos] = v;
                hist[pos + TAPS] = v;
            }
            self.pos[channel] = pos;
            return sample_peak;
        }
        let peak = if self.factor == 2 {
            interpolate::<2>(&self.coeffs, hist, &mut pos, x)
        } else {
            interpolate::<LANES>(&self.coeffs, hist, &mut pos, x)
        };
        self.pos[channel] = pos;
        peak
    }
}

/// Runs the `P` phases over `x`, updating the history ring at `pos`, and returns the
/// highest absolute interpolated value. Each phase adds its taps in the same order as a
/// plain dot product, so the values are bit-exact; the phases advance together, one
/// sample feeding all of them in one vector step.
#[inline(always)]
fn interpolate<const P: usize>(
    coeffs: &[[f32; LANES]; TAPS],
    hist: &mut [f32],
    pos: &mut usize,
    x: &[f32],
) -> f32 {
    let mut peak = 0.0f32;
    let mut at = *pos;
    for &v in x {
        at = if at == 0 { TAPS - 1 } else { at - 1 };
        hist[at] = v;
        hist[at + TAPS] = v;
        let Some(window) = hist.get(at..at + TAPS) else {
            break;
        };
        let mut acc = [0.0f32; P];
        for (tap, &s) in coeffs.iter().zip(window) {
            for l in 0..P {
                acc[l] += tap[l] * s;
            }
        }
        for a in acc {
            let a = a.abs();
            peak = if a > peak { a } else { peak };
        }
    }
    *pos = at;
    peak
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
