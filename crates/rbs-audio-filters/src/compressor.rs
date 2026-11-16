//! Compressor with a soft knee, an RMS or peak detector, a sidechain key and parallel
//! mix (ADR-0130 decision 6). No latency.
//!
//! Per sample: a linked level over all channels (or over the key's channels), in dB; the
//! gain computer's reduction with a quadratic soft knee (Giannoulis, Massberg and Reiss,
//! 2012); a smoothed reduction that follows with `attack` when it deepens and `release`
//! when it eases; then `makeup` and `mix`. The dB conversions use fast polynomial log2
//! and exp2 (error below 0.001 dB), so the block cost stays within the budget.

use std::sync::Arc;

use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter, amp_to_db, clean};

/// Type id of [`Compressor`].
pub const COMPRESSOR: &str = "org.rbs.audio-filter.compressor";

/// Time constant of the RMS detector, in seconds.
const RMS_SECONDS: f32 = 0.010;

/// A reduction above this, in dB, counts as none.
const SNAP_DB: f32 = -1.0e-6;

/// A detector square below this (-300 dB) counts as silence, as do NaN and infinity.
const TINY_SQ: f32 = 1.0e-30;

/// Added to the RMS detector's mean square each sample (about -330 dB once settled).
const RMS_FLOOR: f32 = 1.0e-36;

/// Index of each field (the order of [`FIELDS`]).
const KEY: usize = 0;
const THRESHOLD: usize = 1;
const RATIO: usize = 2;
const KNEE: usize = 3;
const ATTACK: usize = 4;
const RELEASE: usize = 5;
const DETECTOR: usize = 6;
const MAKEUP: usize = 7;
const MIX: usize = 8;

const DETECTORS: [(&str, &str); 2] = [("rms", "Average (RMS)"), ("peak", "Peak")];

const FIELDS: [Field; 9] = [
    Field::source("key_source", "Sidechain").with_help(
        "Compress when another source is loud instead of this one. None uses this sound.",
    ),
    Field::number("threshold", "Threshold", -60.0, 0.0, -18.0, 0.5, "dB")
        .with_help("Sound above this level is turned down."),
    Field::number("ratio", "Ratio", 1.0, 20.0, 4.0, 0.1, ":1")
        .log()
        .with_help("How much: at 4:1, 4 dB over the threshold comes out as 1 dB over."),
    Field::number("knee", "Knee", 0.0, 24.0, 6.0, 0.5, "dB")
        .with_help("How gently compression starts around the threshold. 0 is a hard knee."),
    Field::number("attack", "Attack", 0.1, 200.0, 5.0, 0.1, "ms")
        .log()
        .with_help("How fast the level is turned down when it rises."),
    Field::number("release", "Release", 10.0, 2000.0, 120.0, 1.0, "ms")
        .log()
        .with_help("How fast the level comes back when it falls."),
    Field::choice("detector", "Detector", &DETECTORS, "rms")
        .with_help("Average follows loudness and sounds natural; Peak catches short hits."),
    Field::number("makeup", "Makeup gain", 0.0, 24.0, 0.0, 0.5, "dB")
        .with_help("Raises the compressed sound to make up for the reduction."),
    Field::number("mix", "Mix", 0.0, 100.0, 100.0, 1.0, "%")
        .with_help("Blends in the uncompressed sound. Below 100 % keeps the voice's attacks."),
];

/// The Compressor filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Compressor;

impl AudioFilterType for Compressor {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(COMPRESSOR)
    }
    fn name(&self) -> &'static str {
        "Compressor"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(CompressorDsp::new(setup, values)))
    }
    fn key_input(&self) -> bool {
        true
    }
}

/// The compressor's settings in the units `process` uses.
#[derive(Debug, Clone, Copy)]
struct Settings {
    keyed: bool,
    threshold: f32,
    /// `1 / ratio - 1`, at most 0.
    slope: f32,
    knee: f32,
    /// `slope / (2 * knee)`, 0 for a hard knee.
    knee_scale: f32,
    /// The detector's mean square (or square) below which the curve is 0: the bottom
    /// of the knee, so a quiet input skips the dB conversion.
    quiet_sq: f32,
    attack: f32,
    release: f32,
    rms: bool,
    makeup_db: f32,
    /// The output gain while there is no reduction: exactly 1 when `makeup` is 0.
    makeup_gain: f32,
    mix: f32,
}

