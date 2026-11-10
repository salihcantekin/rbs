//! Built-in audio filters (ADR-0082, spec `audio-filters.md`).
//!
//! An [`AudioFilterType`] describes a filter (id, name, settings schema) and makes an
//! [`AudioDsp`] from validated [`Values`]. [`AudioFilterNode`] runs one DSP inside the
//! audio partition: one audio input, one audio output, no latency. The catalogue holds
//! [`Gain`](gain::Gain) and [`NoiseGate`](gate::NoiseGate).
//!
//! Real-time rules (P6): `process` never allocates, blocks, locks or does I/O; all state
//! is sized in `make` from the [`DspSetup`]. Output is sanitised (no NaN, no infinity,
//! no denormals).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rbs_core::media::AudioFormat;
use rbs_graph::node::NodeTypeId;
pub use rbs_params::{Field, FieldKind, ParamError, Schema, Value, Values};

pub mod catalogue;
pub mod conformance;
pub mod ducker;
pub mod gain;
pub mod gate;
pub mod node;

pub use catalogue::{catalogue, find};
pub use node::{AudioFilterNode, AudioFilterStats, descriptor};

/// Most fields an audio filter type may have (the shared schema limit).
pub const MAX_FIELDS: usize = rbs_params::MAX_FIELDS;

/// What a DSP is made for: the graph's audio format and block size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DspSetup {
    /// The graph's audio format; blocks of any other format never reach the DSP.
    pub format: AudioFormat,
    /// The graph's block size in frames; also the longest block `process` handles.
    pub block_frames: u32,
}

impl DspSetup {
    /// A setup for `format` and blocks of `block_frames` frames.
    pub const fn new(format: AudioFormat, block_frames: u32) -> Self {
        Self {
            format,
            block_frames,
        }
    }
}

/// A kind of audio filter: its identity, its settings schema and its factory.
pub trait AudioFilterType: Send + Sync + 'static {
    /// `org.rbs.audio-filter.<name>` for built-in types; well-formed.
    fn type_id(&self) -> NodeTypeId;
    /// English display name, for example "Noise gate".
    fn name(&self) -> &'static str;
    /// The settings schema, in display order; at most [`MAX_FIELDS`].
    fn fields(&self) -> &'static [Field];
    /// A DSP for `setup` with `values` (already validated against the schema). Runs off
    /// the audio thread; allocate everything here.
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError>;
    /// Whether the node has a second audio input, `key`, that carries another source's
    /// signal (a ducker, ADR-0095). The binder links it from the key source named by the
    /// `Int` setting `key_source`. Default: no.
    fn key_input(&self) -> bool {
        false
    }
}

/// A catalogue entry's schema is its field list, so [`Values`] works on it directly.
impl Schema for dyn AudioFilterType {
    fn fields(&self) -> &'static [Field] {
        AudioFilterType::fields(self)
    }
}

/// The signal processing of one filter instance. Owned by one executor thread.
pub trait AudioDsp: Send {
    /// New values from the control side. Takes effect at once and never resets the state
    /// or makes a step: gains ramp, thresholds and times simply change.
    fn set(&mut self, values: &Values);
    /// Back to the initial state (closed gate, no stale envelope); not for a value edit.
    fn reset(&mut self);
    /// Processes `frames` frames of every channel in place. `frames` is at most the
    /// setup's `block_frames`. No allocation, no blocking.
    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize);
    /// Like [`process`](AudioDsp::process) for a filter with a key input: `key` holds
    /// the key's channels (planar, at least `frames` long each), or `None` when no key
    /// is linked or it produced no block, which a DSP treats as silence. The default
    /// ignores the key.
    fn process_keyed(
        &mut self,
        channels: &mut [&mut [f32]],
        key: Option<&[&[f32]]>,
        frames: usize,
    ) {
        let _ = key;
        self.process(channels, frames);
    }
    /// The live meter, if this filter has one. Called once, before the node is adopted,
    /// so the studio keeps the `Arc` while the executor thread owns the DSP.
    fn meter(&self) -> Option<Arc<FilterMeter>> {
        None
    }
}

/// Two levels a filter publishes once per block for its editor: the input peak and the
/// gain reduction, both in dB. Relaxed atomics; the UI reads them only while visible.
#[derive(Debug)]
pub struct FilterMeter {
    input_peak_db: AtomicU32,
    gain_reduction_db: AtomicU32,
}

/// Level shown for silence.
pub const FLOOR_DB: f32 = -120.0;

impl Default for FilterMeter {
    fn default() -> Self {
        Self {
            input_peak_db: AtomicU32::new(FLOOR_DB.to_bits()),
            gain_reduction_db: AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

impl FilterMeter {
    /// Stores this block's input peak and gain reduction (dB, reduction >= 0).
    pub fn store(&self, input_peak_db: f32, gain_reduction_db: f32) {
        self.input_peak_db
            .store(input_peak_db.to_bits(), Ordering::Relaxed);
        self.gain_reduction_db
            .store(gain_reduction_db.to_bits(), Ordering::Relaxed);
    }

    /// The last input peak in dB (`FLOOR_DB` for silence).
    pub fn input_peak_db(&self) -> f32 {
        f32::from_bits(self.input_peak_db.load(Ordering::Relaxed))
    }

    /// The last gain reduction in dB (0 when open).
    pub fn gain_reduction_db(&self) -> f32 {
        f32::from_bits(self.gain_reduction_db.load(Ordering::Relaxed))
    }
}

/// Amplitude of `db` decibels.
pub(crate) fn db_to_amp(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// Decibels of an amplitude, floored at [`FLOOR_DB`].
pub(crate) fn amp_to_db(amp: f32) -> f32 {
    if amp > 1.0e-6 {
        20.0 * amp.log10()
    } else {
        FLOOR_DB
    }
}

/// `y` if it is a normal finite number, else silence (NaN, infinity and denormals).
#[inline]
pub(crate) fn clean(y: f32) -> f32 {
    if (f32::MIN_POSITIVE..f32::INFINITY).contains(&y.abs()) {
        y
    } else {
        0.0
    }
}
