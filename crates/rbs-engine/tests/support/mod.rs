//! Small nodes for the engine tests and the cross-thread bench. They work with every
//! `Runtime` and do not allocate in `process`.

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size, TextureId,
    VideoFormat, VideoFrame,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// The role of a test node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Video out, one frame per tick.
    Source,
    /// Video in, one packet out per new frame.
    Encoder,
    /// Packets in; counts them.
    PacketSink,
    /// Video in; counts frames.
    VideoSink,
}

/// Shared observations of one node.
#[derive(Debug, Default)]
pub struct Probe {
    /// Values seen (frames or packets).
    pub seen: AtomicU64,
    /// `process` calls.
    pub ticks: AtomicU64,
    /// `prepare` calls.
    pub prepares: AtomicU64,
    /// Sum of latencies, in ns, from the timestamp carried by a packet to the moment a
    /// sink saw it, read on the probe's clock.
    pub latency_ns: AtomicU64,
    /// The latest latencies, in ns, as a ring (preallocated; see `Probe::with_samples`).
    pub samples: Vec<AtomicU64>,
}

impl Probe {
    /// A probe that also keeps the last `n` latencies.
    pub fn with_samples(n: usize) -> Self {
        Self {
            samples: (0..n).map(|_| AtomicU64::new(0)).collect(),
            ..Self::default()
        }
    }
}

/// Records the order in which nodes were released, by name.
pub type ReleaseLog = Arc<Mutex<Vec<&'static str>>>;

pub struct TestNode {
    desc: NodeDescriptor,
    role: Role,
    name: &'static str,
    pub probe: Arc<Probe>,
    releases: Option<ReleaseLog>,
    /// Fail `process` while `prepares` is at most this.
    fail_until_prepare: u64,
    /// The last frame time the encoder turned into a packet.
    last: Option<Timestamp>,
    /// Clock for latency measurements (sinks) and packet stamps (encoder).
    clock: Option<Arc<dyn Clock>>,
}

fn format() -> VideoFormat {
    VideoFormat::new(Size::new(1920, 1080), PixelFormat::Bgra8, ColorSpace::SRGB)
}

impl TestNode {
    pub fn new(role: Role, name: &'static str) -> Self {
        let (kind, inputs, outputs) = match role {
            Role::Source => (
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            Role::Encoder => (
                NodeKind::Encoder,
                vec![Pin::one("in", LinkKind::Video)],
                vec![Pin::one("out", LinkKind::Packets)],
            ),
            Role::PacketSink => (
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Packets)],
                vec![],
            ),
            Role::VideoSink => (
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
        };
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new(format!("org.rbs.test.{role:?}").to_lowercase()),
                kind,
                inputs,
                outputs,
            ),
            role,
            name,
            probe: Arc::new(Probe::default()),
            releases: None,
            fail_until_prepare: 0,
            last: None,
            clock: None,
        }
    }

    /// Logs `release` calls to `log`.
    pub fn logging(mut self, log: &ReleaseLog) -> Self {
        self.releases = Some(Arc::clone(log));
        self
    }

    /// `process` fails for the first `instances` prepared instances.
    pub fn failing(mut self, instances: u64) -> Self {
        self.fail_until_prepare = instances;
        self
    }

    /// Uses `probe` (for example one with samples).
    pub fn with_probe(mut self, probe: Probe) -> Self {
        self.probe = Arc::new(probe);
        self
    }

    /// Stamps packets (encoder) or measures latency (sinks) on `clock`.
    pub fn timed(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    pub fn descriptor_arc(&self) -> Arc<NodeDescriptor> {
        Arc::new(self.desc.clone())
    }
}

impl<X: Runtime> Node<X> for TestNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        self.probe.prepares.fetch_add(1, Ordering::Relaxed);
        self.last = None;
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        self.probe.ticks.fetch_add(1, Ordering::Relaxed);
        if self.probe.prepares.load(Ordering::Relaxed) <= self.fail_until_prepare {
            return Err(NodeError::Backend("scripted failure"));
        }
        let tick = ctx.tick();
        let inputs = ctx.inputs();
        match self.role {
            Role::Source => {
                let frame = VideoFrame::texture(
                    format(),
                    tick.time,
                    TextureId::new(tick.index as u32, 0),
                    None,
                );
                let _ = ctx.outputs().set_video(0, Some(frame));
            }
            Role::Encoder => {
                let Some(frame) = inputs.video(0, 0) else {
                    return Ok(());
                };
                let t = frame.timestamp();
                if self.last == Some(t) {
                    return Ok(());
                }
                self.last = Some(t);
                self.probe.seen.fetch_add(1, Ordering::Relaxed);
                let stamp = self.clock.as_ref().map_or(t, |c| c.now());
                let packet = EncodedPacket::new(
                    CodecId::H264,
                    BitstreamFormat::AnnexB,
                    stamp,
                    stamp,
                    true,
                    Bytes::from_static(b"\0\0\x01"),
                )
                .map_err(|_| NodeError::InvalidInput("timestamps"))?;
                let _ = ctx.outputs().push_packet(0, packet);
            }
            Role::PacketSink => {
                let now = self.clock.as_ref().map(|c| c.now());
                for p in inputs.packets(0) {
                    self.probe.seen.fetch_add(1, Ordering::Relaxed);
                    if let Some(now) = now {
                        let ns = now.saturating_since(p.pts()).as_nanos() as u64;
                        self.probe.latency_ns.fetch_add(ns, Ordering::Relaxed);
                        let n = self.probe.samples.len();
                        if n > 0 {
                            let i = self.probe.seen.load(Ordering::Relaxed) as usize % n;
                            self.probe.samples[i].store(ns, Ordering::Relaxed);
                        }
                    }
                }
            }
            Role::VideoSink => {
                if inputs.video(0, 0).is_some() {
                    self.probe.seen.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        if let Some(log) = &self.releases {
            log.lock().unwrap().push(self.name);
        }
    }
}

/// Adds `node` to `graph` as Active in partition `p`.
pub fn add(graph: &mut GraphDescription, node: &TestNode, p: PartitionId) -> NodeId {
    let id = graph.add_node(node.descriptor_arc()).expect("node");
    graph.set_state(id, Lifecycle::Standby).expect("standby");
    graph.set_state(id, Lifecycle::Active).expect("active");
    graph.set_partition(id, p).expect("partition");
    id
}

/// Waits (yielding, not sleeping) until `cond` holds or `limit` real time passed.
/// Returns whether it held. Used only to wait for other threads, never for timing.
pub fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::yield_now();
    }
    cond()
}
