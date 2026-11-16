//! Structures for computing audio features.
//!
//! This module contains utilities for computing features of an audio signal. These features are
//! used in two ways: they can be fed into a trained neural net for noise removal and speech
//! detection, or when the `train` feature is enabled they can be collected and used to train new
//! neural nets.

// Upstream style is kept so the copy stays diffable (README.md); these lints are style only.
#![allow(
    clippy::identity_op,
    clippy::manual_div_ceil,
    clippy::needless_borrow,
    clippy::needless_range_loop,
    clippy::neg_cmp_op_on_partial_ord,
    clippy::redundant_slicing
)]

// Vendored from nnnoiseless 0.5.2 (BSD-3-Clause, see LICENSE-RNNOISE). Patched: the
// spectra are plain `[Complex; FREQ_SIZE]` arrays transformed by the denoiser's own
// realfft plans (no easyfft thread-local planner), the tables come from `Tables`, and
// the training-only API is removed. See README.md.

use crate::{
    CEPS_MEM, Complex, FRAME_SIZE, FREQ_SIZE, Fft, NB_BANDS, NB_DELTA_CEPS, NB_FEATURES,
    PITCH_BUF_SIZE, PITCH_MAX_PERIOD, Tables, WINDOW_SIZE,
};

/// Contains the necessary state to compute the features of audio input and synthesize the output.
///
/// This is quite a large struct and should probably be kept behind some kind of pointer.
pub(crate) struct DenoiseFeatures {
    /// This stores some of the previous input. Currently, whenever we get new input we shift this
    /// backwards and copy the new input at the end. It might be worth investigating a ring buffer.
    input_mem: [f32; max(FRAME_SIZE, PITCH_BUF_SIZE)],
    /// This is some sort of ring buffer, storing the last bunch of cepstra.
    cepstral_mem: [[f32; crate::NB_BANDS]; crate::CEPS_MEM],
    /// The index pointing to the most recent cepstrum in `cepstral_mem`. The previous cepstra are
    /// at indices mem_id - 1, mem_id - 2, etc (wrapped appropriately).
    mem_id: usize,
    mem_hp_x: [f32; 2],
    synthesis_mem: [f32; FRAME_SIZE],
    window_buf: [f32; WINDOW_SIZE],

    // What follows are various buffers. The names are cryptic, but they follow a pattern.
    /// The Fourier transform of the most recent frame of input.
    x: [Complex; FREQ_SIZE],
    /// The Fourier transform of a pitch-period-shifted window of input.
    p: [Complex; FREQ_SIZE],
    /// The band energies of `x` (the signal).
    ex: [f32; NB_BANDS],
    /// The band energies of `p` (the signal, lagged by one pitch period).
    ep: [f32; NB_BANDS],
    /// The band correlations between `x` (the signal) and `p` (the pitch-period-lagged signal).
    exp: [f32; NB_BANDS],
    /// The computed features.
    features: [f32; NB_FEATURES],

    pitch_finder: crate::pitch::PitchFinder,
}

const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

impl DenoiseFeatures {
    /// Creates a new, empty, `DenoiseFeatures`.
    pub(crate) fn new() -> DenoiseFeatures {
        DenoiseFeatures {
            input_mem: [0.0; max(FRAME_SIZE, PITCH_BUF_SIZE)],
            cepstral_mem: [[0.0; NB_BANDS]; CEPS_MEM],
            mem_id: 0,
            mem_hp_x: [0.0; 2],
            synthesis_mem: [0.0; FRAME_SIZE],
            window_buf: [0.0; WINDOW_SIZE],
            x: [Complex::default(); FREQ_SIZE],
            p: [Complex::default(); FREQ_SIZE],
            ex: [0.0; NB_BANDS],
            ep: [0.0; NB_BANDS],
            exp: [0.0; NB_BANDS],
            features: [0.0; NB_FEATURES],
            pitch_finder: crate::pitch::PitchFinder::new(),
        }
    }

    /// Back to a silent history, allocating nothing.
    pub(crate) fn reset(&mut self) {
        self.input_mem.fill(0.0);
        self.cepstral_mem = [[0.0; NB_BANDS]; CEPS_MEM];
        self.mem_id = 0;
        self.mem_hp_x = [0.0; 2];
        self.synthesis_mem.fill(0.0);
        self.window_buf.fill(0.0);
        self.x.fill(Complex::default());
        self.p.fill(Complex::default());
        self.ex = [0.0; NB_BANDS];
        self.ep = [0.0; NB_BANDS];
        self.exp = [0.0; NB_BANDS];
        self.features = [0.0; NB_FEATURES];
        self.pitch_finder.reset();
    }

