//! Partitions and queues between executors (ADR-0026).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

#[path = "../benches/support/lean_nodes.rs"]
mod lean_nodes;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lean_nodes::Lean;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::NodeKind;
use rbs_graph::partition::{Lowered, Lowering};
use rbs_graph::runtime::queue::Doorbell;
use rbs_graph::runtime::{Controller, Executor, Node, Tick, channel};

const RENDER: PartitionId = PartitionId::RENDER;
const ENCODER: PartitionId = PartitionId(1);
const OUTPUT: PartitionId = PartitionId(2);

/// source -> compositor (render) -> encoder (partition 1) -> output (partition 2).
struct Rig {
    graph: GraphDescription,
    lowering: Lowering,
    parts: BTreeMap<PartitionId, (Controller<()>, Executor<()>)>,
    instances: BTreeMap<NodeId, Box<dyn Node<()>>>,
    source: NodeId,
    comp: NodeId,
    enc: NodeId,
    out: NodeId,
    packets_seen: Arc<AtomicU64>,
    cadence: Cadence,
    tick: u64,
}

impl Rig {
    fn new() -> Self {
        let mut graph = GraphDescription::new();
        let mut instances: BTreeMap<NodeId, Box<dyn Node<()>>> = BTreeMap::new();
        let mut add = |graph: &mut GraphDescription, kind: NodeKind, p: PartitionId| {
            let node = Lean::new(kind);
            let seen = Arc::clone(&node.seen);
            let desc = Node::<()>::descriptor(&node).clone();
            let id = graph.add_node(Arc::new(desc)).expect("node");
            graph.set_state(id, Lifecycle::Standby).expect("standby");
            graph.set_state(id, Lifecycle::Active).expect("active");
            graph.set_partition(id, p).expect("partition");
            instances.insert(id, Box::new(node));
            (id, seen)
        };
        let (source, _) = add(&mut graph, NodeKind::Source, RENDER);
        let (comp, _) = add(&mut graph, NodeKind::Compositor, RENDER);
        let (enc, _) = add(&mut graph, NodeKind::Encoder, ENCODER);
        let (out, packets_seen) = add(&mut graph, NodeKind::Muxer, OUTPUT);
        graph.connect(source, "out", comp, "in").expect("link");
        graph.connect(comp, "out", enc, "in").expect("link");
        graph.connect(enc, "out", out, "in").expect("link");
        let mut parts = BTreeMap::new();
        for p in [RENDER, ENCODER, OUTPUT] {
            parts.insert(p, channel::<()>(Arc::new(()), ()));
        }
        Self {
            graph,
            lowering: Lowering::new(),
            parts,
            instances,
            source,
            comp,
            enc,
            out,
            packets_seen,
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60),
            tick: 0,
        }
    }

    /// Lowers, adopts new instances and endpoints, applies every part, runs one tick
    /// on each executor and collects. Returns the lowering.
    fn apply(&mut self) -> Lowered {
        let mut lowered = self.lowering.lower(&self.graph);
        for (id, node) in std::mem::take(&mut self.instances) {
            let p = self.graph.node(id).expect("node").partition;
            self.parts
                .get_mut(&p)
                .expect("part")
                .0
                .adopt(id, node)
                .expect("adopt");
        }
        for new in lowered.created.drain(..) {
            let (writer, reader) = new.ends.writer::<()>();
            let c = &new.crossing;
            self.parts
                .get_mut(&c.from)
                .unwrap()
                .0
                .adopt(c.writer, writer)
                .unwrap();
            self.parts
                .get_mut(&c.to)
                .unwrap()
                .0
                .adopt(c.reader, reader.into_node::<()>())
                .unwrap();
        }
        for (p, part) in &lowered.parts {
            let (ctl, exec) = self.parts.get_mut(p).expect("controller");
            let report = ctl.apply(part).expect("apply");
            assert!(report.failed.is_empty(), "{p}: {:?}", report.failed);
            exec.run_tick(Tick::of(&self.cadence, self.tick));
            ctl.collect().expect("executor").expect("swapped");
        }
        self.tick += 1;
        lowered
    }

    /// One tick on every executor, in pipeline order.
    fn run(&mut self) {
        for (_, exec) in self.parts.values_mut() {
            exec.run_tick(Tick::of(&self.cadence, self.tick));
        }
        self.tick += 1;
    }

    fn running(&self, p: PartitionId) -> Vec<NodeId> {
        self.parts[&p].1.scheduled().collect()
    }
}

