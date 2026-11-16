//! RNNoise speech denoiser (ADR-0130 decision 5).
//!
//! A vendored copy of [`nnnoiseless`](https://github.com/jneem/nnnoiseless) 0.5.2, the
//! Rust port of Xiph's RNNoise, under its BSD-3-Clause licence (`LICENSE-RNNOISE`).
//! The patches against upstream are listed in `README.md`. In short: the FFT plans,
//! scratch buffers and lookup tables are owned by the [`Denoiser`] and made in
//! [`Denoiser::new`], so [`Denoiser::process_frame`] never allocates, locks or touches a
//! thread-local; the frame path has no `assert!` or `unwrap`; and a floor for the band
//! gains limits how far noise is reduced.
//!
//! Audio is 48 kHz mono in frames of [`FRAME_SIZE`] samples, in the i16 range
//! (`-32768.0..32767.0`, as upstream). The output of a frame is the input of the frame
//! before (overlap-add synthesis): the latency is one frame.

use std::sync::{Arc, OnceLock};

use realfft::num_complex::Complex32 as Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

mod features;
mod pitch;
mod rnn;
mod util;

use features::DenoiseFeatures;
use rnn::{RnnModel, RnnState};

const FRAME_SIZE_SHIFT: usize = 2;
/// Samples per frame: 10 ms at 48 kHz.
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
/// The only sample rate the model works at.
pub const SAMPLE_RATE: u32 = 48_000;
const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
const FREQ_SIZE: usize = FRAME_SIZE + 1;

const PITCH_MIN_PERIOD: usize = 60;
const PITCH_MAX_PERIOD: usize = 768;
const PITCH_FRAME_SIZE: usize = 960;
const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

const NB_BANDS: usize = 22;
const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
const EBAND_5MS: [usize; 22] = [
    // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

/// Why a denoiser could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DenoiseError {
    /// The built-in model did not parse (a corrupted build).
    Model,
    /// The FFT library refused the frame size.
    Fft,
}

impl std::fmt::Display for DenoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenoiseError::Model => f.write_str("the built-in noise model is invalid"),
            DenoiseError::Fft => f.write_str("the FFT could not be planned"),
        }
    }
}

impl std::error::Error for DenoiseError {}

/// Window, DCT table and normalisation, shared read-only by every denoiser.
struct Tables {
    window: [f32; WINDOW_SIZE],
    dct_table: [f32; NB_BANDS * NB_BANDS],
    wnorm: f32,
}

impl Tables {
    fn new() -> Self {
        let pi = std::f64::consts::PI;
        let mut window = [0.0; WINDOW_SIZE];
        for i in 0..FRAME_SIZE {
            let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
            window[i] = (0.5 * pi * sin * sin).sin() as f32;
            window[WINDOW_SIZE - i - 1] = (0.5 * pi * sin * sin).sin() as f32;
        }
        let wnorm = 1_f32 / window.iter().map(|x| x * x).sum::<f32>();

        let mut dct_table = [0.0; NB_BANDS * NB_BANDS];
        for i in 0..NB_BANDS {
            for j in 0..NB_BANDS {
                dct_table[i * NB_BANDS + j] =
                    ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
                if j == 0 {
                    dct_table[i * NB_BANDS + j] *= 0.5f32.sqrt();
                }
            }
        }
        Self {
            window,
            dct_table,
            wnorm,
        }
    }

    /// A brute-force DCT of size `NB_BANDS`.
    fn dct(&self, out: &mut [f32], x: &[f32]) {
        for (i, o) in out.iter_mut().enumerate().take(NB_BANDS) {
            let mut sum = 0.0;
            for (j, &xj) in x.iter().enumerate().take(NB_BANDS) {
                sum += xj * self.dct_table[j * NB_BANDS + i];
            }
            *o = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
        }
    }

    fn apply_window(&self, output: &mut [f32], input: &[f32]) {
        for ((x, &y), &w) in output.iter_mut().zip(input).zip(&self.window[..]) {
            *x = y * w;
        }
    }

    fn apply_window_in_place(&self, xs: &mut [f32]) {
        for (x, &w) in xs.iter_mut().zip(&self.window[..]) {
            *x *= w;
        }
    }
}

/// What every denoiser shares: the parsed model and the tables. Made once, off the
/// audio thread, by the first [`Denoiser::new`].
struct Shared {
    model: RnnModel,
    tables: Tables,
}

fn shared() -> Result<Arc<Shared>, DenoiseError> {
    static SHARED: OnceLock<Option<Arc<Shared>>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let model = RnnModel::builtin()?;
            Some(Arc::new(Shared {
                model,
                tables: Tables::new(),
            }))
        })
        .clone()
        .ok_or(DenoiseError::Model)
}

/// The FFT plans and their scratch, owned by one denoiser.
struct Fft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    forward_scratch: Vec<Complex>,
    inverse_scratch: Vec<Complex>,
}

