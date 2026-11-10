//! Noise gate with hysteresis, hold and a linked peak detector (ADR-0082 decision 4).

use std::sync::Arc;

use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter, amp_to_db, clean, db_to_amp};

/// Type id of [`NoiseGate`].
pub const NOISE_GATE: &str = "org.rbs.audio-filter.noise-gate";

/// Time constant of the peak envelope follower, in seconds.
const DETECTOR_SECONDS: f32 = 0.040;

const FIELDS: [Field; 6] = [
    Field::number(
        "open_threshold",
        "Open threshold",
        -80.0,
        0.0,
        -40.0,
        0.5,
        "dB",
    )
    .with_help("The gate opens when the level rises to this value."),
    Field::number(
        "close_threshold",
        "Close threshold",
        -80.0,
        0.0,
        -46.0,
        0.5,
        "dB",
    )
    .with_help(
        "The gate starts to close when the level falls below this value. Keep it a few dB \
         under the open threshold, or the gate can chatter on a steady tone.",
    ),
    Field::number("attack", "Attack", 0.1, 100.0, 5.0, 0.1, "ms")
        .log()
        .with_help("How fast the gate opens."),
    Field::number("hold", "Hold", 0.0, 500.0, 100.0, 1.0, "ms")
        .with_help("How long the gate stays open after the level falls below the close threshold."),
    Field::number("release", "Release", 5.0, 1000.0, 150.0, 1.0, "ms")
        .log()
        .with_help("How fast the gate closes."),
    Field::number("range", "Range", -80.0, 0.0, -80.0, 0.5, "dB")
        .with_help("How far a closed gate lowers the signal. 0 dB means no effect."),
];

/// The Noise gate filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoiseGate;

impl AudioFilterType for NoiseGate {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(NOISE_GATE)
    }
    fn name(&self) -> &'static str {
        "Noise gate"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(GateDsp::new(setup, values)))
    }
}

/// The gate's settings in the units `process` uses.
#[derive(Debug, Clone, Copy)]
struct Settings {
    open: f32,
    close: f32,
    hold_samples: u32,
    /// dB per sample, positive, while opening.
    up_rate: f32,
    /// dB per sample, positive, while closing.
    down_rate: f32,
    range_db: f32,
}

/// The gate DSP.
#[derive(Debug)]
pub struct GateDsp {
    rate: f32,
    decay: f32,
    s: Settings,
    env: f32,
    open: bool,
    hold_left: u32,
    gain_db: f32,
    /// The amplitude of `cached_db`, so a steady gain costs no `powf` per sample.
    cached: (f32, f32),
    gains: Vec<f32>,
    meter: Arc<FilterMeter>,
}

impl GateDsp {
    /// A closed gate with `values`, sized for `setup`.
    pub fn new(setup: DspSetup, values: &Values) -> Self {
        let rate = setup.format.sample_rate.hz() as f32;
        let s = Self::settings(rate, values);
        Self {
            rate,
            decay: (-1.0 / (DETECTOR_SECONDS * rate)).exp(),
            s,
            env: 0.0,
            open: false,
            hold_left: 0,
            gain_db: s.range_db,
            cached: (s.range_db, db_to_amp(s.range_db)),
            gains: vec![1.0; setup.block_frames as usize],
            meter: Arc::new(FilterMeter::default()),
        }
    }

    fn settings(rate: f32, v: &Values) -> Settings {
        let open_db = v.number(0).clamp(-80.0, 0.0);
        // The close threshold never lies above the open threshold.
        let close_db = v.number(1).clamp(-80.0, 0.0).min(open_db);
        let per_ms = rate / 1000.0;
        let range_db = v.number(5).clamp(-80.0, 0.0);
        let span = -range_db;
        let attack = v.number(2).clamp(0.1, 100.0);
        let release = v.number(4).clamp(5.0, 1000.0);
        Settings {
            open: db_to_amp(open_db),
            close: db_to_amp(close_db),
            hold_samples: (v.number(3).clamp(0.0, 500.0) * per_ms) as u32,
            up_rate: span / (attack * per_ms),
            down_rate: span / (release * per_ms),
            range_db,
        }
    }

    /// Whether the gate is open (for tests and meters).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The current gain in dB (`range` when closed, 0 when open).
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }
}

impl AudioDsp for GateDsp {
    fn set(&mut self, values: &Values) {
        self.s = Self::settings(self.rate, values);
        // A range raised above the current gain would otherwise be a step; move there.
        self.hold_left = self.hold_left.min(self.s.hold_samples);
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.open = false;
        self.hold_left = 0;
        self.gain_db = self.s.range_db;
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        let frames = frames.min(self.gains.len());
        let s = self.s;
        let mut peak = 0.0f32;
        // Pass 1: the linked detector and the gate's gain for every frame.
        for f in 0..frames {
            let mut level = 0.0f32;
            for ch in channels.iter() {
                level = level.max(ch[f].abs());
            }
            // NaN or infinity must not poison the envelope.
            let level = if level.is_finite() { level } else { 0.0 };
            peak = peak.max(level);
            let env = (self.env * self.decay).max(level);
            self.env = if env < 1.0e-9 { 0.0 } else { env };

            if self.env >= s.close && (self.open || self.env >= s.open) {
                self.open = true;
                self.hold_left = s.hold_samples;
            } else if self.open {
                if self.hold_left == 0 {
                    self.open = false;
                } else {
                    self.hold_left -= 1;
                }
            }

            let target = if self.open { 0.0 } else { s.range_db };
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
        // Pass 2: apply the gains to every channel.
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