#[test]
fn links_between_partitions_become_queue_endpoints_with_stable_reserved_ids() {
    let mut rig = Rig::new();
    let lowered = rig.apply();
    assert_eq!(lowered.crossings.len(), 2);
    let video = &lowered.crossings[0];
    let packets = &lowered.crossings[1];
    assert_eq!((video.from, video.to), (RENDER, ENCODER));
    assert_eq!((packets.from, packets.to), (ENCODER, OUTPUT));
    for c in &lowered.crossings {
        assert!(c.writer.is_reserved() && c.reader.is_reserved());
    }
    // Each part holds its own nodes plus the endpoints.
    assert_eq!(
        rig.running(RENDER),
        vec![rig.source, rig.comp, video.writer]
    );
    assert_eq!(
        rig.running(ENCODER),
        vec![video.reader, rig.enc, packets.writer]
    );
    assert_eq!(rig.running(OUTPUT), vec![packets.reader, rig.out]);

    // Lowering again creates nothing and keeps the ids.
    let again = rig.apply();
    assert!(again.created.is_empty() && again.retired.is_empty());
    assert_eq!(again.crossings[0].writer, video.writer);
    assert_eq!(again.crossings[1].reader, packets.reader);
}

#[test]
fn frames_and_packets_flow_through_the_queues() {
    let mut rig = Rig::new();
    let lowered = rig.apply();
    let video = lowered.crossings[0].stats.clone();
    let packets = lowered.crossings[1].stats.clone();
    for _ in 0..10 {
        rig.run();
    }
    // One frame per render tick crosses; the encoder makes one packet per frame it
    // sees, and the output gets every packet.
    assert!(video.sent() >= 10);
    assert_eq!(video.video_drops(), 0);
    assert_eq!(packets.overflows(), 0);
    assert_eq!(packets.received(), packets.sent());
    assert_eq!(rig.packets_seen.load(Ordering::Relaxed), packets.sent());
    assert!(packets.sent() >= 9);
}

#[test]
fn a_slow_encoder_sees_counted_video_drops_and_only_the_newest_frame() {
    let mut rig = Rig::new();
    let lowered = rig.apply();
    let video = lowered.crossings[0].stats.clone();
    let base = video.sent();
    // Render runs five ticks while the encoder does not run: each new frame replaces
    // the one waiting.
    for _ in 0..5 {
        let tick = Tick::of(&rig.cadence, rig.tick);
        rig.parts.get_mut(&RENDER).unwrap().1.run_tick(tick);
        rig.tick += 1;
    }
    assert_eq!(video.sent(), base + 5);
    let before = video.video_drops();
    let tick = Tick::of(&rig.cadence, rig.tick);
    rig.parts.get_mut(&ENCODER).unwrap().1.run_tick(tick);
    assert_eq!(video.received(), 1);
    assert!(before >= 4, "four replaced frames are counted: {before}");
    assert_eq!(video.video_drops(), before, "taking a frame drops nothing");
}

/// Records the timestamp of the video frame it sees, in nanoseconds.
struct Probe {
    desc: rbs_graph::node::NodeDescriptor,
    last: Arc<AtomicU64>,
}

impl Node<()> for Probe {
    fn descriptor(&self) -> &rbs_graph::node::NodeDescriptor {
        &self.desc
    }
    fn prepare(
        &mut self,
        _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>) {}
    fn process(
        &mut self,
        ctx: &mut rbs_graph::runtime::ProcessCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        if let Some(frame) = ctx.inputs().video(0, 0) {
            self.last
                .store(frame.timestamp().as_nanos(), Ordering::Relaxed);
        }
        Ok(())
    }
}

