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
            // Unity: nothing to do except the sanitising.
            for ch in channels.iter_mut() {
                for x in ch[..frames].iter_mut() {
                    *x = clean(*x);
                }
            }
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