/// The compressor DSP.
#[derive(Debug)]
pub struct CompressorDsp {
    rate: f32,
    rms_coef: f32,
    s: Settings,
    /// Mean square of the RMS detector.
    ms: f32,
    /// The smoothed gain reduction in dB, at most 0.
    gr_db: f32,
    /// Per frame of a block: the detector's square, then the output gain. Made in
    /// `new` for the graph's block size; a longer block is processed in parts.
    work: Vec<f32>,
    meter: Arc<FilterMeter>,
}

/// log2 of a positive normal `x`, error below 1e-5 (about 0.0001 dB).
#[inline]
fn fast_log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    let m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    // log2(m) = 2 / ln 2 * atanh(t), t = (m - 1) / (m + 1) in [0, 1/3): odd series.
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let series =
        t * (1.0 + t2 * (1.0 / 3.0 + t2 * (1.0 / 5.0 + t2 * (1.0 / 7.0 + t2 * (1.0 / 9.0)))));
    exponent as f32 + 2.885_39 * series
}

/// 2^x for x in about -126..126, relative error below 2e-6. Branch- and
/// conversion-free, so a block loop over it vectorises.
#[inline]
fn fast_exp2(x: f32) -> f32 {
    // Adding 1.5 * 2^23 rounds to an integer `n` held in the low mantissa bits.
    const MAGIC: f32 = 12_582_912.0;
    let x = x.clamp(-126.0, 126.0);
    let y = x + MAGIC;
    let f = x - (y - MAGIC);
    let n = y.to_bits() as i32 - MAGIC.to_bits() as i32;
    // Taylor series of e^(f ln 2), f in [-0.5, 0.5], to the 7th power.
    let p = 1.0
        + f * (std::f32::consts::LN_2
            + f * (0.240_226_5
                + f * (0.055_504_11
                    + f * (0.009_618_129
                        + f * (0.001_333_355 + f * (0.000_154_035_3 + f * 0.000_015_252_73))))));
    f32::from_bits(((n + 127) as u32) << 23) * p
}

/// 20 / log2(10): dB per unit of log2 amplitude.
const DB_PER_LOG2: f32 = 6.020_6;

impl CompressorDsp {
    /// A compressor with `values` for `setup`, not compressing.
    pub fn new(setup: DspSetup, values: &Values) -> Self {
        let rate = setup.format.sample_rate.hz() as f32;
        Self {
            rate,
            rms_coef: (-1.0 / (RMS_SECONDS * rate)).exp(),
            s: Self::settings(rate, values),
            ms: 0.0,
            gr_db: 0.0,
            work: vec![0.0; (setup.block_frames as usize).max(1)],
            meter: Arc::new(FilterMeter::default()),
        }
    }

    fn settings(rate: f32, v: &Values) -> Settings {
        let coef = |ms: f32| (-1.0 / (ms * 0.001 * rate)).exp();
        let ratio = v.number(RATIO).clamp(1.0, 20.0);
        let threshold = v.number(THRESHOLD).clamp(-60.0, 0.0);
        let knee = v.number(KNEE).clamp(0.0, 24.0);
        let makeup_db = v.number(MAKEUP).clamp(0.0, 24.0);
        let mix = v.number(MIX).clamp(0.0, 100.0) / 100.0;
        let mut s = Settings {
            keyed: v.source(KEY).is_some(),
            threshold,
            slope: 1.0 / ratio - 1.0,
            knee,
            knee_scale: if knee > 0.0 {
                (1.0 / ratio - 1.0) / (2.0 * knee)
            } else {
                0.0
            },
            quiet_sq: 10f32.powf((threshold - knee / 2.0) / 10.0),
            attack: coef(v.number(ATTACK).clamp(0.1, 200.0)),
            release: coef(v.number(RELEASE).clamp(10.0, 2000.0)),
            rms: v.choice(DETECTOR) == 0,
            makeup_db,
            makeup_gain: 1.0,
            mix,
        };
        s.makeup_gain = Self::gain(&s, 0.0);
        s
    }