#[test]
fn after_a_stall_the_consumer_gets_the_newest_frame_not_a_stale_one() {
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeTypeId, Pin};
    let mut graph = GraphDescription::new();
    let src_node = Lean::new(NodeKind::Source);
    let src = graph
        .add_node(Arc::new(Node::<()>::descriptor(&src_node).clone()))
        .unwrap();
    let probe = Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        last: Arc::new(AtomicU64::new(u64::MAX)),
    };
    let last = Arc::clone(&probe.last);
    let sink = graph.add_node(Arc::new(probe.desc.clone())).unwrap();
    for id in [src, sink] {
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
    }
    graph.set_partition(sink, ENCODER).unwrap();
    graph.connect(src, "out", sink, "in").unwrap();

    let mut lowering = Lowering::new();
    let mut lowered = lowering.lower(&graph);
    let (mut render, mut render_exec) = channel::<()>(Arc::new(()), ());
    let (mut enc, mut enc_exec) = channel::<()>(Arc::new(()), ());
    render.adopt(src, Box::new(src_node)).unwrap();
    enc.adopt(sink, Box::new(probe)).unwrap();
    let new = lowered.created.pop().unwrap();
    let (writer, reader) = new.ends.writer::<()>();
    render.adopt(new.crossing.writer, writer).unwrap();
    enc.adopt(new.crossing.reader, reader.into_node::<()>())
        .unwrap();
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    render.apply(&lowered.parts[&RENDER]).unwrap();
    enc.apply(&lowered.parts[&ENCODER]).unwrap();
    // Schedules swap in at the end of a tick.
    render_exec.run_tick(Tick::of(&cadence, 0));
    enc_exec.run_tick(Tick::of(&cadence, 0));
    let mut index = 1;
    for _ in 0..5 {
        render_exec.run_tick(Tick::of(&cadence, index));
        index += 1;
    }
    enc_exec.run_tick(Tick::of(&cadence, index));
    let newest = Tick::of(&cadence, index - 1).time.as_nanos();
    assert_eq!(last.load(Ordering::Relaxed), newest);
    assert_eq!(new.crossing.stats.sent(), 5);
    assert_eq!(new.crossing.stats.received(), 1);
    assert_eq!(new.crossing.stats.skipped(), 4);
}

#[test]
fn a_stalled_output_overflows_its_own_queue_only() {
    let mut rig = Rig::new();
    rig.lowering.set_packet_capacity(4);
    let lowered = rig.apply();
    let packets = lowered.crossings[1].stats.clone();
    for _ in 0..10 {
        let tick = Tick::of(&rig.cadence, rig.tick);
        for p in [RENDER, ENCODER] {
            rig.parts.get_mut(&p).unwrap().1.run_tick(tick);
        }
        rig.tick += 1;
    }
    assert!(
        packets.overflows() > 0,
        "the output must notice a broken stream"
    );
    // The encoder kept running.
    let health = rig.parts[&ENCODER].1.health(rig.enc).expect("scheduled");
    assert!(!health.bypassed && health.processed >= 10);
}

#[test]
fn a_crossing_whose_output_is_off_is_not_scheduled_on_either_side() {
    let mut rig = Rig::new();
    rig.apply();
    rig.graph
        .set_state(rig.out, Lifecycle::Standby)
        .expect("standby");
    let lowered = rig.apply();
    // Nothing reaches a sink: every part schedules nothing, like the whole graph.
    assert!(rig.graph.plan().order.is_empty());
    for p in [RENDER, ENCODER, OUTPUT] {
        assert!(rig.running(p).is_empty(), "{p}: {:?}", rig.running(p));
    }
    assert_eq!(lowered.crossings.len(), 2, "queues stay, idle");
}

#[test]
fn moving_a_node_retires_its_queues_and_reports_the_move() {
    let mut rig = Rig::new();
    let first = rig.apply();
    // The encoder moves into the output's partition: its link to the output no longer
    // crosses, and the video queue now goes from render to partition 2.
    rig.graph.set_partition(rig.enc, OUTPUT).expect("partition");
    let lowered = rig.lowering.lower(&rig.graph);
    assert_eq!(lowered.moved, vec![(rig.enc, ENCODER, OUTPUT)]);
    assert_eq!(lowered.retired.len(), 2);
    assert_eq!(lowered.created.len(), 1);
    assert_eq!(lowered.crossings.len(), 1);
    assert_ne!(lowered.crossings[0].reader, first.crossings[0].reader);
    // The emptied partition gets one empty description, then is forgotten.
    assert!(lowered.parts[&ENCODER].nodes().is_empty());
    let next = rig.lowering.lower(&rig.graph);
    assert!(!next.parts.contains_key(&ENCODER));
}

