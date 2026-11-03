//! The audio encoder node: mixed blocks in, packets out (ADR-0037 decision 10).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_codec::AudioEncoder;
use rbs_core::media::EncodedPacket;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Node type id of [`EncoderNode`].
pub const ENCODER_NODE_TYPE: &str = "org.rbs.audio.encoder";

/// Encodes the block on its input with an audio encoder session. Runs on an encoder
/// partition, behind an audio queue: encoders allocate packet payloads, which the
/// audio thread must not do.
pub struct EncoderNode {
    desc: NodeDescriptor,
    encoder: Box<dyn AudioEncoder>,
    /// Packets drained but not passed on yet (`PER_TICK_EVENTS` per tick).
    waiting: VecDeque<EncodedPacket>,
    drained: Vec<EncodedPacket>,
    capacity: usize,
    dropped: Arc<AtomicU64>,
}

/// The descriptor of audio encoder nodes.
pub fn encoder_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new(ENCODER_NODE_TYPE),
        NodeKind::Encoder,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![Pin::one("packets", LinkKind::Packets)],
    )
}

impl EncoderNode {
    /// A node for `encoder` that holds up to `packets` packets waiting to be passed on
    /// (reserved here, so `process` does not allocate for them).
    pub fn new(encoder: Box<dyn AudioEncoder>, packets: usize) -> Self {
        let capacity = packets.max(1);
        Self {
            desc: encoder_descriptor(),
            encoder,
            waiting: VecDeque::with_capacity(capacity),
            drained: Vec::with_capacity(capacity),
            capacity,
            dropped: Arc::default(),
        }
    }

    /// Packets lost because more were waiting than the node holds.
    pub fn dropped(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

impl<X: Runtime> Node<X> for EncoderNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        // End of stream through the graph is an open question of the engine spec; the
        // tail the codec still holds is dropped with the queue.
        self.drained.clear();
        let _ = self.encoder.flush(&mut self.drained);
        self.drained.clear();
        self.waiting.clear();
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        if let Some(block) = ctx.inputs().audio(0, 0) {
            self.encoder
                .submit(&block)
                .map_err(|_| NodeError::Backend("audio encoder submit failed"))?;
        }
        self.encoder
            .drain(&mut self.drained)
            .map_err(|_| NodeError::Backend("audio encoder drain failed"))?;
        for packet in self.drained.drain(..) {
            if self.waiting.len() < self.capacity {
                self.waiting.push_back(packet);
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let out = ctx.outputs();
        while let Some(packet) = self.waiting.pop_front() {
            if let Err(rejected) = out.push_packet(0, packet) {
                self.waiting.push_front(rejected.value);
                break;
            }
        }
        Ok(())
    }
}
