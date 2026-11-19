//! The RBS audio graph (ADR-0037, `docs/specs/audio-graph.md`).
//!
//! Audio runs as nodes in an audio partition of the engine, ticking once per block on
//! the master clock:
//!
//! - [`InputNode`] reads one [`AudioInputStream`](rbs_audio_io::AudioInputStream),
//!   aligns it to the block's master time, corrects the device's drift and converts its
//!   rate and channels to the graph format, all in one variable-ratio resampler.
//! - [`MixerNode`] sums its inputs with per-strip gain and mute (ramped, never clicking)
//!   and feeds lock-free [`LevelMeter`]s. The UI and the control API each read their
//!   own peak, so neither clears the other's.
//! - [`EncoderNode`] hands mixed blocks to an [`AudioEncoder`](rbs_codec::AudioEncoder)
//!   on an encoder partition, behind an audio queue.
//! - [`OutputNode`] plays a partition's blocks on an
//!   [`AudioOutput`](rbs_audio_io::AudioOutput) (monitoring), resampled to the
//!   device's rate and steered against its clock.
//! - [`LoudnessTapNode`] measures loudness after BS.1770-4 / EBU R 128 on a block it
//!   reads, while loudness is shown ([`loudness`], ADR-0092).
//! - [`SyncProbeNode`] logs the energy of every 1 ms of a source's sound while a sync
//!   offset measurement runs (ADR-0126).
//! - [`MeterSinkNode`] keeps a mixer scheduled for its meters while nothing records.
//!
//! Every block of a partition has the graph's format and length and the timestamp
//! [`AudioGraphConfig::block_time`], so video and audio meet in the muxer by master time.

mod config;
mod encoder;
pub mod gate;
mod input;
pub mod loudness;
mod meter;
mod mixer;
mod output;
pub mod resample;
mod sink;
mod sync_probe;

pub use config::{AudioGraphConfig, AudioGraphError, DEFAULT_LATENCY, MAX_LATENCY, MIN_LATENCY};
pub use encoder::{
    ENCODER_NODE_TYPE, EncoderNode, HOLD_PIN as ENCODER_HOLD_PIN, encoder_descriptor,
    encoder_hold_descriptor,
};
pub use gate::{AudioGate, HoldGate, NoGate, PAUSE_FADE, PauseGate};
pub use input::{
    INPUT_NODE_TYPE, InputAligner, InputCounters, InputNode, InputParams, InputStats,
    MAP_TOLERANCE, MAX_CORRECTION, MAX_PACKET, RESYNC_THRESHOLD, STEER_GAIN, effective_offset_ns,
    input_descriptor,
};
pub use loudness::{
    LOUDNESS_TAP_NODE_TYPE, Loudness, LoudnessMeter, LoudnessReadout, LoudnessTapNode,
    LoudnessTapStats, loudness_tap_descriptor,
};
pub use meter::{LevelMeter, Levels};
pub use mixer::{
    DEFAULT_RAMP, MIXER_NODE_TYPE, MixerNode, MixerParams, MixerStats, MonitorMode, Strip,
    mixer_descriptor,
};
pub use output::{
    DEFAULT_OUTPUT_TARGET, MAX_OUTPUT_VOLUME, OUTPUT_NODE_TYPE, OUTPUT_VOLUME_RAMP, OutputCounters,
    OutputNode, OutputNodeStats, OutputParams, output_descriptor,
};
pub use sink::{METER_SINK_NODE_TYPE, MeterSinkNode, meter_sink_descriptor};
pub use sync_probe::{
    SYNC_PROBE_NODE_TYPE, SYNC_PROBE_STEP, SubBlockEnergy, SyncProbeLog, SyncProbeNode,
    sync_probe_descriptor,
};

/// Copies interleaved `samples` into the first `frames` of each channel.
/// A short block fills the rest of that span with silence. No allocation.
pub fn deinterleave(samples: &[f32], channels: &mut [&mut [f32]], frames: usize) {
    let width = channels.len().max(1);
    let whole = (samples.len() / width).min(frames);
    for (c, dest) in channels.iter_mut().enumerate() {
        let span = frames.min(dest.len());
        let whole = whole.min(span);
        let (head, tail) = dest[..span].split_at_mut(whole);
        for (sample, frame) in head.iter_mut().zip(samples.chunks_exact(width)) {
            *sample = frame[c];
        }
        tail.fill(0.0);
    }
}
