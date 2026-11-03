//! Partitions and queues between executors (ADR-0025).

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
    // Render runs five ticks while the encoder does not run.
    for _ in 0..5 {
        let tick = Tick::of(&rig.cadence, rig.tick);
        rig.parts.get_mut(&RENDER).unwrap().1.run_tick(tick);
        rig.tick += 1;
    }
    // Capacity 2: two frames wait, three were dropped by the writer.
    assert_eq!(video.sent(), 2);
    assert_eq!(video.dropped_full(), 3);
    let before = video.video_drops();
    let tick = Tick::of(&rig.cadence, rig.tick);
    rig.parts.get_mut(&ENCODER).unwrap().1.run_tick(tick);
    // The reader took both waiting frames and kept the newest.
    assert_eq!(video.skipped(), 1);
    assert_eq!(video.received(), 1);
    assert_eq!(video.video_drops(), before + 1);
}

#[test]
fn a_stalled_output_overflows_its_own_queue_only() {
    let mut rig = Rig::new();
    rig.lowering.set_capacities(2, 4);
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
fn audio_links_cannot_cross() {
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeTypeId, Pin};
    let mut g = GraphDescription::new();
    let mic = g
        .add_node(Arc::new(NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.mic"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Audio)],
        )))
        .unwrap();
    let enc = g
        .add_node(Arc::new(NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.aac"),
            NodeKind::Encoder,
            vec![Pin::one("in", LinkKind::Audio)],
            vec![Pin::one("out", LinkKind::Packets)],
        )))
        .unwrap();
    g.connect(mic, "out", enc, "in").unwrap();
    g.set_partition(enc, ENCODER).unwrap();
    let lowered = Lowering::new().lower(&g);
    assert_eq!(lowered.problems.len(), 1);
    assert!(lowered.crossings.is_empty());
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