    /// The output gain for a reduction of `gr_db`: exactly 1 when the wet gain is 0 dB
    /// or the mix is dry, so those cases are bit-exact.
    #[inline]
    fn gain(s: &Settings, gr_db: f32) -> f32 {
        let gain_db = gr_db + s.makeup_db;
        if gain_db == 0.0 || s.mix == 0.0 {
            1.0
        } else {
            1.0 - s.mix + s.mix * fast_exp2(gain_db / DB_PER_LOG2)
        }
    }

    /// The static curve: the gain change in dB (at most 0) for a level of `x` dB.
    #[inline]
    fn curve(s: &Settings, x: f32) -> f32 {
        let over = x - s.threshold;
        let t = over + 0.5 * s.knee;
        // Selects, not branches, so the block loop vectorises; the knee term is only
        // used when the knee is above 0.
        let in_knee = s.knee > 0.0 && 2.0 * over.abs() <= s.knee;
        let knee_gr = s.knee_scale * t * t;
        let above = s.slope * over.max(0.0);
        if in_knee { knee_gr } else { above }
    }

    /// The gain change of the static curve for a steady level (for tests).
    pub fn static_gain_db(&self, level_db: f32) -> f32 {
        Self::curve(&self.s, level_db)
    }

    /// The current smoothed gain reduction in dB (at most 0).
    pub fn reduction_db(&self) -> f32 {
        self.gr_db
    }

    /// Smooths the targets in `w` (dB) into the reduction and replaces them with the
    /// output gain per frame.
    #[inline]
    fn smooth(&mut self, s: &Settings, w: &mut [f32], deepest: &mut f32) {
        let mut gr = self.gr_db;
        let (att, rel) = (s.attack, s.release);
        for d in w.iter_mut() {
            let t = *d;
            // Both candidates, then a select on bits: a branch here mispredicts on
            // every cycle of a low tone.
            let down = att * gr + (1.0 - att) * t;
            let up = rel * gr + (1.0 - rel) * t;
            let m = u32::from(t < gr).wrapping_neg();
            gr = f32::from_bits((down.to_bits() & m) | (up.to_bits() & !m));
            *deepest = deepest.min(gr);
            *d = gr;
        }
        // A vanishing reduction is exactly 0: in the gain below each frame, and in the
        // state once per part (off the recursion's critical path; a reduction above
        // -1e-6 dB cannot decay to a denormal within one block).
        if gr > SNAP_DB {
            gr = 0.0;
        }
        self.gr_db = gr;
        for d in w.iter_mut() {
            let wet = 1.0 - s.mix + s.mix * fast_exp2((*d + s.makeup_db) / DB_PER_LOG2);
            *d = if *d > SNAP_DB { s.makeup_gain } else { wet };
        }
    }