#[test]
fn a_doorbell_wakes_a_parked_worker() {
    let bell = Arc::new(Doorbell::new());
    assert!(
        !bell.wait(Duration::from_millis(1)),
        "times out without a ring"
    );
    bell.ring();
    assert!(
        bell.wait(Duration::from_secs(5)),
        "a ring before waiting is kept"
    );
    let worker = {
        let bell = Arc::clone(&bell);
        std::thread::spawn(move || bell.wait(Duration::from_secs(30)))
    };
    // Ring until the worker has woken (it may not be parked yet on the first ring).
    while !worker.is_finished() {
        bell.ring();
        std::thread::yield_now();
    }
    assert!(worker.join().expect("worker"));
}

#[test]
fn queue_endpoints_do_not_allocate_per_tick() {
    let mut rig = Rig::new();
    rig.apply();
    rig.run();
    rig.run();
    let info = allocation_counter::measure(|| {
        for _ in 0..100 {
            rig.run();
        }
    });
    assert_eq!(info.count_total, 0, "ticks with queues allocated");
}

/// Makes `burst` packets per tick with increasing decode times; every `gop`-th packet
/// is a keyframe.
struct Gop {
    desc: rbs_graph::node::NodeDescriptor,
    burst: u64,
    gop: u64,
    next: u64,
}

impl Node<()> for Gop {
    fn descriptor(&self) -> &rbs_graph::node::NodeDescriptor {
        &self.desc
    }
    fn prepare(
        &mut self,
        _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>) {}
    fn process(
        &mut self,
        ctx: &mut rbs_graph::runtime::ProcessCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        use rbs_core::media::{BitstreamFormat, Bytes, CodecId, EncodedPacket};
        for _ in 0..self.burst {
            let t = Timestamp::from_nanos(self.next);
            let packet = EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                t,
                t,
                self.next.is_multiple_of(self.gop),
                Bytes::from_static(b"\0\0\x01"),
            )
            .map_err(|_| rbs_graph::runtime::NodeError::InvalidInput("timestamps"))?;
            self.next += 1;
            let _ = ctx.outputs().push_packet(0, packet);
        }
        Ok(())
    }
}

/// Records `(decode time, keyframe)` of every packet it gets.
struct Recorder {
    desc: rbs_graph::node::NodeDescriptor,
    got: Arc<std::sync::Mutex<Vec<(u64, bool)>>>,
}