    /// Returns the computed features.
    pub(crate) fn features(&self) -> &[f32; NB_FEATURES] {
        &self.features
    }

    /// Shifts our input buffer and adds the new input to it, while running the input through a
    /// high-pass filter.
    pub(crate) fn shift_and_filter_input(&mut self, input: &[f32; FRAME_SIZE]) {
        let new_idx = self.input_mem.len() - FRAME_SIZE;
        for i in 0..new_idx {
            self.input_mem[i] = self.input_mem[i + FRAME_SIZE];
        }
        crate::util::BIQUAD_HP.filter(&mut self.input_mem[new_idx..], &mut self.mem_hp_x, input);
    }

    fn find_pitch(&mut self) -> usize {
        // `input_mem` holds max(FRAME_SIZE, PITCH_BUF_SIZE) samples, so this is its tail.
        let input = &self.input_mem[self.input_mem.len().saturating_sub(PITCH_BUF_SIZE)..];
        let (pitch, _gain) = self.pitch_finder.process(input);
        pitch
    }

    /// Computes the features of the current frame.
    ///
    /// The return value is `true` if the input was pretty much silent.
    pub(crate) fn compute_frame_features(&mut self, tables: &Tables, fft: &mut Fft) -> bool {
        let mut ly = [0.0; NB_BANDS];
        let mut tmp = [0.0; NB_BANDS];

        transform_input(
            tables,
            fft,
            &self.input_mem,
            0,
            &mut self.window_buf,
            &mut self.x,
            &mut self.ex,
        );
        // The pitch finder never returns more than PITCH_MAX_PERIOD; the clamp keeps the
        // lagged window inside `input_mem` whatever the input was.
        let pitch_idx = self.find_pitch().min(PITCH_MAX_PERIOD);

        transform_input(
            tables,
            fft,
            &self.input_mem,
            pitch_idx,
            &mut self.window_buf,
            &mut self.p,
            &mut self.ep,
        );
        crate::compute_band_corr(&mut self.exp[..], &self.x[..], &self.p[..]);
        for i in 0..NB_BANDS {
            self.exp[i] /= (0.001 + self.ex[i] * self.ep[i]).sqrt();
        }
        tables.dct(&mut tmp[..], &self.exp[..]);
        for i in 0..NB_DELTA_CEPS {
            self.features[NB_BANDS + 2 * NB_DELTA_CEPS + i] = tmp[i];
        }

        self.features[NB_BANDS + 2 * NB_DELTA_CEPS] -= 1.3;
        self.features[NB_BANDS + 2 * NB_DELTA_CEPS + 1] -= 0.9;
        self.features[NB_BANDS + 3 * NB_DELTA_CEPS] = 0.01 * (pitch_idx as f32 - 300.0);
        let mut log_max = -2.0;
        let mut follow = -2.0;
        let mut e = 0.0;
        for i in 0..NB_BANDS {
            ly[i] = (1e-2 + self.ex[i])
                .log10()
                .max(log_max - 7.0)
                .max(follow - 1.5);
            log_max = log_max.max(ly[i]);
            follow = (follow - 1.5).max(ly[i]);
            e += self.ex[i];
        }

        if e < 0.04 {
            /* If there's no audio, avoid messing up the state. */
            for i in 0..NB_FEATURES {
                self.features[i] = 0.0;
            }
            return true;
        }
        tables.dct(&mut self.features, &ly[..]);
        self.features[0] -= 12.0;
        self.features[1] -= 4.0;
        let ceps_0_idx = self.mem_id;
        let ceps_1_idx = if self.mem_id < 1 {
            CEPS_MEM + self.mem_id - 1
        } else {
            self.mem_id - 1
        };
        let ceps_2_idx = if self.mem_id < 2 {
            CEPS_MEM + self.mem_id - 2
        } else {
            self.mem_id - 2
        };

        for i in 0..NB_BANDS {
            self.cepstral_mem[ceps_0_idx][i] = self.features[i];
        }
        self.mem_id += 1;

        let ceps_0 = &self.cepstral_mem[ceps_0_idx];
        let ceps_1 = &self.cepstral_mem[ceps_1_idx];
        let ceps_2 = &self.cepstral_mem[ceps_2_idx];
        for i in 0..NB_DELTA_CEPS {
            self.features[i] = ceps_0[i] + ceps_1[i] + ceps_2[i];
            self.features[NB_BANDS + i] = ceps_0[i] - ceps_2[i];
            self.features[NB_BANDS + NB_DELTA_CEPS + i] = ceps_0[i] - 2.0 * ceps_1[i] + ceps_2[i];
        }

        /* Spectral variability features. */
        let mut spec_variability = 0.0;
        if self.mem_id == CEPS_MEM {
            self.mem_id = 0;
        }
        for i in 0..CEPS_MEM {
            let mut min_dist = 1e15f32;
            for j in 0..CEPS_MEM {
                let mut dist = 0.0;
                for k in 0..NB_BANDS {
                    let tmp = self.cepstral_mem[i][k] - self.cepstral_mem[j][k];
                    dist += tmp * tmp;
                }
                if j != i {
                    min_dist = min_dist.min(dist);
                }
            }
            spec_variability += min_dist;
        }

        self.features[NB_BANDS + 3 * NB_DELTA_CEPS + 1] = spec_variability / CEPS_MEM as f32 - 2.1;

        false
    }