    fn run(&mut self, channels: &mut [&mut [f32]], key: Option<&[&[f32]]>, frames: usize) {
        let s = self.s;
        let mut work = std::mem::take(&mut self.work);
        let chunk = work.len().max(1);
        let mut peak = 0.0f32;
        let mut deepest = 0.0f32;
        let mut start = 0;
        while start < frames {
            let end = (start + chunk).min(frames);
            let w = &mut work[..end - start];
            // The detector input: the largest square over the channels, per frame.
            w.fill(0.0);
            for ch in channels.iter() {
                let ch = ch.get(start..end.min(ch.len())).unwrap_or(&[]);
                for (d, x) in w.iter_mut().zip(ch) {
                    *d = d.max(x * x);
                }
            }
            for d in w.iter_mut() {
                if !(*d >= TINY_SQ && *d < f32::INFINITY) {
                    *d = 0.0;
                }
                peak = peak.max(*d);
            }
            if s.keyed {
                w.fill(0.0);
                for ch in key.unwrap_or(&[]) {
                    let ch = ch.get(start..end.min(ch.len())).unwrap_or(&[]);
                    for (d, x) in w.iter_mut().zip(ch) {
                        *d = d.max(x * x);
                    }
                }
                for d in w.iter_mut() {
                    if !(*d >= TINY_SQ && *d < f32::INFINITY) {
                        *d = 0.0;
                    }
                }
            }
            // The gain computer, in place: square in, output gain out, in passes so the
            // conversions vectorise and only the two smoothers are sample by sample.
            // Fully dry (mix 0) gives gain 1 but keeps the detector running, so a mix
            // change starts settled.
            if s.rms {
                // ms[n] = a ms[n-1] + b d[n], four frames per step: only `a^4 ms` is on
                // the recursion's critical path. The floor keeps a decaying `ms` out of
                // denormals without a branch; it is far below every `quiet_sq`.
                let a = self.rms_coef;
                let b = 1.0 - a;
                let (a2, a3, a4) = (a * a, a * a * a, a * a * a * a);
                let mut ms = self.ms;
                let mut quads = w.chunks_exact_mut(4);
                for q in &mut quads {
                    let e0 = b * q[0] + RMS_FLOOR;
                    let e1 = b * q[1] + RMS_FLOOR;
                    let e2 = b * q[2] + RMS_FLOOR;
                    let e3 = b * q[3] + RMS_FLOOR;
                    let p1 = a * e0 + e1;
                    let p2 = a * p1 + e2;
                    let p3 = a * p2 + e3;
                    q[0] = a * ms + e0;
                    q[1] = a2 * ms + p1;
                    q[2] = a3 * ms + p2;
                    ms = a4 * ms + p3;
                    q[3] = ms;
                }
                for d in quads.into_remainder() {
                    ms = a * ms + (b * *d + RMS_FLOOR);
                    *d = ms;
                }
                self.ms = ms;
            }
            let mut any = 0u32;
            for d in w.iter_mut() {
                let target = Self::curve(&s, 0.5 * DB_PER_LOG2 * fast_log2(d.max(1.0e-30)));
                *d = if *d < s.quiet_sq { 0.0 } else { target };
                any |= d.to_bits();
            }
            if any == 0 && self.gr_db == 0.0 {
                // Below the knee all part long with no reduction left: the common idle
                // case of a voice under the threshold costs no smoothing or exp.
                w.fill(s.makeup_gain);
            } else {
                self.smooth(&s, w, &mut deepest);
            }
            // x * 1.0 is x, so frames without reduction or makeup stay bit-exact.
            for ch in channels.iter_mut() {
                let len = ch.len();
                let ch = ch.get_mut(start..end.min(len)).unwrap_or(&mut []);
                for (x, g) in ch.iter_mut().zip(w.iter()) {
                    *x = clean(*x * g);
                }
            }
            start = end;
        }
        self.work = work;
        let deepest = if deepest > SNAP_DB { 0.0 } else { deepest };
        self.meter.store(amp_to_db(peak.sqrt()), -deepest);
    }
}

impl AudioDsp for CompressorDsp {
    fn set(&mut self, values: &Values) {
        self.s = Self::settings(self.rate, values);
    }

    fn reset(&mut self) {
        self.ms = 0.0;
        self.gr_db = 0.0;
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        self.run(channels, None, frames);
    }

    fn process_keyed(
        &mut self,
        channels: &mut [&mut [f32]],
        key: Option<&[&[f32]]>,
        frames: usize,
    ) {
        self.run(channels, key, frames);
    }

    fn meter(&self) -> Option<Arc<FilterMeter>> {
        Some(Arc::clone(&self.meter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_log2_and_exp2_are_accurate() {
        let mut x = 1.0e-6f32;
        while x < 100.0 {
            let err_db = DB_PER_LOG2 * (fast_log2(x) - x.log2()).abs();
            assert!(err_db < 0.001, "log2({x}) off by {err_db} dB");
            x *= 1.037;
        }
        let mut y = -40.0f32;
        while y < 10.0 {
            let rel = (fast_exp2(y) / y.exp2() - 1.0).abs();
            assert!(rel < 1.0e-5, "exp2({y}) off by {rel}");
            y += 0.0137;
        }
    }
}
