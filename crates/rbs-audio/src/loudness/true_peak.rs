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
        for p in 0..factor {
            let sum: f64 = (0..TAPS).map(|k| proto[p + factor * k]).sum();
            for k in 0..TAPS {
                coeffs[p * TAPS + k] = (proto[p + factor * k] / sum) as f32;
            }
        }
        Self {
            factor,
            coeffs: coeffs.into_boxed_slice(),
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
    #[inline]
    pub(crate) fn peak(&mut self, channel: usize, x: &[f32]) -> f32 {
        if channel >= MAX_CHANNELS {
            return 0.0;
        }
        if self.factor == 1 {
            return x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        }
        let hist = &mut self.history[channel * 2 * TAPS..(channel + 1) * 2 * TAPS];
        let mut pos = self.pos[channel];
        let mut peak = 0.0f32;
        for &v in x {
            pos = if pos == 0 { TAPS - 1 } else { pos - 1 };
            hist[pos] = v;
            hist[pos + TAPS] = v;
            let window = &hist[pos..pos + TAPS];
            for phase in self.coeffs.chunks_exact(TAPS) {
                let y: f32 = phase.iter().zip(window).map(|(c, s)| c * s).sum();
                peak = peak.max(y.abs());
            }
        }
        self.pos[channel] = pos;
        peak
    }
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
}