impl Node<()> for Recorder {
    fn descriptor(&self) -> &rbs_graph::node::NodeDescriptor {
        &self.desc
    }
    fn prepare(
        &mut self,
        _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut rbs_graph::runtime::PrepareCtx<'_, ()>) {}
    fn process(
        &mut self,
        ctx: &mut rbs_graph::runtime::ProcessCtx<'_, ()>,
    ) -> Result<(), rbs_graph::runtime::NodeError> {
        let mut got = self.got.lock().unwrap();
        for p in ctx.inputs().packets(0) {
            got.push((p.dts().as_nanos(), p.keyframe()));
        }
        Ok(())
    }
}

/// A `Gop` producer in partition 1 and a `Recorder` in partition 2.
struct PacketRig {
    stats: Arc<rbs_graph::runtime::queue::QueueStats>,
    prod: Executor<()>,
    cons: Executor<()>,
    got: Arc<std::sync::Mutex<Vec<(u64, bool)>>>,
    cadence: Cadence,
    tick: u64,
    bell: Arc<Doorbell>,
    _ctl: (Controller<()>, Controller<()>),
}

impl PacketRig {
    fn new(burst: u64, gop: u64, capacity: usize) -> Self {
        use rbs_graph::node::{LinkKind, NodeDescriptor, NodeTypeId, Pin};
        let gop_node = Gop {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.gop"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Packets)],
            ),
            burst,
            gop,
            next: 0,
        };
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = Recorder {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.recorder"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Packets)],
                vec![],
            ),
            got: Arc::clone(&got),
        };
        let mut graph = GraphDescription::new();
        let a = graph.add_node(Arc::new(gop_node.desc.clone())).unwrap();
        let b = graph.add_node(Arc::new(rec.desc.clone())).unwrap();
        for id in [a, b] {
            graph.set_state(id, Lifecycle::Standby).unwrap();
            graph.set_state(id, Lifecycle::Active).unwrap();
        }
        graph.set_partition(a, ENCODER).unwrap();
        graph.set_partition(b, OUTPUT).unwrap();
        graph.connect(a, "out", b, "in").unwrap();
        let mut lowering = Lowering::new();
        lowering.set_packet_capacity(capacity);
        let mut lowered = lowering.lower(&graph);
        let bell = lowering.doorbell(OUTPUT);
        let (mut pc, mut prod) = channel::<()>(Arc::new(()), ());
        let (mut cc, mut cons) = channel::<()>(Arc::new(()), ());
        pc.adopt(a, Box::new(gop_node)).unwrap();
        cc.adopt(b, Box::new(rec)).unwrap();
        let new = lowered.created.pop().unwrap();
        let stats = Arc::clone(&new.crossing.stats);
        let (writer, reader) = new.ends.writer::<()>();
        pc.adopt(new.crossing.writer, writer).unwrap();
        cc.adopt(new.crossing.reader, reader.into_node::<()>())
            .unwrap();
        pc.apply(&lowered.parts[&ENCODER]).unwrap();
        cc.apply(&lowered.parts[&OUTPUT]).unwrap();
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        // Schedules swap in at the end of a tick (nothing runs in it yet).
        cons.run_tick(Tick::of(&cadence, 0));
        prod.run_tick(Tick::of(&cadence, 0));
        Self {
            stats,
            prod,
            cons,
            got,
            cadence,
            tick: 1,
            bell,
            _ctl: (pc, cc),
        }
    }

    fn tick(&mut self, producer: bool, consumer: bool) {
        let t = Tick::of(&self.cadence, self.tick);
        if producer {
            self.prod.run_tick(t);
        }
        if consumer {
            self.cons.run_tick(t);
        }
        self.tick += 1;
    }

    fn got(&self) -> Vec<(u64, bool)> {
        self.got.lock().unwrap().clone()
    }
}

#[test]
fn after_a_packet_overflow_the_output_resumes_at_a_keyframe() {
    let mut rig = PacketRig::new(3, 5, 4);
    // The output stalls while 12 packets are made into a queue of 4.
    for _ in 0..4 {
        rig.tick(true, false);
    }
    assert!(rig.stats.overflows() > 0);
    for _ in 0..6 {
        rig.tick(true, true);
    }
    assert!(rig.stats.resync_drops() > 0);
    let got = rig.got();
    assert_eq!(&got[..4], &[(0, true), (1, false), (2, false), (3, false)]);
    // Packets arrive in order; after every gap comes a keyframe.
    for pair in got.windows(2) {
        let ((a, _), (b, key)) = (pair[0], pair[1]);
        assert!(b > a, "in order: {got:?}");
        assert!(b == a + 1 || key, "a gap must end at a keyframe: {got:?}");
    }
    assert!(got.len() > 4, "the stream resumed: {got:?}");
}

#[test]
fn packets_carried_over_to_the_next_tick_keep_their_order() {
    let burst = rbs_graph::runtime::PER_TICK_EVENTS as u64;
    let mut rig = PacketRig::new(burst, 1000, 256);
    // Three full bursts wait while the output stalls; the reader passes on at most one
    // burst per tick.
    for _ in 0..3 {
        rig.tick(true, false);
    }
    for _ in 0..3 {
        rig.tick(false, true);
    }
    let got = rig.got();
    assert_eq!(got.len() as u64, 3 * burst);
    for (i, (dts, _)) in got.iter().enumerate() {
        assert_eq!(*dts, i as u64);
    }
    assert_eq!(rig.stats.overflows() + rig.stats.resync_drops(), 0);
}

