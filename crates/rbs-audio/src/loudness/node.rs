//! The loudness tap: a sink node that runs a [`LoudnessMeter`] on the block it reads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use rbs_core::media::MAX_CHANNELS;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use super::{Loudness, LoudnessMeter};
use crate::config::AudioGraphConfig;

/// Node type id of [`LoudnessTapNode`].
pub const LOUDNESS_TAP_NODE_TYPE: &str = "org.rbs.audio.loudness-tap";

/// The descriptor of loudness taps: kind `Output`, one audio input "in", no outputs.
pub fn loudness_tap_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(LOUDNESS_TAP_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    )
}

/// The values of a running loudness tap, written by the audio thread after each block
/// and read by any thread without locks. Each value is one atomic, so a reader may
/// see values of two neighbouring blocks together, never a torn value.
#[derive(Debug)]
pub struct LoudnessReadout {
    momentary: AtomicU32,
    short_term: AtomicU32,
    integrated: AtomicU32,
    range: AtomicU32,
    true_peak: AtomicU32,
    max_momentary: AtomicU32,
    max_short_term: AtomicU32,
    reset: AtomicBool,
}

impl LoudnessReadout {
    fn new() -> Self {
        let none = |v: f32| AtomicU32::new(v.to_bits());
        let n = Loudness::NONE;
        Self {
            momentary: none(n.momentary),
            short_term: none(n.short_term),
            integrated: none(n.integrated),
            range: none(n.range),
            true_peak: none(n.true_peak),
            max_momentary: none(n.max_momentary),
            max_short_term: none(n.max_short_term),
            reset: AtomicBool::new(false),
        }
    }

    /// The values the tap published last.
    pub fn get(&self) -> Loudness {
        let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
        Loudness {
            momentary: f(&self.momentary),
            short_term: f(&self.short_term),
            integrated: f(&self.integrated),
            range: f(&self.range),
            true_peak: f(&self.true_peak),
            max_momentary: f(&self.max_momentary),
            max_short_term: f(&self.max_short_term),
        }
    }

    /// Asks the tap to restart the programme measurement (integrated loudness, range,
    /// maximum true peak and maxima) at its next block. Until then [`get`](Self::get)
    /// still shows the old values.
    pub fn reset(&self) {
        self.reset.store(true, Ordering::Relaxed);
    }

    fn publish(&self, v: &Loudness) {
        let s = |a: &AtomicU32, x: f32| a.store(x.to_bits(), Ordering::Relaxed);
        s(&self.momentary, v.momentary);
        s(&self.short_term, v.short_term);
        s(&self.integrated, v.integrated);
        s(&self.range, v.range);
        s(&self.true_peak, v.true_peak);
        s(&self.max_momentary, v.max_momentary);
        s(&self.max_short_term, v.max_short_term);
    }
}

/// Counters of a loudness tap.
#[derive(Debug, Default)]
pub struct LoudnessTapStats {
    blocks: AtomicU64,
    invalid_inputs: AtomicU64,
}

impl LoudnessTapStats {
    /// Blocks measured.
    pub fn blocks(&self) -> u64 {
        self.blocks.load(Ordering::Relaxed)
    }

    /// Blocks refused because their format or length was not the graph's.
    pub fn invalid_inputs(&self) -> u64 {
        self.invalid_inputs.load(Ordering::Relaxed)
    }
}

/// A sink that measures the loudness of the block on its input (ADR-0092 stage 1).
///
/// It is a parallel tap: it reads its producer's block and changes nothing, so adding
/// or removing it never alters what other consumers get. It exists only while
/// loudness is shown (ADR-0013): without it nothing of the meter runs.
pub struct LoudnessTapNode {
    desc: NodeDescriptor,
    config: AudioGraphConfig,
    meter: LoudnessMeter,
    readout: Arc<LoudnessReadout>,
    stats: Arc<LoudnessTapStats>,
}

impl LoudnessTapNode {
    /// A tap for blocks of `config`, with its readout and counters. Allocates; call
    /// off the audio thread.
    pub fn new(config: AudioGraphConfig) -> (Self, Arc<LoudnessReadout>, Arc<LoudnessTapStats>) {
        let readout = Arc::new(LoudnessReadout::new());
        let stats = Arc::new(LoudnessTapStats::default());
        let node = Self {
            desc: loudness_tap_descriptor(),
            config,
            meter: LoudnessMeter::new(config.format),
            readout: Arc::clone(&readout),
            stats: Arc::clone(&stats),
        };
        (node, readout, stats)
    }
}

impl<X: Runtime> Node<X> for LoudnessTapNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        if self.readout.reset.swap(false, Ordering::Relaxed) {
            self.meter.reset();
        }
        let Some(block) = ctx.inputs().audio(0, 0) else {
            return Ok(());
        };
        let frames = block.frames();
        if block.format() != self.config.format || frames > self.config.block_frames as usize {
            self.stats.invalid_inputs.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let mut chans: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, slot) in chans.iter_mut().enumerate().take(self.meter.channels()) {
            *slot = block.channel(c).unwrap_or(&[]);
        }
        self.meter.process(&chans[..self.meter.channels()], frames);
        self.readout.publish(&self.meter.loudness());
        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_a_valid_audio_output() {
        let d = loudness_tap_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
        let (node, readout, _) = LoudnessTapNode::new(AudioGraphConfig::default());
        assert_eq!(Node::<()>::descriptor(&node), &d);
        assert_eq!(readout.get(), Loudness::NONE);
    }
}
