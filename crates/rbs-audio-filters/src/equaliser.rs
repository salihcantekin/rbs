//! Equaliser (ADR-0130 decision 8): a low cut, a low shelf, two peaking bands and a high
//! shelf, RBJ biquads (Bristow-Johnson, "Audio EQ Cookbook") in transposed direct form
//! II per channel. No latency.
//!
//! `set` only stores targets. Frequency (in octaves), gain (in dB) and Q (in octaves)
//! glide toward them with a 20 ms time constant; while anything glides the coefficients
//! are recomputed every 32 frames, so a sweep makes no zipper noise or step. A shelf or
//! band at 0 dB and an off low cut are skipped, so the defaults are bit-exact.

use rbs_core::media::MAX_CHANNELS;
use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, clean};

/// Type id of [`Equaliser`].
pub const EQUALISER: &str = "org.rbs.audio-filter.equaliser";

/// Frames between coefficient updates while a value glides.
const STEP: usize = 32;
/// The glide's time constant in seconds.
const GLIDE_SECONDS: f32 = 0.020;
/// Highest frequency, as a fraction of the sample rate.
const MAX_FRACTION: f32 = 0.45;
/// Q of the low cut (Butterworth, 12 dB per octave).
const LOW_CUT_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

const LOW_CUT: usize = 0;
const LOW_CUT_FREQ: usize = 1;
const LOW_SHELF_FREQ: usize = 2;
const LOW_SHELF_GAIN: usize = 3;
const BAND1_FREQ: usize = 4;
const BAND1_GAIN: usize = 5;
const BAND1_Q: usize = 6;
const BAND2_FREQ: usize = 7;
const BAND2_GAIN: usize = 8;
const BAND2_Q: usize = 9;
const HIGH_SHELF_FREQ: usize = 10;
const HIGH_SHELF_GAIN: usize = 11;

const FIELDS: [Field; 12] = [
    Field::toggle("low_cut", "Low cut", false)
        .with_help("Removes rumble and handling noise below the low cut frequency."),
    Field::number(
        "low_cut_freq",
        "Low cut frequency",
        20.0,
        400.0,
        80.0,
        1.0,
        "Hz",
    )
    .log()
    .with_help("12 dB per octave below this. 80 Hz suits most voices."),
    Field::number(
        "low_shelf_freq",
        "Low shelf frequency",
        20.0,
        1000.0,
        120.0,
        1.0,
        "Hz",
    )
    .log()
    .with_help("The low shelf changes everything below this frequency."),
    Field::number(
        "low_shelf_gain",
        "Low shelf gain",
        -18.0,
        18.0,
        0.0,
        0.5,
        "dB",
    )
    .with_help("Warmth: raise or lower the lows. 0 dB is off."),
    Field::number(
        "band1_freq",
        "Band 1 frequency",
        40.0,
        16000.0,
        400.0,
        1.0,
        "Hz",
    )
    .log()
    .with_help("Centre of band 1. Around 300 to 500 Hz a voice can sound boxy."),
    Field::number("band1_gain", "Band 1 gain", -18.0, 18.0, 0.0, 0.5, "dB")
        .with_help("Raise or lower band 1. 0 dB is off."),
    Field::number("band1_q", "Band 1 width (Q)", 0.1, 10.0, 1.0, 0.1, "")
        .log()
        .with_help("Higher is narrower."),
    Field::number(
        "band2_freq",
        "Band 2 frequency",
        40.0,
        16000.0,
        3000.0,
        1.0,
        "Hz",
    )
    .log()
    .with_help("Centre of band 2. Around 2 to 5 kHz makes a voice clearer."),
    Field::number("band2_gain", "Band 2 gain", -18.0, 18.0, 0.0, 0.5, "dB")
        .with_help("Raise or lower band 2. 0 dB is off."),
    Field::number("band2_q", "Band 2 width (Q)", 0.1, 10.0, 1.0, 0.1, "")
        .log()
        .with_help("Higher is narrower."),
    Field::number(
        "high_shelf_freq",
        "High shelf frequency",
        1000.0,
        20000.0,
        8000.0,
        10.0,
        "Hz",
    )
    .log()
    .with_help("The high shelf changes everything above this frequency."),
    Field::number(
        "high_shelf_gain",
        "High shelf gain",
        -18.0,
        18.0,
        0.0,
        0.5,
        "dB",
    )
    .with_help("Air: raise or lower the highs. 0 dB is off."),
];

/// The Equaliser filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Equaliser;

impl AudioFilterType for Equaliser {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(EQUALISER)
    }
    fn name(&self) -> &'static str {
        "Equaliser"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(EqualiserDsp::new(setup, values)))
    }
}

