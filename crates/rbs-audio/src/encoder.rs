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
///
/// End of stream (ADR-0045): the first final tick (`Tick::draining`) without a block
/// on its input is the end of its input (the audio queue's reader rings for one more
/// final tick after its last block). On it the node flushes the session, passes the
/// tail on and takes no more blocks until it is prepared again.
pub struct EncoderNode {
    desc: NodeDescriptor,
    encoder: Box<dyn AudioEncoder>,
    /// Packets drained but not passed on yet (`PER_TICK_EVENTS` per tick).
    waiting: VecDeque<EncodedPacket>,
    drained: Vec<EncodedPacket>,
    capacity: usize,
    dropped: Arc<AtomicU64>,
    /// The session was flushed on a final tick: it takes no more blocks.
    ended: bool,
    flushes: Arc<AtomicU64>,
    tail_lost: Arc<AtomicU64>,
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
            ended: false,
            flushes: Arc::default(),
            tail_lost: Arc::default(),
        }
    }

    /// Packets lost because more were waiting than the node holds.
    pub fn dropped(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    /// Final ticks on which the session was flushed into the graph (ADR-0045).
    pub fn flushes(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.flushes)
    }

    /// End-of-stream losses (ADR-0045): packets still waiting when the node is released
    /// after its flush (the packet queue refused them on the last final tick), and
    /// blocks that arrived after the flush.
    pub fn tail_lost(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.tail_lost)
    }
}

impl<X: Runtime> Node<X> for EncoderNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        self.ended = false;
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        // Without a final tick that ended the input (a live edit, a drain that timed
        // out) the tail the codec still holds is dropped here.
        if self.ended {
            self.tail_lost
                .fetch_add(self.waiting.len() as u64, Ordering::Relaxed);
        }
        self.drained.clear();
        let _ = self.encoder.flush(&mut self.drained);
        self.drained.clear();
        self.waiting.clear();
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let draining = ctx.tick().draining;
        let block = ctx.inputs().audio(0, 0);
        let block = if self.ended {
            if block.is_some() {
                self.tail_lost.fetch_add(1, Ordering::Relaxed);
            }
            None
        } else {
            block
        };
        let ended = match block {
            Some(block) => {
                self.encoder
                    .submit(&block)
                    .map_err(|_| NodeError::Backend("audio encoder submit failed"))?;
                false
            }
            // End of input (ADR-0045): a final tick without a block. The flush may
            // pad the last frame and allocate; final ticks are bounded by the drain
            // timeout (ADR-0043 Decision 4).
            None => draining && !self.ended,
        };
        if ended {
            self.ended = true;
            self.encoder
                .flush(&mut self.drained)
                .map_err(|_| NodeError::Backend("audio encoder flush failed"))?;
            self.flushes.fetch_add(1, Ordering::Relaxed);
        } else if !self.ended {
            self.encoder
                .drain(&mut self.drained)
                .map_err(|_| NodeError::Backend("audio encoder drain failed"))?;
        }
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
