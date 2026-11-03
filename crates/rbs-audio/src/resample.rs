//! A variable-ratio polyphase windowed-sinc kernel (audio graph spec "Resampler").
//!
//! The input node steps through its FIFO by a fractional position per output frame;
//! for each output frame it asks [`Kernel::coefficients`] for the `TAPS` weights at the
//! fractional part of the position and applies them to the input frames around it. The
//! kernel is computed once (allocation on the control thread); looking up weights
//! interpolates linearly between two of its `PHASES + 1` phases and never allocates.

use rbs_core::timing::SampleRate;

/// Taps per output frame. Even; the window spans `TAPS / 2` input frames either side.
pub const TAPS: usize = 64;
/// Phases of the kernel table (fractional positions between two input frames).
pub const PHASES: usize = 256;
/// Kaiser window shape (about 70 dB stop-band attenuation).
pub const BETA: f64 = 7.0;
/// Width of the transition band, in cycles per input frame (3.4 kHz at 48 kHz).
pub const TRANSITION: f64 = 0.07;

/// Input frames the window reaches before the position's integer part.
pub const BEFORE: usize = TAPS / 2 - 1;
/// Input frames the window reaches after the position's integer part.
pub const AFTER: usize = TAPS / 2;

/// The filter for one rate pair.
pub struct Kernel {
    /// `(PHASES + 1) x TAPS` weights; phase `j` is for fractional position `j / PHASES`.
    table: Box<[f32]>,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel").finish_non_exhaustive()
    }
}

/// Modified Bessel function of the first kind, order 0 (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        let f = half / k as f64;
        term *= f * f;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Kernel {
    /// The cutoff in cycles per input frame for converting `source` to `target`: the
    /// lower Nyquist frequency minus half the transition band, so the stop band starts
    /// at the lower Nyquist frequency and nothing above it aliases.
    pub fn cutoff(source: SampleRate, target: SampleRate) -> f64 {
        let ratio = f64::from(target.hz()) / f64::from(source.hz());
        (0.5 * ratio.min(1.0) - TRANSITION / 2.0).max(0.01)
    }

    /// The kernel for converting `source` to `target` (control thread: allocates).
    pub fn new(source: SampleRate, target: SampleRate) -> Self {
        let fc = Self::cutoff(source, target);
        let radius = (TAPS / 2) as f64;
        let i0_beta = bessel_i0(BETA);
        let mut table = vec![0.0f32; (PHASES + 1) * TAPS].into_boxed_slice();
        let mut phase = [0.0f64; TAPS];
        for j in 0..=PHASES {
            let frac = j as f64 / PHASES as f64;
            let mut sum = 0.0;
            for (k, w) in phase.iter_mut().enumerate() {
                let d = k as f64 - BEFORE as f64 - frac;
                let x = 2.0 * fc * d;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let r = d / radius;
                let window = if r.abs() <= 1.0 {
                    bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0_beta
                } else {
                    0.0
                };
                *w = 2.0 * fc * sinc * window;
                sum += *w;
            }
            // Unit gain at DC for every phase.
            for (k, w) in phase.iter().enumerate() {
                table[j * TAPS + k] = (*w / sum) as f32;
            }
        }
        Self { table }
    }

    /// The weights for an output frame at fractional position `frac` (`0.0..1.0`) past
    /// an input frame: weight `k` applies to input frame `floor(position) - BEFORE + k`.
    #[inline]
    pub fn coefficients(&self, frac: f64, out: &mut [f32; TAPS]) {
        let x = frac.clamp(0.0, 1.0) * PHASES as f64;
        let j = (x as usize).min(PHASES - 1);
        let t = (x - j as f64) as f32;
        let a = &self.table[j * TAPS..(j + 1) * TAPS];
        let b = &self.table[(j + 1) * TAPS..(j + 2) * TAPS];
        for ((o, &a), &b) in out.iter_mut().zip(a).zip(b) {
            *o = a + (b - a) * t;
        }
    }
}

/// Applies `weights` to `input[start..start + TAPS]`.
#[inline]
pub fn apply(weights: &[f32; TAPS], input: &[f32]) -> f32 {
    let mut acc = [0.0f32; 4];
    for (w, x) in weights.chunks_exact(4).zip(input.chunks_exact(4)) {
        acc[0] += w[0] * x[0];
        acc[1] += w[1] * x[1];
        acc[2] += w[2] * x[2];
        acc[3] += w[3] * x[3];
    }
    (acc[0] + acc[1]) + (acc[2] + acc[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resamples a tone of `hz` from `from` to `to` at a fixed ratio and returns the
    /// RMS of the output, away from the edges.
    fn rms_after(hz: f64, from: SampleRate, to: SampleRate) -> f64 {
        let kernel = Kernel::new(from, to);
        let n_in = from.hz() as usize; // one second
        let input: Vec<f32> = (0..n_in)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / f64::from(from.hz())).sin() as f32)
            .collect();
        let step = f64::from(from.hz()) / f64::from(to.hz());
        let mut w = [0.0f32; TAPS];
        let mut p = 1000.0;
        let (mut sum, mut n) = (0.0f64, 0usize);
        while (p as usize) + AFTER + 1000 < n_in {
            let i = p as usize;
            kernel.coefficients(p - i as f64, &mut w);
            let y = apply(&w, &input[i - BEFORE..i - BEFORE + TAPS]);
            sum += f64::from(y) * f64::from(y);
            n += 1;
            p += step;
        }
        (sum / n as f64).sqrt()
    }

    fn db(rms: f64) -> f64 {
        20.0 * (rms * std::f64::consts::SQRT_2).log10()
    }

    #[test]
    fn a_1_khz_tone_keeps_its_level_both_ways() {
        let a = db(rms_after(
            1000.0,
            SampleRate::HZ_44100,
            SampleRate::HZ_48000,
        ));
        let b = db(rms_after(
            1000.0,
            SampleRate::HZ_48000,
            SampleRate::HZ_44100,
        ));
        let c = db(rms_after(
            1000.0,
            SampleRate::HZ_48000,
            SampleRate::HZ_48000,
        ));
        for level in [a, b, c] {
            assert!(level.abs() < 0.1, "{level} dB");
        }
    }

    #[test]
    fn the_pass_band_reaches_18_khz() {
        let level = db(rms_after(
            18_000.0,
            SampleRate::HZ_48000,
            SampleRate::HZ_44100,
        ));
        assert!(level.abs() < 0.5, "{level} dB");
    }

    #[test]
    fn tones_that_would_alias_are_attenuated_by_60_db() {
        // 23 kHz at 48 kHz input lies above the 22.05 kHz output Nyquist frequency.
        let level = db(rms_after(
            23_000.0,
            SampleRate::HZ_48000,
            SampleRate::HZ_44100,
        ));
        assert!(level < -60.0, "{level} dB");
        // Downsampling 96 to 48 kHz: 30 kHz must not fold to 18 kHz.
        let level = db(rms_after(
            30_000.0,
            SampleRate::HZ_96000,
            SampleRate::HZ_48000,
        ));
        assert!(level < -60.0, "{level} dB");
    }

    #[test]
    fn every_phase_has_unit_dc_gain() {
        let k = Kernel::new(SampleRate::HZ_44100, SampleRate::HZ_48000);
        let mut w = [0.0f32; TAPS];
        for j in 0..=100 {
            k.coefficients(j as f64 / 100.0, &mut w);
            let s: f32 = w.iter().sum();
            assert!((s - 1.0).abs() < 1e-5, "{s}");
        }
    }
}