/// The shape of one section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// High-pass, 12 dB per octave (the low cut).
    LowCut,
    /// Low shelf, slope 1.
    LowShelf,
    /// Peaking band.
    Peak,
    /// High shelf, slope 1.
    HighShelf,
}

/// Biquad coefficients, normalised so `a0` is 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefs {
    /// Feed-forward coefficient of `x[n]`.
    pub b0: f32,
    /// Feed-forward coefficient of `x[n-1]`.
    pub b1: f32,
    /// Feed-forward coefficient of `x[n-2]`.
    pub b2: f32,
    /// Feedback coefficient of `y[n-1]`.
    pub a1: f32,
    /// Feedback coefficient of `y[n-2]`.
    pub a2: f32,
}

impl Coefs {
    /// The RBJ coefficients of `shape` at `freq` Hz, `gain_db` and `q`, for `rate` Hz.
    pub fn rbj(shape: Shape, rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let rate = f64::from(rate);
        let w0 = std::f64::consts::TAU * f64::from(freq) / rate;
        let (sin, cos) = w0.sin_cos();
        // 10^(gain / 40) as 2^(gain * log2(10) / 40), in f32: cheaper, and exact enough.
        let a = f64::from((gain_db * (std::f32::consts::LOG2_10 / 40.0)).exp2());
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::LowCut => {
                let alpha = sin / (2.0 * f64::from(q));
                let b = (1.0 + cos) / 2.0;
                (b, -(1.0 + cos), b, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Shape::Peak => {
                let alpha = sin / (2.0 * f64::from(q));
                (
                    1.0 + alpha * a,
                    -2.0 * cos,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos,
                    1.0 - alpha / a,
                )
            }
            Shape::LowShelf | Shape::HighShelf => {
                // Slope 1: alpha = sin / 2 * sqrt(2).
                let alpha = sin / 2.0 * std::f64::consts::SQRT_2;
                let k = 2.0 * a.sqrt() * alpha;
                let (p, m) = (a + 1.0, a - 1.0);
                if shape == Shape::LowShelf {
                    (
                        a * (p - m * cos + k),
                        2.0 * a * (m - p * cos),
                        a * (p - m * cos - k),
                        p + m * cos + k,
                        -2.0 * (m + p * cos),
                        p + m * cos - k,
                    )
                } else {
                    (
                        a * (p + m * cos + k),
                        -2.0 * a * (m + p * cos),
                        a * (p + m * cos - k),
                        p - m * cos + k,
                        2.0 * (m - p * cos),
                        p - m * cos - k,
                    )
                }
            }
        };
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
        }
    }

    /// The magnitude response in dB at `freq` Hz, for `rate` Hz (for tests).
    pub fn magnitude_db(&self, rate: f32, freq: f32) -> f32 {
        let w = std::f64::consts::TAU * f64::from(freq) / f64::from(rate);
        let (b0, b1, b2) = (f64::from(self.b0), f64::from(self.b1), f64::from(self.b2));
        let (a1, a2) = (f64::from(self.a1), f64::from(self.a2));
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num = (b0 + b1 * c1 + b2 * c2).powi(2) + (b1 * s1 + b2 * s2).powi(2);
        let den = (1.0 + a1 * c1 + a2 * c2).powi(2) + (a1 * s1 + a2 * s2).powi(2);
        (10.0 * (num / den).log10()) as f32
    }
}

/// One section's values: frequency and Q in log2 (octaves), gain in dB.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Params {
    on: bool,
    log_freq: f32,
    gain_db: f32,
    log_q: f32,
}

/// One section: its shape, current and target values, and coefficients.
#[derive(Debug, Clone, Copy)]
struct Section {
    shape: Shape,
    now: Params,
    target: Params,
    coefs: Coefs,
}

impl Section {
    /// Whether the section changes the sound; a skipped one also has its state cleared.
    fn active(&self) -> bool {
        match self.shape {
            Shape::LowCut => self.now.on,
            _ => self.now.gain_db != 0.0,
        }
    }

    fn gliding(&self) -> bool {
        self.now != self.target
    }

    /// One glide step of `k` toward the target; snaps when close.
    fn glide(&mut self, k: f32) {
        let step = |now: f32, to: f32, snap: f32| {
            let next = now + (to - now) * k;
            if (to - next).abs() < snap { to } else { next }
        };
        self.now.on = self.target.on;
        self.now.log_freq = step(self.now.log_freq, self.target.log_freq, 1.0e-4);
        self.now.gain_db = step(self.now.gain_db, self.target.gain_db, 1.0e-3);
        self.now.log_q = step(self.now.log_q, self.target.log_q, 1.0e-4);
    }

    fn update(&mut self, rate: f32) {
        self.coefs = Coefs::rbj(
            self.shape,
            rate,
            self.now.log_freq.exp2(),
            self.now.gain_db,
            self.now.log_q.exp2(),
        );
    }
}

