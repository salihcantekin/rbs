//! The RBS audio graph (ADR-0037, `docs/specs/audio-graph.md`).
//!
//! Audio runs as nodes in an audio partition of the engine, ticking once per block on
//! the master clock:
//!
//! - [`InputNode`] reads one [`AudioInputStream`](rbs_audio_io::AudioInputStream),
//!   aligns it to the block's master time, corrects the device's drift and converts its
//!   rate and channels to the graph format, all in one variable-ratio resampler.
//! - [`MixerNode`] sums its inputs with per-strip gain and mute (ramped, never clicking)
//!   and feeds lock-free [`LevelMeter`]s the UI reads at its own rate.
//! - [`EncoderNode`] hands mixed blocks to an [`AudioEncoder`](rbs_codec::AudioEncoder)
//!   on an encoder partition, behind an audio queue.
//! - [`OutputNode`] plays a partition's blocks on an
//!   [`AudioOutput`](rbs_audio_io::AudioOutput) (monitoring), resampled to the
//!   device's rate and steered against its clock.
//! - [`MeterSinkNode`] keeps a mixer scheduled for its meters while nothing records.
//!
//! Every block of a partition has the graph's format and length and the timestamp
//! [`AudioGraphConfig::block_time`], so video and audio meet in the muxer by master time.

mod config;
mod encoder;
mod input;
mod meter;
mod mixer;
mod output;
pub mod resample;
mod sink;

pub use config::{AudioGraphConfig, AudioGraphError, DEFAULT_LATENCY, MAX_LATENCY, MIN_LATENCY};
pub use encoder::{ENCODER_NODE_TYPE, EncoderNode, encoder_descriptor};
pub use input::{
    INPUT_NODE_TYPE, InputCounters, InputNode, InputParams, InputStats, MAP_TOLERANCE,
    MAX_CORRECTION, MAX_PACKET, RESYNC_THRESHOLD, STEER_GAIN, input_descriptor,
};
pub use meter::{LevelMeter, Levels};
pub use mixer::{
    DEFAULT_RAMP, MIXER_NODE_TYPE, MixerNode, MixerParams, MixerStats, Strip, mixer_descriptor,
};
pub use output::{
    DEFAULT_OUTPUT_TARGET, MAX_OUTPUT_VOLUME, OUTPUT_NODE_TYPE, OUTPUT_VOLUME_RAMP, OutputCounters,
    OutputNode, OutputNodeStats, OutputParams, output_descriptor,
};
pub use sink::{METER_SINK_NODE_TYPE, MeterSinkNode, meter_sink_descriptor};
