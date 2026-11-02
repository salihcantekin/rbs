//! Minimal nodes that do not allocate in `process`, shared by the executor benchmark
//! and the allocation test. They model the shape of a canvas: sources, a filter per
//! source, a compositor with many inputs, an encoder and two outputs.

#![allow(dead_code, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size, TextureId,
    VideoFormat, VideoFrame,
};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Controller, Node, NodeError, PrepareCtx, ProcessCtx};

pub struct Lean {
    desc: NodeDescriptor,
    kind: NodeKind,
    pub seen: Arc<AtomicU64>,
}

fn format() -> VideoFormat {
    VideoFormat::new(Size::new(1920, 1080), PixelFormat::Bgra8, ColorSpace::SRGB)
}

impl Lean {
    pub fn new(kind: NodeKind) -> Self {
        let (name, inputs, outputs) = match kind {
            NodeKind::Source => ("source", vec![], vec![Pin::one("out", LinkKind::Video)]),
            NodeKind::VideoFilter => (
                "filter",
                vec![Pin::one("in", LinkKind::Video)],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            NodeKind::Compositor => (
                "compositor",
                vec![Pin::many("in", LinkKind::Video)],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            NodeKind::Encoder => (
                "encoder",
                vec![Pin::one("in", LinkKind::Video)],
                vec![Pin::one("out", LinkKind::Packets)],
            ),
            NodeKind::Muxer => (
                "packet-out",
                vec![Pin::one("in", LinkKind::Packets)],
                vec![],
            ),
            _ => ("video-out", vec![Pin::one("in", LinkKind::Video)], vec![]),
        };
        let kind_in_graph = if kind == NodeKind::Muxer {
            NodeKind::Output
        } else {
            kind
        };
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new(format!("org.rbs.bench.{name}")),
                kind_in_graph,
                inputs,
                outputs,
            ),
            kind,
            seen: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Node<()> for Lean {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let inputs = ctx.inputs();
        match self.kind {
            NodeKind::Source => {
                let frame = VideoFrame::texture(
                    format(),
                    tick.time,
                    TextureId::new(tick.index as u32, 0),
                    None,
                );
                let _ = ctx.outputs().set_video(0, Some(frame));
            }
            NodeKind::VideoFilter => {
                let frame = inputs.video(0, 0).and_then(VideoFrame::share);
                let _ = ctx.outputs().set_video(0, frame);
            }
            NodeKind::Compositor => {
                let layers = inputs.videos(0).flatten().count() as u32;
                let frame =
                    VideoFrame::texture(format(), tick.time, TextureId::new(layers, 0), None);
                let _ = ctx.outputs().set_video(0, Some(frame));
            }
            NodeKind::Encoder => {
                if inputs.video(0, 0).is_some() {
                    let packet = EncodedPacket::new(
                        CodecId::H264,
                        BitstreamFormat::AnnexB,
                        tick.time,
                        tick.time,
                        true,
                        Bytes::from_static(b"\0\0\x01"),
                    )
                    .map_err(|_| NodeError::InvalidInput("timestamps"))?;
                    let _ = ctx.outputs().push_packet(0, packet);
                }
            }
            NodeKind::Muxer => {
                self.seen
                    .fetch_add(inputs.packets(0).count() as u64, Ordering::Relaxed);
            }
            _ => {
                if inputs.video(0, 0).is_some() {
                    self.seen.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, ()>) {}
}

/// Builds `sources` sources, each through one filter into a compositor, which feeds a
/// preview output and an encoder with a packet output. Returns the graph (all nodes
/// Active) and the packet output's counter.
pub fn canvas(ctl: &mut Controller<()>, sources: usize) -> (GraphDescription, Arc<AtomicU64>) {
    let mut graph = GraphDescription::new();
    let mut add = |graph: &mut GraphDescription, kind: NodeKind| -> (NodeId, Arc<AtomicU64>) {
        let node = Lean::new(kind);
        let seen = Arc::clone(&node.seen);
        let id = graph
            .add_node(Arc::new(node.desc.clone()))
            .expect("valid descriptor");
        graph
            .set_state(id, Lifecycle::Standby)
            .expect("installed to standby");
        graph
            .set_state(id, Lifecycle::Active)
            .expect("standby to active");
        ctl.adopt(id, Box::new(node)).expect("new id");
        (id, seen)
    };
    let (comp, _) = add(&mut graph, NodeKind::Compositor);
    for _ in 0..sources {
        let (src, _) = add(&mut graph, NodeKind::Source);
        let (filter, _) = add(&mut graph, NodeKind::VideoFilter);
        graph.connect(src, "out", filter, "in").expect("link");
        graph.connect(filter, "out", comp, "in").expect("link");
    }
    let (preview, _) = add(&mut graph, NodeKind::Output);
    let (enc, _) = add(&mut graph, NodeKind::Encoder);
    let (sink, packets) = add(&mut graph, NodeKind::Muxer);
    graph.connect(comp, "out", preview, "in").expect("link");
    graph.connect(comp, "out", enc, "in").expect("link");
    graph.connect(enc, "out", sink, "in").expect("link");
    (graph, packets)
}

/// Adds one more source and filter into the compositor (node 0) of a `canvas` graph.
pub fn add_layer(ctl: &mut Controller<()>, graph: &mut GraphDescription) {
    let comp = graph.nodes()[0].id;
    let mut ids = Vec::new();
    for kind in [NodeKind::Source, NodeKind::VideoFilter] {
        let node = Lean::new(kind);
        let id = graph
            .add_node(Arc::new(node.desc.clone()))
            .expect("valid descriptor");
        graph
            .set_state(id, Lifecycle::Standby)
            .expect("installed to standby");
        graph
            .set_state(id, Lifecycle::Active)
            .expect("standby to active");
        ctl.adopt(id, Box::new(node)).expect("new id");
        ids.push(id);
    }
    graph.connect(ids[0], "out", ids[1], "in").expect("link");
    graph.connect(ids[1], "out", comp, "in").expect("link");
}