/// Number of sections.
const SECTIONS: usize = 5;

/// The equaliser DSP.
#[derive(Debug)]
pub struct EqualiserDsp {
    rate: f32,
    /// Glide factor per [`STEP`] frames.
    k: f32,
    sections: [Section; SECTIONS],
    /// Per channel and section, the two TDF-II state values.
    z: Vec<[[f32; 2]; SECTIONS]>,
    /// Frames left until the next coefficient update while gliding.
    countdown: usize,
}

impl EqualiserDsp {
    /// An equaliser with `values` for `setup`, at its values (no glide from defaults).
    pub fn new(setup: DspSetup, values: &Values) -> Self {
        let rate = setup.format.sample_rate.hz() as f32;
        let channels = setup.format.channels.channels().clamp(1, MAX_CHANNELS);
        let blank = Params {
            on: false,
            log_freq: 10.0,
            gain_db: 0.0,
            log_q: 0.0,
        };
        let coefs = Coefs {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        };
        let section = |shape| Section {
            shape,
            now: blank,
            target: blank,
            coefs,
        };
        let mut dsp = Self {
            rate,
            k: 1.0 - (-(STEP as f32) / (GLIDE_SECONDS * rate)).exp(),
            sections: [
                section(Shape::LowCut),
                section(Shape::LowShelf),
                section(Shape::Peak),
                section(Shape::Peak),
                section(Shape::HighShelf),
            ],
            z: vec![[[0.0; 2]; SECTIONS]; channels],
            countdown: 0,
        };
        dsp.set(values);
        for s in &mut dsp.sections {
            s.now = s.target;
            s.update(rate);
        }
        dsp
    }

    /// The current coefficients of section `index` (0 low cut, 1 low shelf, 2 and 3 the
    /// bands, 4 high shelf), for tests.
    pub fn coefs(&self, index: usize) -> Option<Coefs> {
        self.sections.get(index).map(|s| s.coefs)
    }

    /// Whether section `index` is processed, for tests.
    pub fn is_active(&self, index: usize) -> bool {
        self.sections.get(index).is_some_and(Section::active)
    }

    /// The response of every processed section together at `freq` Hz, in dB: what the
    /// editor draws as the curve (ADR-0130 decision 10). Off the audio thread only.
    pub fn response_db(&self, freq: f32) -> f32 {
        self.sections
            .iter()
            .filter(|s| s.active())
            .map(|s| s.coefs.magnitude_db(self.rate, freq))
            .sum()
    }

    /// One glide step for every section that glides; clears the state of a section that
    /// stops being processed, so it starts clean when it comes back.
    fn glide(&mut self) {
        for (i, s) in self.sections.iter_mut().enumerate() {
            if !s.gliding() {
                continue;
            }
            let was = s.active();
            s.glide(self.k);
            s.update(self.rate);
            if was && !s.active() {
                for z in &mut self.z {
                    z[i] = [0.0; 2];
                }
            }
        }
    }

    fn filter(&mut self, channels: &mut [&mut [f32]], start: usize, end: usize) {
        let mut active = [Coefs {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }; SECTIONS];
        let mut index = [0usize; SECTIONS];
        let mut count = 0;
        for (i, s) in self.sections.iter().enumerate() {
            if s.active() {
                active[count] = s.coefs;
                index[count] = i;
                count += 1;
            }
        }
        let coefs = &active[..count];
        let index = &index[..count];
        // Two channels at a time: their recursions are independent, so the CPU overlaps
        // them (one channel alone waits on each section's feedback).
        let mut zs = self.z.iter_mut();
        for pair in channels.chunks_mut(2) {
            match pair {
                [l, r] => {
                    let (Some(zl), Some(zr)) = (zs.next(), zs.next()) else {
                        return;
                    };
                    let (ll, rl) = (l.len(), r.len());
                    let l = l.get_mut(start..end.min(ll)).unwrap_or(&mut []);
                    let r = r.get_mut(start..end.min(rl)).unwrap_or(&mut []);
                    if l.len() == r.len() {
                        run_lanes::<2>(coefs, index, [zl, zr], [l, r]);
                    } else {
                        run_lanes::<1>(coefs, index, [zl], [l]);
                        run_lanes::<1>(coefs, index, [zr], [r]);
                    }
                }
                [c] => {
                    let Some(z) = zs.next() else {
                        return;
                    };
                    let len = c.len();
                    let c = c.get_mut(start..end.min(len)).unwrap_or(&mut []);
                    run_lanes::<1>(coefs, index, [z], [c]);
                }
                _ => {}
            }
        }
    }
}

/// Runs `N` channels of equal length through the active sections `coefs` (whose states
/// are `z[..][index[j]]`), sanitising input and output. Indexed by lane and frame on
/// purpose: the lanes of one frame must sit side by side in the inner loop.
/// Section states below this are flushed to 0 at the end of a block.
const TINY: f32 = 1.0e-30;

