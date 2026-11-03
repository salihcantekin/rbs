//! Property test deferred by the ADR-0026 review: over random graphs and edits, the
//! partitions' plans together schedule exactly what the logical plan schedules.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

#[path = "../benches/support/lean_nodes.rs"]
mod lean_nodes;

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use lean_nodes::Lean;
use rbs_core::lifecycle::Lifecycle;
use rbs_graph::description::{GraphDescription, Link, NodeId, PartitionId};
use rbs_graph::node::NodeKind;
use rbs_graph::partition::Lowering;
use rbs_graph::runtime::Node;

/// A small deterministic generator (no new dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const KINDS: [NodeKind; 6] = [
    NodeKind::Source,
    NodeKind::VideoFilter,
    NodeKind::Compositor,
    NodeKind::Encoder,
    NodeKind::Muxer,  // packet output
    NodeKind::Output, // video output
];

fn random_graph(rng: &mut Lcg, partitions: u32) -> GraphDescription {
    let mut graph = GraphDescription::new();
    let count = 2 + rng.below(12) as usize;
    let mut ids = Vec::new();
    for i in 0..count {
        // Roughly pipeline order (sources first, outputs last), so that links from
        // earlier to later nodes often fit and many nodes reach a sink.
        let lane = (i * KINDS.len()) / count;
        let kind = KINDS[(lane + rng.below(2) as usize).min(KINDS.len() - 1)];
        let node = Lean::new(kind);
        let id = graph
            .add_node(Arc::new(Node::<()>::descriptor(&node).clone()))
            .unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        if rng.below(8) != 0 {
            graph.set_state(id, Lifecycle::Active).unwrap();
        }
        graph
            .set_partition(id, PartitionId(rng.below(u64::from(partitions)) as u32))
            .unwrap();
        ids.push(id);
    }
    // Links only from earlier to later nodes, so the graph stays acyclic; kinds that do
    // not fit are refused by `connect` and skipped.
    for _ in 0..count * 6 {
        let a = rng.below(count as u64) as usize;
        let b = rng.below(count as u64) as usize;
        if a < b {
            let _ = graph.connect(ids[a], "out", ids[b], "in");
        }
    }
    graph
}

/// Random edits: move nodes, switch states, drop links.
fn edit(rng: &mut Lcg, graph: &mut GraphDescription, partitions: u32) {
    let ids: Vec<NodeId> = graph.nodes().iter().map(|n| n.id).collect();
    for id in ids {
        match rng.below(6) {
            0 => {
                let p = PartitionId(rng.below(u64::from(partitions)) as u32);
                graph.set_partition(id, p).unwrap();
            }
            1 => {
                let state = graph.node(id).unwrap().state;
                let next = if state == Lifecycle::Active {
                    Lifecycle::Standby
                } else {
                    Lifecycle::Active
                };
                graph.set_state(id, next).unwrap();
            }
            _ => {}
        }
    }
    if let Some(link) = graph.links().first().copied() {
        if rng.below(3) == 0 {
            graph.disconnect(link);
        }
    }
}

fn check(lowering: &mut Lowering, logical: &GraphDescription, case: usize) {
    let lowered = lowering.lower(logical);
    assert!(
        lowered.problems.is_empty(),
        "case {case}: only video and packets"
    );
    let global = logical.plan();
    let wanted: BTreeSet<NodeId> = global.order.iter().copied().collect();

    let mut scheduled: BTreeSet<NodeId> = BTreeSet::new();
    let mut endpoints: HashSet<NodeId> = HashSet::new();
    let mut part_links: HashSet<(PartitionId, Link)> = HashSet::new();
    for (p, part) in &lowered.parts {
        let plan = part.plan();
        for id in &plan.order {
            if id.is_reserved() {
                endpoints.insert(*id);
            } else {
                assert!(scheduled.insert(*id), "case {case}: {id} in two parts");
                assert_eq!(logical.node(*id).unwrap().partition, *p);
            }
        }
        for l in plan.links {
            part_links.insert((*p, l));
        }
    }
    // The parts schedule exactly the logical plan's nodes.
    assert_eq!(scheduled, wanted, "case {case}");

    // Every live link is carried: inside its partition, or by a scheduled endpoint
    // pair; and an endpoint runs only for a live link.
    let live: HashSet<Link> = global.links.iter().copied().collect();
    let mut live_endpoints: HashSet<NodeId> = HashSet::new();
    for c in &lowered.crossings {
        let is_live = live.contains(&c.link);
        assert_eq!(
            endpoints.contains(&c.writer),
            is_live,
            "case {case}: writer"
        );
        assert_eq!(
            endpoints.contains(&c.reader),
            is_live,
            "case {case}: reader"
        );
        if is_live {
            live_endpoints.insert(c.writer);
            live_endpoints.insert(c.reader);
            let carried = |p: PartitionId, f: &dyn Fn(&Link) -> bool| {
                part_links.iter().any(|(q, l)| *q == p && f(l))
            };
            assert!(
                carried(c.from, &|l| l.from == c.link.from
                    && l.to.node == c.writer
                    && l.kind == c.link.kind),
                "case {case}: producer to writer"
            );
            assert!(
                carried(c.to, &|l| l.from.node == c.reader
                    && l.to == c.link.to
                    && l.kind == c.link.kind),
                "case {case}: reader to consumer"
            );
        }
    }
    assert_eq!(endpoints, live_endpoints, "case {case}: no stray endpoints");
    for l in &global.links {
        let from = logical.node(l.from.node).unwrap().partition;
        let to = logical.node(l.to.node).unwrap().partition;
        if from == to {
            assert!(part_links.contains(&(from, *l)), "case {case}: inner link");
        }
    }
}

#[test]
fn the_parts_plans_together_equal_the_logical_plan() {
    let mut rng = Lcg(0x5eed);
    for case in 0..400 {
        let partitions = 1 + rng.below(4) as u32;
        let mut graph = random_graph(&mut rng, partitions);
        let mut lowering = Lowering::new();
        check(&mut lowering, &graph, case);
        // The same lowering across edits (stable endpoints, moves, retired queues).
        for _ in 0..4 {
            edit(&mut rng, &mut graph, partitions);
            check(&mut lowering, &graph, case);
        }
    }
}
