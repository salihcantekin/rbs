//! The meter sink: keeps a mixer scheduled while nothing else consumes it, so its level
//! meters run (ADR-0041).

use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Node type id of [`MeterSinkNode`].
pub const METER_SINK_NODE_TYPE: &str = "org.rbs.audio.meter-sink";

/// The descriptor of meter sinks: kind `Output`, one audio input "in", no outputs.
pub fn meter_sink_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(METER_SINK_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    )
}

/// An output that discards the block on its input.
///
/// A graph schedules only nodes with a path to an output, so a mixer whose meters a UI
/// shows while nothing records or plays needs one. It runs in the mixer's partition:
/// no queue, no copy, and `process` does nothing.
pub struct MeterSinkNode {
    desc: NodeDescriptor,
}

impl MeterSinkNode {
    /// A meter sink.
    pub fn new() -> Self {
        Self {
            desc: meter_sink_descriptor(),
        }
    }
}

impl Default for MeterSinkNode {
    fn default() -> Self {
        Self::new()
    }
}

impl<X: Runtime> Node<X> for MeterSinkNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn process(&mut self, _ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_a_valid_audio_output() {
        let d = meter_sink_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
        assert_eq!(Node::<()>::descriptor(&MeterSinkNode::new()), &d);
    }
}