#[allow(clippy::needless_range_loop)]
fn run_lanes<const N: usize>(
    coefs: &[Coefs],
    index: &[usize],
    z: [&mut [[f32; 2]; SECTIONS]; N],
    mut ch: [&mut [f32]; N],
) {
    let frames = ch.iter().map(|c| c.len()).min().unwrap_or(0);
    if coefs.is_empty() {
        for c in ch.iter_mut() {
            for x in c.iter_mut() {
                *x = clean(*x);
            }
        }
        return;
    }
    // The active sections' states, per lane, copied to locals for the frame loop.
    let mut st = [[[0.0f32; N]; 2]; SECTIONS];
    for (j, &i) in index.iter().enumerate() {
        for lane in 0..N {
            st[j][0][lane] = z[lane][i][0];
            st[j][1][lane] = z[lane][i][1];
        }
    }
    for f in 0..frames {
        let mut v = [0.0f32; N];
        for lane in 0..N {
            v[lane] = clean(ch[lane][f]);
        }
        for (c, s) in coefs.iter().zip(st.iter_mut()) {
            for lane in 0..N {
                let y = c.b0 * v[lane] + s[0][lane];
                s[0][lane] = c.b1 * v[lane] - c.a1 * y + s[1][lane];
                s[1][lane] = c.b2 * v[lane] - c.a2 * y;
                v[lane] = y;
            }
        }
        for lane in 0..N {
            ch[lane][f] = clean(v[lane]);
        }
    }
    for (j, &i) in index.iter().enumerate() {
        for lane in 0..N {
            // A state driven out of range (it cannot be, for finite input and stable
            // coefficients) restarts clean rather than ringing forever. A state that
            // has decayed below `TINY` after silence is flushed to 0 before it turns
            // into denormals, which cost x86 a microcode assist per operation (A-04
            // of the 2026-11 performance audit); it is far below any audible level.
            let s = [st[j][0][lane], st[j][1][lane]];
            z[lane][i] = if s[0].is_finite() && s[1].is_finite() {
                s.map(|v| if v.abs() < TINY { 0.0 } else { v })
            } else {
                [0.0; 2]
            };
        }
    }
}

impl AudioDsp for EqualiserDsp {
    fn set(&mut self, v: &Values) {
        let top = (MAX_FRACTION * self.rate).log2();
        let freq = |i: usize, lo: f32, hi: f32| v.number(i).clamp(lo, hi).log2().min(top);
        let gain = |i: usize| v.number(i).clamp(-18.0, 18.0);
        let q = |i: usize| v.number(i).clamp(0.1, 10.0).log2();
        let targets = [
            Params {
                on: v.toggle(LOW_CUT),
                log_freq: freq(LOW_CUT_FREQ, 20.0, 400.0),
                gain_db: 0.0,
                log_q: LOW_CUT_Q.log2(),
            },
            Params {
                on: true,
                log_freq: freq(LOW_SHELF_FREQ, 20.0, 1000.0),
                gain_db: gain(LOW_SHELF_GAIN),
                log_q: 0.0,
            },
            Params {
                on: true,
                log_freq: freq(BAND1_FREQ, 40.0, 16000.0),
                gain_db: gain(BAND1_GAIN),
                log_q: q(BAND1_Q),
            },
            Params {
                on: true,
                log_freq: freq(BAND2_FREQ, 40.0, 16000.0),
                gain_db: gain(BAND2_GAIN),
                log_q: q(BAND2_Q),
            },
            Params {
                on: true,
                log_freq: freq(HIGH_SHELF_FREQ, 1000.0, 20000.0),
                gain_db: gain(HIGH_SHELF_GAIN),
                log_q: 0.0,
            },
        ];
        for (i, (s, t)) in self.sections.iter_mut().zip(targets).enumerate() {
            let was = s.active();
            s.target = t;
            // The low cut's switch acts at once (it has no neutral setting to glide
            // through); its state starts clean either way.
            s.now.on = t.on;
            if s.shape == Shape::LowCut && was != s.active() {
                for z in &mut self.z {
                    z[i] = [0.0; 2];
                }
            }
        }
    }

    fn reset(&mut self) {
        for z in &mut self.z {
            *z = [[0.0; 2]; SECTIONS];
        }
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        let mut start = 0;
        while start < frames {
            let gliding = self.sections.iter().any(Section::gliding);
            if !gliding {
                self.countdown = 0;
                self.filter(channels, start, frames);
                return;
            }
            if self.countdown == 0 {
                self.glide();
                self.countdown = STEP;
            }
            let end = (start + self.countdown).min(frames);
            self.countdown -= end - start;
            self.filter(channels, start, end);
            start = end;
        }
    }
}
