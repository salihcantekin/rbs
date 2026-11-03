//! Small nodes for the engine tests and the cross-thread bench. They work with every
//! `Runtime` and do not allocate in `process`.

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size, TextureId,
    VideoFormat, VideoFrame,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime, Tick, WarmCtx};

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
    /// One more than the index of the last tick `process` saw (0: none yet).
    pub next_index: AtomicU64,
    /// `process` calls whose tick index was not above the one before.
    pub repeated_indices: AtomicU64,
    /// Time, in ns, of the last frame a source wrote or an encoder encoded.
    pub last_frame_ns: AtomicU64,
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

/// A gate a node's `cool` (or `process`) waits on, so a test can hold a swap or a tick
/// past the engine's timeout. The test opens it, directly or with [`open_when`] once
/// the engine took the step it waits for; `hold` only bounds the wait so a broken test
/// cannot hang (ADR-0032), it is never the expected duration.
#[derive(Debug)]
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    /// The longest a `cool` waits.
    hold: Duration,
}

impl Default for Gate {
    fn default() -> Self {
        Self::holding(Duration::from_secs(30))
    }
}

impl Gate {
    /// A gate that lets `cool` through after `hold` even if never opened: a hang guard
    /// only. A test whose own thread is blocked in the engine call opens the gate with
    /// [`open_when`] instead of relying on `hold`.
    pub fn holding(hold: Duration) -> Self {
        Self {
            open: Mutex::new(false),
            changed: Condvar::new(),
            hold,
        }
    }

    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn pass(&self) {
        let open = self.open.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(open, self.hold, |open| !*open)
            .unwrap();
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
    /// `cool` waits for this gate.
    cool_gate: Option<Arc<Gate>>,
    /// The first `process` waits for this gate.
    process_gate: Option<Arc<Gate>>,
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
            cool_gate: None,
            process_gate: None,
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

    /// `cool` waits until `gate` opens.
    pub fn cooling_on(mut self, gate: &Arc<Gate>) -> Self {
        self.cool_gate = Some(Arc::clone(gate));
        self
    }

    /// The first `process` waits until `gate` opens (a stalled output).
    pub fn processing_on(mut self, gate: &Arc<Gate>) -> Self {
        self.process_gate = Some(Arc::clone(gate));
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
        let index = tick.index + 1;
        if self.probe.next_index.swap(index, Ordering::Relaxed) >= index {
            self.probe.repeated_indices.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(gate) = self.process_gate.take() {
            gate.pass();
        }
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
                self.probe
                    .last_frame_ns
                    .store(tick.time.as_nanos(), Ordering::Relaxed);
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
                self.probe
                    .last_frame_ns
                    .store(t.as_nanos(), Ordering::Relaxed);
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

    fn cool(&mut self, _ctx: &mut WarmCtx<'_, X>) {
        if let Some(gate) = &self.cool_gate {
            gate.pass();
        }
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

/// Opens `gate` from a helper thread as soon as `cond` holds (checked while yielding).
/// For a test whose own thread is blocked inside an engine call: the gate opens on an
/// engine step the test can observe (a counter, a tick index), never after a duration.
/// Gives up after `limit` (a hang guard) and opens the gate anyway.
pub fn open_when(
    gate: &Arc<Gate>,
    limit: Duration,
    cond: impl FnMut() -> bool + Send + 'static,
) -> std::thread::JoinHandle<bool> {
    let gate = Arc::clone(gate);
    std::thread::spawn(move || {
        let held = eventually(limit, cond);
        gate.open();
        held
    })
}

/// Runtime for tests that wait for a tick by its index: `end_tick` records the index,
/// so a test advances the clock to a tick's time and waits until that tick ran, with
/// no race against counters a worker updates after its tick (ADR-0032).
pub struct Ticks;

/// The `Shared` of [`Ticks`]: one more than the index of the last tick (0: none yet).
#[derive(Debug, Default)]
pub struct TickLog {
    next: AtomicU64,
}

impl TickLog {
    /// One more than the index of the last tick that ran (0: none yet).
    pub fn next(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }
}

impl Runtime for Ticks {
    type Shared = TickLog;
    type Local = ();

    fn end_tick(shared: &TickLog, _: &mut (), tick: Tick) {
        shared.next.store(tick.index + 1, Ordering::Release);
    }
}