#[test]
fn a_reader_that_leaves_packets_queued_rings_its_own_doorbell() {
    let burst = rbs_graph::runtime::PER_TICK_EVENTS as u64;
    let mut rig = PacketRig::new(burst, 1000, 256);
    // Two bursts wait; the writer rang.
    rig.tick(true, false);
    rig.tick(true, false);
    assert!(rig.bell.wait(Duration::ZERO));
    // One burst passes; the reader rings so the rest does not wait for a timer. The
    // tick with the ring allocates nothing.
    rig.got.lock().unwrap().reserve(4 * burst as usize); // the recorder's own growth
    let info = allocation_counter::measure(|| rig.tick(false, true));
    assert_eq!(info.count_total, 0, "the reader's ring allocated");
    assert!(rig.bell.wait(Duration::ZERO), "rang for the packets left");
    // The rest passes and nothing is left: no ring.
    rig.tick(false, true);
    assert!(!rig.bell.wait(Duration::ZERO));
    assert_eq!(rig.got().len() as u64, 2 * burst);
}

#[test]
fn one_ring_wakes_a_parked_worker() {
    let bell = Arc::new(Doorbell::new());
    let worker = {
        let bell = Arc::clone(&bell);
        std::thread::spawn(move || bell.wait(Duration::from_secs(30)))
    };
    // Once registered, a single ring wakes it, parked or about to park.
    while !bell.has_worker() {
        std::thread::yield_now();
    }
    let started = std::time::Instant::now();
    bell.ring();
    assert!(worker.join().expect("worker"), "woken by the ring");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "not the timeout"
    );
}

#[test]
fn an_active_node_moves_between_partitions_in_two_phases() {
    let mut rig = Rig::new();
    rig.apply();
    for _ in 0..3 {
        rig.run();
    }
    rig.graph.set_partition(rig.enc, OUTPUT).expect("partition");
    let mut lowered = rig.lowering.lower(&rig.graph);
    for new in lowered.created.drain(..) {
        let (writer, reader) = new.ends.writer::<()>();
        let c = &new.crossing;
        rig.parts
            .get_mut(&c.from)
            .unwrap()
            .0
            .adopt(c.writer, writer)
            .unwrap();
        rig.parts
            .get_mut(&c.to)
            .unwrap()
            .0
            .adopt(c.reader, reader.into_node::<()>())
            .unwrap();
    }
    let swap = |rig: &mut Rig, p: PartitionId, part: &GraphDescription| {
        let tick = Tick::of(&rig.cadence, rig.tick);
        let (ctl, exec) = rig.parts.get_mut(&p).unwrap();
        let report = ctl.apply(part).expect("apply");
        assert!(report.failed.is_empty(), "{p}: {:?}", report.failed);
        exec.run_tick(tick);
        ctl.collect().expect("executor").expect("swapped");
    };
    // Phase 1: the partitions the node leaves.
    let old: Vec<PartitionId> = lowered.moved.iter().map(|m| m.1).collect();
    for p in &old {
        swap(&mut rig, *p, &lowered.parts[p]);
    }
    for (id, from, to) in &lowered.moved {
        let node = rig.parts.get_mut(from).unwrap().0.take(*id).unwrap();
        let node = node.expect("idle instance");
        rig.parts.get_mut(to).unwrap().0.adopt(*id, node).unwrap();
    }
    // Phase 2: every other part.
    for (p, part) in &lowered.parts {
        if !old.contains(p) {
            swap(&mut rig, *p, part);
        }
    }
    // Retired endpoints are idle now; take them back.
    for c in &lowered.retired {
        for (p, id) in [(c.from, c.writer), (c.to, c.reader)] {
            let ctl = &mut rig.parts.get_mut(&p).unwrap().0;
            assert!(!ctl.is_running(id));
            ctl.take(id).unwrap();
        }
    }
    rig.tick += 1;
    let packets_before = rig.packets_seen.load(Ordering::Relaxed);
    for _ in 0..5 {
        rig.run();
    }
    assert!(rig.running(OUTPUT).contains(&rig.enc));
    assert!(rig.running(ENCODER).is_empty());
    assert!(rig.packets_seen.load(Ordering::Relaxed) >= packets_before + 4);
}

