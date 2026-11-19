//! Gain: a level change in dB, the only boost in the product (ADR-0082 decision 4).

use rbs_core::media::AudioFormat;
use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, clean, db_to_amp};

/// Type id of [`Gain`].
pub const GAIN: &str = "org.rbs.audio-filter.gain";

/// Ramp of a gain change, in seconds (linear in amplitude).
const RAMP_SECONDS: f32 = 0.010;

const FIELDS: [Field; 1] = [Field::number("gain", "Gain", -30.0, 30.0, 0.0, 0.5, "dB")
    .with_help("Raises or lowers the source. Above 0 dB the signal can clip later in the chain.")];

/// The Gain filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gain;

impl AudioFilterType for Gain {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(GAIN)
    }
    fn name(&self) -> &'static str {
        "Gain"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(GainDsp::new(setup.format, values)))
    }
}

/// The Gain DSP: a linear amplitude ramp of 10 ms towards the target.
#[derive(Debug)]
pub struct GainDsp {
    ramp_samples: u32,
    current: f32,
    target: f32,
    step: f32,
    left: u32,
}

impl GainDsp {
    /// A gain at `values`' level, steady from the start.
    pub fn new(format: AudioFormat, values: &Values) -> Self {
        let target = Self::amp(values);
        Self {
            ramp_samples: ((format.sample_rate.hz() as f32 * RAMP_SECONDS) as u32).max(1),
            current: target,
            target,
            step: 0.0,
            left: 0,
        }
    }

    fn amp(values: &Values) -> f32 {
        db_to_amp(values.number(0).clamp(-30.0, 30.0))
    }
}

impl AudioDsp for GainDsp {
    fn set(&mut self, values: &Values) {
        let target = Self::amp(values);
        if target != self.target {
            self.target = target;
            self.left = self.ramp_samples;
            self.step = (target - self.current) / self.ramp_samples as f32;
        }
    }

    fn reset(&mut self) {
        self.current = self.target;
        self.left = 0;
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        if self.left == 0 && self.current == 1.0 {
            // Steady unity. A normal or an already flushed sample is left in place.
            // NaN, infinity, a denormal and -0 still become 0, as `clean` wrote.
            flush_non_normals(channels, frames);
            return;
        }
        let (start, left, step, target) = (self.current, self.left, self.step, self.target);
        for ch in channels.iter_mut() {
            let ch = &mut ch[..frames];
            let ramp = (left as usize).min(frames);
            let mut g = start;
            for x in &mut ch[..ramp] {
                g += step;
                *x = clean(*x * g);
            }
            if ramp as u32 == left {
                for x in &mut ch[ramp..] {
                    *x = clean(*x * target);
                }
            }
        }
        let ramp = (left as usize).min(frames) as u32;
        self.left = left - ramp;
        self.current = if self.left == 0 {
            target
        } else {
            start + step * ramp as f32
        };
    }
}

/// A sample `clean` replaces with silence: NaN, infinity, a denormal, or -0.
/// Positive zero and every finite normal number are already that result.
#[inline]
fn needs_flush(x: f32) -> bool {
    let bits = x.to_bits();
    let exponent = bits & 0x7f80_0000;
    bits != 0 && (exponent == 0 || exponent == 0x7f80_0000)
}

/// One pass over a unity block. Clean samples are not written.
fn flush_non_normals(channels: &mut [&mut [f32]], frames: usize) {
    for ch in channels.iter_mut() {
        for x in ch[..frames].iter_mut() {
            if needs_flush(*x) {
                *x = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unity() -> GainDsp {
        let values = Values::defaults(&Gain as &dyn crate::AudioFilterType);
        GainDsp::new(AudioFormat::STEREO_48K, &values)
    }

    /// The per-sample walk unity gain used to run.
    fn old_walk(samples: &mut [f32]) {
        for x in samples.iter_mut() {
            *x = clean(*x);
        }
    }

    #[test]
    fn unity_gain_of_a_clean_block_matches_the_flush_walk() {
        let mut dsp = unity();
        let clean_l = [
            0.0f32,
            0.25,
            -0.5,
            1.0,
            -1.0,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
        ];
        let clean_r = [0.1f32, -0.2, 0.0, 0.5, -0.75, 0.125, -0.0625];
        let mut l = clean_l;
        let mut r = clean_r;
        let mut expect_l = clean_l;
        let mut expect_r = clean_r;
        old_walk(&mut expect_l);
        old_walk(&mut expect_r);
        let frames = l.len();
        dsp.process(&mut [&mut l[..], &mut r[..]], frames);
        assert_eq!(l.map(f32::to_bits), expect_l.map(f32::to_bits));
        assert_eq!(r.map(f32::to_bits), expect_r.map(f32::to_bits));
        assert_eq!(l.map(f32::to_bits), clean_l.map(f32::to_bits));

        let dirty = [
            0.5f32,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1.0e-40,
            -1.0e-40,
            -0.0,
            0.0,
        ];
        let mut l = dirty;
        let mut r = dirty;
        let mut expect_l = dirty;
        let mut expect_r = dirty;
        old_walk(&mut expect_l);
        old_walk(&mut expect_r);
        let frames = l.len();
        dsp.process(&mut [&mut l[..], &mut r[..]], frames);
        assert_eq!(l.map(f32::to_bits), expect_l.map(f32::to_bits));
        assert_eq!(r.map(f32::to_bits), expect_r.map(f32::to_bits));
        assert_eq!(l[1].to_bits(), 0);
        assert_eq!(l[2].to_bits(), 0);
        assert_eq!(l[4].to_bits(), 0);
        assert_eq!(l[6].to_bits(), 0);

        // Any other steady gain, including silence, still multiplies and flushes.
        let mut dsp = unity();
        dsp.current = 2.0;
        dsp.target = 2.0;
        let scaled = [0.25f32, f32::NAN, 1.0e-40, -0.0];
        let mut l = scaled;
        let mut r = scaled;
        let mut expect_l = scaled;
        let mut expect_r = scaled;
        for x in expect_l.iter_mut().chain(expect_r.iter_mut()) {
            *x = clean(*x * 2.0);
        }
        let frames = l.len();
        dsp.process(&mut [&mut l[..], &mut r[..]], frames);
        assert_eq!(l.map(f32::to_bits), expect_l.map(f32::to_bits));
        assert_eq!(r.map(f32::to_bits), expect_r.map(f32::to_bits));

        dsp.current = 0.0;
        dsp.target = 0.0;
        let mut l = scaled;
        let mut r = scaled;
        let mut expect_l = scaled;
        let mut expect_r = scaled;
        for x in expect_l.iter_mut().chain(expect_r.iter_mut()) {
            *x = clean(*x * 0.0);
        }
        let frames = l.len();
        dsp.process(&mut [&mut l[..], &mut r[..]], frames);
        assert_eq!(l.map(f32::to_bits), expect_l.map(f32::to_bits));
        assert_eq!(r.map(f32::to_bits), expect_r.map(f32::to_bits));
    }
}
