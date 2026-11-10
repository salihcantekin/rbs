//! The audio filter node: one audio input, one audio output, no latency.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::{AudioFormat, MAX_CHANNELS};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};
use rbs_params::{ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter};

/// The descriptor of a node of audio filter type `ty`.
pub fn descriptor(ty: &dyn AudioFilterType) -> NodeDescriptor {
    NodeDescriptor::new(
        ty.type_id(),
        NodeKind::AudioFilter,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![Pin::one("out", LinkKind::Audio)],
    )
}

/// What a filter node shares with the control thread. Atomics only.
#[derive(Debug, Default)]
pub struct AudioFilterStats {
    invalid_inputs: AtomicU64,
}

impl AudioFilterStats {
    /// Input blocks dropped because their format or length was not the graph's.
    pub fn invalid_inputs(&self) -> u64 {
        self.invalid_inputs.load(Ordering::Relaxed)
    }
}

/// Passes audio through unchanged: used when a filter could not be made, so the source
/// stays audible (ADR-0082 decision 8).
#[derive(Debug, Default)]
pub struct PassThrough;

impl AudioDsp for PassThrough {
    fn set(&mut self, _: &Values) {}
    fn reset(&mut self) {}
    fn process(&mut self, _: &mut [&mut [f32]], _: usize) {}
}

/// An audio filter in the graph.
pub struct AudioFilterNode {
    desc: NodeDescriptor,
    format: AudioFormat,
    frames: usize,
    dsp: Box<dyn AudioDsp>,
    params: ParamReceiver<Values>,
    stats: Arc<AudioFilterStats>,
}

/// What [`AudioFilterNode::new`] returns besides the node.
pub type FilterHandles = (
    ParamSender<Values>,
    Option<Arc<FilterMeter>>,
    Arc<AudioFilterStats>,
);

impl AudioFilterNode {
    /// A node of `ty` for `setup` starting with `values`. The sender replaces the values
    /// while the node runs (latest wins); the meter, if the filter has one, is read by
    /// the studio.
    pub fn new(
        ty: &dyn AudioFilterType,
        setup: DspSetup,
        values: Values,
    ) -> Result<(Self, FilterHandles), ParamError> {
        let dsp = ty.make(setup, &values)?;
        Ok(Self::with_dsp(descriptor(ty), setup, dsp, values))
    }

    /// A node of `ty` that passes audio through (the filter could not be made).
    pub fn pass_through(
        ty: &dyn AudioFilterType,
        setup: DspSetup,
        values: Values,
    ) -> (Self, FilterHandles) {
        Self::with_dsp(descriptor(ty), setup, Box::new(PassThrough), values)
    }

    fn with_dsp(
        desc: NodeDescriptor,
        setup: DspSetup,
        dsp: Box<dyn AudioDsp>,
        values: Values,
    ) -> (Self, FilterHandles) {
        let meter = dsp.meter();
        let (tx, rx) = channel(values);
        let stats = Arc::new(AudioFilterStats::default());
        let node = Self {
            desc,
            format: setup.format,
            frames: setup.block_frames as usize,
            dsp,
            params: rx,
            stats: Arc::clone(&stats),
        };
        (node, (tx, meter, stats))
    }
}

impl<X: Runtime> Node<X> for AudioFilterNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        // A filter that joins the schedule starts clean (a closed gate, no stale state).
        let values = *self.params.get();
        self.dsp.set(&values);
        self.dsp.reset();
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {}

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        if self.params.update() {
            self.dsp.set(self.params.get());
        }
        let Some(block) = ctx.inputs().audio(0, 0) else {
            // No input block: publish nothing; the consumer hears silence.
            return Ok(());
        };
        if block.format() != self.format || block.frames() != self.frames {
            self.stats.invalid_inputs.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let n = self.format.channels.channels();
        let mut out = ctx.outputs().audio(0)?;
        out.write(&block)?;
        let mut chans: [&mut [f32]; MAX_CHANNELS] = Default::default();
        for (slot, ch) in chans.iter_mut().zip(out.channels_mut(n)) {
            *slot = ch;
        }
        self.dsp.process(&mut chans[..n], self.frames);
        Ok(())
    }
}