    /// Applies a filter to the audio, attenuating pitches that have poor correlation with the
    /// pitch-lagged signal.
    pub(crate) fn pitch_filter(&mut self, gain: &[f32; NB_BANDS]) {
        let mut r = [0.0; NB_BANDS];
        let mut rf = [0.0; FREQ_SIZE];
        for i in 0..NB_BANDS {
            r[i] = if self.exp[i] > gain[i] {
                1.0
            } else {
                let exp_sq = self.exp[i] * self.exp[i];
                let g_sq = gain[i] * gain[i];
                exp_sq * (1.0 - g_sq) / (0.001 + g_sq * (1.0 - exp_sq))
            };
            r[i] = r[i].clamp(0.0, 1.0).sqrt();
            r[i] *= (self.ex[i] / (1e-8 + self.ep[i])).sqrt();
        }
        crate::interp_band_gain(&mut rf[..], &r[..]);
        for ((x, p), &rf) in self.x.iter_mut().zip(&self.p).zip(&rf) {
            *x += p * rf;
        }

        let mut new_e = [0.0; NB_BANDS];
        crate::compute_band_corr(&mut new_e[..], &self.x, &self.x);
        for i in 0..NB_BANDS {
            r[i] = (self.ex[i] / (1e-8 + new_e[i])).sqrt();
        }
        crate::interp_band_gain(&mut rf[..], &r[..]);
        for (x, &g) in self.x.iter_mut().zip(&rf) {
            *x *= g;
        }
    }

    pub(crate) fn apply_gain(&mut self, gain: &[f32; FREQ_SIZE]) {
        for (x, &g) in self.x.iter_mut().zip(gain) {
            *x *= g;
        }
    }

    pub(crate) fn frame_synthesis(
        &mut self,
        tables: &Tables,
        fft: &mut Fft,
        out: &mut [f32; FRAME_SIZE],
    ) {
        // The spectrum is not needed after this frame; the inverse uses it as scratch.
        fft.inverse(&mut self.x, &mut self.window_buf);
        // Not too sure why this scaling factor is introduced
        for x in &mut self.window_buf {
            *x /= 2.0;
        }

        tables.apply_window_in_place(&mut self.window_buf[..]);
        for i in 0..FRAME_SIZE {
            out[i] = self.window_buf[i] + self.synthesis_mem[i];
            self.synthesis_mem[i] = self.window_buf[FRAME_SIZE + i];
        }
    }
}

/// Fourier transforms the input.
///
/// The Fourier transform goes in `x` and the band energies go in `ex`.
fn transform_input(
    tables: &Tables,
    fft: &mut Fft,
    input: &[f32],
    lag: usize,
    window_buf: &mut [f32; WINDOW_SIZE],
    x: &mut [Complex; FREQ_SIZE],
    ex: &mut [f32],
) {
    // `lag` is at most PITCH_MAX_PERIOD and `input` holds PITCH_BUF_SIZE >= WINDOW_SIZE +
    // PITCH_MAX_PERIOD samples, so the start is never negative; saturate anyway.
    let input = &input[input.len().saturating_sub(WINDOW_SIZE + lag)..];
    tables.apply_window(&mut window_buf[..], input);
    // The forward transform uses its input as scratch; the window is rebuilt next time.
    fft.forward(&mut window_buf[..], &mut x[..]);

    // In the original RNNoise code, the forward transform is normalized and the inverse
    // tranform isn't. `rustfft` doesn't normalize either one, so we do it ourselves.
    let norm = tables.wnorm;
    for v in x.iter_mut() {
        *v *= norm;
    }

    crate::compute_band_corr(ex, x, x);
}
