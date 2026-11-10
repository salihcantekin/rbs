//! Ducker: lowers a source while another source (the key) is loud (ADR-0095).

use std::sync::Arc;

use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter, amp_to_db, clean, db_to_amp};

/// Type id of [`Ducker`].
pub const DUCKER: &str = "org.rbs.audio-filter.ducker";

/// Time constant of the key's peak envelope follower, in seconds.
const DETECTOR_SECONDS: f32 = 0.010;

/// How far below the threshold the key falls before the hold timer starts.
const HYSTERESIS_DB: f32 = 3.0;

/// Index of each field (the order of [`FIELDS`]).
const KEY: usize = 0;
const THRESHOLD: usize = 1;
const AMOUNT: usize = 2;
const ATTACK: usize = 3;
const HOLD: usize = 4;
const RELEASE: usize = 5;

const FIELDS: [Field; 6] = [
    Field::source("key_source", "Key source").with_help(
        "The sound that lowers this one, usually a microphone. It must be in the scene. \
         The level is taken before that source's volume and mute.",
    ),
    Field::number("threshold", "Threshold", -80.0, 0.0, -30.0, 0.5, "dB")
        .with_help("The key must be louder than this to lower the sound."),
    Field::number("amount", "Amount", 0.0, 60.0, 12.0, 0.5, "dB")
        .with_help("How far the sound is lowered while the key is loud."),
    Field::number("attack", "Attack", 1.0, 200.0, 20.0, 1.0, "ms")
        .log()
        .with_help(
            "How fast the sound is lowered. There is no look-ahead, so the first syllable \
             passes before the sound settles: 5 to 20 ms is inaudible for music.",
        ),
    Field::number("hold", "Hold", 0.0, 2000.0, 300.0, 10.0, "ms")
        .with_help("How long the sound stays lowered after the key falls quiet."),
    Field::number("release", "Release", 20.0, 3000.0, 500.0, 10.0, "ms")
        .log()
        .with_help("How fast the sound comes back."),
];

/// The Ducker filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ducker;

impl AudioFilterType for Ducker {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(DUCKER)
    }
    fn name(&self) -> &'static str {
        "Ducker"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(DuckerDsp::new(setup, values)))
    }
    fn key_input(&self) -> bool {
        true
    }
}

/// The ducker's settings in the units `process_keyed` uses.
#[derive(Debug, Clone, Copy)]
struct Settings {
    on: f32,
    off: f32,
    hold_samples: u32,
    amount_db: f32,
    /// dB per sample, positive, while lowering.
    down_rate: f32,
    /// dB per sample, positive, while coming back.
    up_rate: f32,
}

/// The ducker DSP.
#[derive(Debug)]
pub struct DuckerDsp {
    rate: f32,
    decay: f32,
    s: Settings,
    env: f32,
    active: bool,
    hold_left: u32,
    /// The gain in dB: 0 or below.
    gain_db: f32,
    /// The amplitude of `cached.0`, so a steady gain costs no `powf` per sample.
    cached: (f32, f32),
    gains: Vec<f32>,
    meter: Arc<FilterMeter>,
}

impl DuckerDsp {
    /// A ducker with `values`, sized for `setup`, not ducking.
    pub fn new(setup: DspSetup, values: &Values) -> Self {
        let rate = setup.format.sample_rate.hz() as f32;
        Self {
            rate,
            decay: (-1.0 / (DETECTOR_SECONDS * rate)).exp(),
            s: Self::settings(rate, values),
            env: 0.0,
            active: false,
            hold_left: 0,
            gain_db: 0.0,
            cached: (0.0, 1.0),
            gains: vec![1.0; setup.block_frames as usize],
            meter: Arc::new(FilterMeter::default()),
        }
    }

    fn settings(rate: f32, v: &Values) -> Settings {
        let threshold = v.number(THRESHOLD).clamp(-80.0, 0.0);
        let amount_db = v.number(AMOUNT).clamp(0.0, 60.0);
        // The ramps cover the full amount; at least 1 dB so an amount of 0 can still
        // bring a lowered gain back.
        let span = amount_db.max(1.0);
        let per_ms = rate / 1000.0;
        Settings {
            on: db_to_amp(threshold),
            off: db_to_amp(threshold - HYSTERESIS_DB),
            hold_samples: (v.number(HOLD).clamp(0.0, 2000.0) * per_ms) as u32,
            amount_db,
            down_rate: span / (v.number(ATTACK).clamp(1.0, 200.0) * per_ms),
            up_rate: span / (v.number(RELEASE).clamp(20.0, 3000.0) * per_ms),
        }
    }

    /// The current gain in dB (0 when not ducking).
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Whether the key is currently above the threshold (or being held).
    pub fn is_ducking(&self) -> bool {
        self.active
    }
}

impl AudioDsp for DuckerDsp {
    fn set(&mut self, values: &Values) {
        debug_assert!(values.get(KEY).is_some() || values.as_slice().is_empty());
        self.s = Self::settings(self.rate, values);
        self.hold_left = self.hold_left.min(self.s.hold_samples);
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.active = false;
        self.hold_left = 0;
        self.gain_db = 0.0;
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        self.process_keyed(channels, None, frames);
    }

    fn process_keyed(
        &mut self,
        channels: &mut [&mut [f32]],
        key: Option<&[&[f32]]>,
        frames: usize,
    ) {
        let key_len = key.map_or(usize::MAX, |k| k.iter().map(|c| c.len()).min().unwrap_or(0));
        let frames = frames.min(self.gains.len());
        let s = self.s;
        let mut peak = 0.0f32;
        for f in 0..frames {
            let mut level = 0.0f32;
            if let Some(k) = key
                && f < key_len
            {
                for ch in k {
                    level = level.max(ch[f].abs());
                }
            }
            // NaN or infinity must not poison the envelope.
            let level = if level.is_finite() { level } else { 0.0 };
            peak = peak.max(level);
            let env = (self.env * self.decay).max(level);
            self.env = if env < 1.0e-9 { 0.0 } else { env };

            if self.env >= s.off && (self.active || self.env >= s.on) {
                self.active = true;
                self.hold_left = s.hold_samples;
            } else if self.active {
                if self.hold_left == 0 {
                    self.active = false;
                } else {
                    self.hold_left -= 1;
                }
            }

            let target = if self.active { -s.amount_db } else { 0.0 };
            if self.gain_db > target {
                self.gain_db = (self.gain_db - s.down_rate).max(target);
            } else if self.gain_db < target {
                self.gain_db = (self.gain_db + s.up_rate).min(target);
            }
            if self.gain_db != self.cached.0 {
                self.cached = (self.gain_db, db_to_amp(self.gain_db));
            }
            self.gains[f] = if self.gain_db == 0.0 {
                1.0
            } else {
                self.cached.1
            };
        }
        for ch in channels.iter_mut() {
            for (x, &g) in ch[..frames].iter_mut().zip(&self.gains[..frames]) {
                *x = clean(*x * g);
            }
        }
        self.meter.store(amp_to_db(peak), -self.gain_db);
    }

    fn meter(&self) -> Option<Arc<FilterMeter>> {
        Some(Arc::clone(&self.meter))
    }
}