/// A second output joins a running packet producer (ADR-0047): the new link gets its
/// own queue, its first packet is a keyframe, the first output sees no gap, and the
/// ticks after the swap allocate nothing.
#[test]
fn a_consumer_added_to_a_running_producer_starts_at_a_keyframe() {
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeTypeId, Pin};
    const SECOND: PartitionId = PartitionId(3);
    let recorder = |got: &Arc<std::sync::Mutex<Vec<(u64, bool)>>>| Recorder {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.recorder"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Packets)],
            vec![],
        ),
        got: Arc::clone(got),
    };
    let gop = Gop {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.gop"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Packets)],
        ),
        burst: 1,
        gop: 10,
        next: 0,
    };
    let first = Arc::new(std::sync::Mutex::new(Vec::with_capacity(256)));
    let second = Arc::new(std::sync::Mutex::new(Vec::with_capacity(256)));
    let rec_a = recorder(&first);
    let rec_b = recorder(&second);
    let mut graph = GraphDescription::new();
    let add = |graph: &mut GraphDescription, desc: &NodeDescriptor, p: PartitionId| {
        let id = graph.add_node(Arc::new(desc.clone())).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        graph.set_partition(id, p).unwrap();
        id
    };
    let g = add(&mut graph, &gop.desc, ENCODER);
    let a = add(&mut graph, &rec_a.desc, OUTPUT);
    graph.connect(g, "out", a, "in").unwrap();
    let mut lowering = Lowering::new();
    let mut parts: BTreeMap<PartitionId, (Controller<()>, Executor<()>)> =
        [ENCODER, OUTPUT, SECOND]
            .into_iter()
            .map(|p| (p, channel::<()>(Arc::new(()), ())))
            .collect();
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let mut tick = 0;
    let mut apply = |graph: &GraphDescription,
                     parts: &mut BTreeMap<PartitionId, (Controller<()>, Executor<()>)>,
                     tick: &mut u64| {
        let mut lowered = lowering.lower(graph);
        for new in lowered.created.drain(..) {
            let (writer, reader) = new.ends.writer::<()>();
            let c = &new.crossing;
            parts
                .get_mut(&c.from)
                .unwrap()
                .0
                .adopt(c.writer, writer)
                .unwrap();
            parts
                .get_mut(&c.to)
                .unwrap()
                .0
                .adopt(c.reader, reader.into_node::<()>())
                .unwrap();
        }
        // Consumers first, as the engine sends them.
        for (p, part) in lowered.parts.iter().rev() {
            let (ctl, exec) = parts.get_mut(p).unwrap();
            let report = ctl.apply(part).unwrap();
            assert!(report.failed.is_empty(), "{p}: {:?}", report.failed);
            exec.run_tick(Tick::of(&cadence, *tick));
            ctl.collect().unwrap().unwrap();
        }
        *tick += 1;
    };
    parts
        .get_mut(&ENCODER)
        .unwrap()
        .0
        .adopt(g, Box::new(gop))
        .unwrap();
    parts
        .get_mut(&OUTPUT)
        .unwrap()
        .0
        .adopt(a, Box::new(rec_a))
        .unwrap();
    apply(&graph, &mut parts, &mut tick);
    let run = |parts: &mut BTreeMap<PartitionId, (Controller<()>, Executor<()>)>,
               tick: &mut u64| {
        for p in [ENCODER, OUTPUT, SECOND] {
            parts
                .get_mut(&p)
                .unwrap()
                .1
                .run_tick(Tick::of(&cadence, *tick));
        }
        *tick += 1;
    };
    // Mid-GOP: 13 packets made, the next keyframe is packet 20.
    for _ in 0..13 {
        run(&mut parts, &mut tick);
    }

    let b = add(&mut graph, &rec_b.desc, SECOND);
    graph.connect(g, "out", b, "in").unwrap();
    parts
        .get_mut(&SECOND)
        .unwrap()
        .0
        .adopt(b, Box::new(rec_b))
        .unwrap();
    apply(&graph, &mut parts, &mut tick);
    run(&mut parts, &mut tick);
    let info = allocation_counter::measure(|| {
        for _ in 0..20 {
            run(&mut parts, &mut tick);
        }
    });
    assert_eq!(info.count_total, 0, "ticks after the swap allocated");

    let a_got = first.lock().unwrap().clone();
    let b_got = second.lock().unwrap().clone();
    for pair in a_got.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].0 + 1,
            "the first output has no gap: {a_got:?}"
        );
    }
    let (start, key) = b_got[0];
    assert!(key, "the joining output starts at a keyframe: {b_got:?}");
    assert!(start >= 13, "and not before it joined: {b_got:?}");
    for pair in b_got.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "then without a gap: {b_got:?}");
    }
    assert_eq!(b_got.last(), a_got.last(), "both are current");
}