impl Fft {
    fn new() -> Result<Self, DenoiseError> {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(WINDOW_SIZE);
        let inverse = planner.plan_fft_inverse(WINDOW_SIZE);
        if forward.complex_len() != FREQ_SIZE || inverse.complex_len() != FREQ_SIZE {
            return Err(DenoiseError::Fft);
        }
        Ok(Self {
            forward_scratch: forward.make_scratch_vec(),
            inverse_scratch: inverse.make_scratch_vec(),
            forward,
            inverse,
        })
    }

    /// Forward transform of `input` (length `WINDOW_SIZE`, used as scratch) into
    /// `output` (length `FREQ_SIZE`). Lengths are fixed by construction; a length error
    /// cannot happen, and if it did the spectrum would be silence.
    fn forward(&mut self, input: &mut [f32], output: &mut [Complex]) {
        if self
            .forward
            .process_with_scratch(input, output, &mut self.forward_scratch)
            .is_err()
        {
            output.fill(Complex::default());
        }
    }

    /// Inverse transform (unnormalised). `input` is used as scratch. The imaginary parts
    /// of the DC and Nyquist bins are always zero here (real gains of a real spectrum);
    /// realfft still transforms when they are not and only reports it, so that report
    /// is not an error. A length error cannot happen and gives silence.
    fn inverse(&mut self, input: &mut [Complex], output: &mut [f32]) {
        match self
            .inverse
            .process_with_scratch(input, output, &mut self.inverse_scratch)
        {
            Ok(()) | Err(realfft::FftError::InputValues(..)) => {}
            Err(_) => output.fill(0.0),
        }
    }
}

/// One RNNoise state: denoises one mono channel, frame by frame.
pub struct Denoiser {
    shared: Arc<Shared>,
    lastg: [f32; NB_BANDS],
    rnn: RnnState,
    feat: DenoiseFeatures,
    fft: Fft,
    /// Lowest band gain: `10^(-max_reduction / 20)`; 0 is upstream behaviour.
    floor: f32,
}

impl Denoiser {
    /// A denoiser with the built-in model, no gain floor and a silent history.
    /// Allocates; call it off the audio thread.
    pub fn new() -> Result<Self, DenoiseError> {
        let shared = shared()?;
        Ok(Self {
            rnn: RnnState::new(&shared.model),
            lastg: [0.0; NB_BANDS],
            feat: DenoiseFeatures::new(),
            fft: Fft::new()?,
            shared,
            floor: 0.0,
        })
    }

    /// Limits the reduction of any band to `max_reduction_db` (>= 0; infinity or a
    /// very large value is upstream behaviour). Takes effect on the next frame.
    pub fn set_max_reduction_db(&mut self, max_reduction_db: f32) {
        let db = if max_reduction_db.is_finite() {
            max_reduction_db.max(0.0)
        } else {
            f32::INFINITY
        };
        self.floor = 10.0f32.powf(-db / 20.0);
    }

    /// Back to a silent history (as new), keeping the model, plans and floor. Allocates
    /// nothing.
    pub fn reset(&mut self) {
        self.lastg = [0.0; NB_BANDS];
        self.rnn.reset();
        self.feat.reset();
    }

    /// Denoises one frame. `input` and `output` are i16-range samples; `output` is the
    /// denoised frame before `input` (one frame of latency). Returns the voice
    /// probability of `input`, 0..1. Never allocates, locks or panics; non-finite input
    /// samples are treated as silence.
    pub fn process_frame(
        &mut self,
        output: &mut [f32; FRAME_SIZE],
        input: &[f32; FRAME_SIZE],
    ) -> f32 {
        let mut g = [0.0; NB_BANDS];
        let mut gf = [1.0; FREQ_SIZE];
        let mut vad_prob = [0.0];
        let tables = &self.shared.tables;

        let mut clean = [0.0f32; FRAME_SIZE];
        for (c, &x) in clean.iter_mut().zip(input) {
            *c = if x.is_finite() { x } else { 0.0 };
        }
        self.feat.shift_and_filter_input(&clean);
        let silence = self.feat.compute_frame_features(tables, &mut self.fft);
        if !silence {
            self.rnn.compute(
                &self.shared.model,
                &mut g,
                &mut vad_prob,
                self.feat.features(),
            );
            self.feat.pitch_filter(&g);
            let mut applied = [0.0; NB_BANDS];
            for i in 0..NB_BANDS {
                g[i] = g[i].max(0.6 * self.lastg[i]);
                self.lastg[i] = g[i];
                applied[i] = g[i].max(self.floor);
            }
            interp_band_gain(&mut gf, &applied);
            self.feat.apply_gain(&gf);
        }

        self.feat.frame_synthesis(tables, &mut self.fft, output);
        for y in output.iter_mut() {
            if !y.is_finite() {
                *y = 0.0;
            }
        }
        let p = vad_prob[0];
        if p.is_finite() {
            p.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// Correlation of two spectra, aggregated into bands (`out` has `NB_BANDS` entries).
fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            let corr = x[idx].re * p[idx].re + x[idx].im * p[idx].im;
            out[i] += (1.0 - frac) * corr;
            out[i + 1] += frac * corr;
        }
    }
    out[0] *= 2.0;
    out[NB_BANDS - 1] *= 2.0;
}

fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            out[idx] = (1.0 - frac) * band_e[i] + frac * band_e[i + 1];
        }
    }
}
