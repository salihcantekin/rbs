//! Tick cost of the executor for a canvas-shaped graph (hot path: render loop).
//! Measures the graph's own overhead: the nodes do almost nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests and benches fail by panicking.

#[path = "support/lean_nodes.rs"]
mod lean_nodes;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::runtime::{Tick, channel, param};

fn tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_tick");
    for sources in [1_usize, 8, 32] {
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        let (graph, _) = lean_nodes::canvas(&mut ctl, sources);
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        ctl.apply(&graph).expect("apply");
        exec.run_tick(Tick::of(&cadence, 0));
        ctl.collect().expect("executor").expect("swapped");
        let nodes = exec.scheduled().count();
        let mut i = 1_u64;
        group.bench_function(BenchmarkId::new("nodes", nodes), |b| {
            b.iter(|| {
                exec.run_tick(Tick::of(&cadence, i));
                i += 1;
            })
        });
    }
    group.finish();
}

fn swap(c: &mut Criterion) {
    // Control-thread cost of applying an edit to a 32-source canvas (plan, layout,
    // send), plus the executor tick that swaps.
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let (graph, _) = lean_nodes::canvas(&mut ctl, 32);
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let mut i = 0_u64;
    c.bench_function("apply_and_swap_32_sources", |b| {
        b.iter(|| {
            ctl.apply(&graph).expect("apply");
            exec.run_tick(Tick::of(&cadence, i));
            i += 1;
            ctl.collect().expect("executor").expect("swapped");
        })
    });
}

fn param_update(c: &mut Criterion) {
    // What a node pays per tick to read its parameters (ADR-0023): the common case
    // (nothing new), and taking a new value of a 64-layer list.
    let mut group = c.benchmark_group("param_update");
    let (_tx, mut rx) = param::channel(vec![0_u64; 64]);
    group.bench_function("unchanged", |b| {
        b.iter(|| std::hint::black_box(rx.update()))
    });
    let (mut tx, mut rx) = param::channel(vec![0_u64; 64]);
    group.bench_function("changed", |b| {
        // Only `update` is timed; the sender's refill runs between the timed calls.
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut next = tx.recycle().unwrap_or_default();
                next.clear();
                next.resize(64, 1);
                tx.send(next);
                let start = std::time::Instant::now();
                std::hint::black_box(rx.update());
                total += start.elapsed();
            }
            total
        })
    });
    // The timer's own cost inside `iter_custom`, to read "changed" against.
    group.bench_function("timer_only", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let start = std::time::Instant::now();
                std::hint::black_box(());
                total += start.elapsed();
            }
            total
        })
    });
    group.finish();
}

fn queue_crossing(c: &mut Criterion) {
    // A frame crossing from the render partition to an encoder partition (ADR-0026):
    // source and video writer on one executor, reader and encoder plus a packet writer
    // on the other, reader and output on a third. Compare with the same pipeline in one
    // partition.
    use rbs_core::lifecycle::Lifecycle;
    use rbs_graph::description::{GraphDescription, PartitionId};
    use rbs_graph::node::NodeKind;
    use rbs_graph::partition::Lowering;
    use rbs_graph::runtime::Node;

    let mut group = c.benchmark_group("queue_crossing");
    for split in [false, true] {
        let mut graph = GraphDescription::new();
        let mut nodes = Vec::new();
        for (kind, p) in [
            (NodeKind::Source, 0),
            (NodeKind::Encoder, 1),
            (NodeKind::Muxer, 2),
        ] {
            let node = lean_nodes::Lean::new(kind);
            let desc = Node::<()>::descriptor(&node).clone();
            let id = graph.add_node(Arc::new(desc)).unwrap();
            graph.set_state(id, Lifecycle::Standby).unwrap();
            graph.set_state(id, Lifecycle::Active).unwrap();
            if split {
                graph.set_partition(id, PartitionId(p)).unwrap();
            }
            nodes.push((id, PartitionId(if split { p } else { 0 }), node));
        }
        graph.connect(nodes[0].0, "out", nodes[1].0, "in").unwrap();
        graph.connect(nodes[1].0, "out", nodes[2].0, "in").unwrap();
        let mut lowered = Lowering::new().lower(&graph);
        let mut parts: Vec<_> = lowered
            .parts
            .keys()
            .map(|p| (*p, channel::<()>(Arc::new(()), ())))
            .collect();
        let part = |parts: &mut Vec<(PartitionId, _)>, p: PartitionId| {
            parts.iter().position(|(q, _)| *q == p).unwrap()
        };
        for (id, p, node) in nodes {
            let i = part(&mut parts, p);
            parts[i].1.0.adopt(id, Box::new(node)).unwrap();
        }
        for new in lowered.created.drain(..) {
            let (w, r) = new.ends.writer::<()>();
            let i = part(&mut parts, new.crossing.from);
            parts[i].1.0.adopt(new.crossing.writer, w).unwrap();
            let i = part(&mut parts, new.crossing.to);
            parts[i]
                .1
                .0
                .adopt(new.crossing.reader, r.into_node())
                .unwrap();
        }
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        for (p, (ctl, exec)) in parts.iter_mut() {
            ctl.apply(&lowered.parts[p]).unwrap();
            exec.run_tick(Tick::of(&cadence, 0));
            ctl.collect().unwrap().unwrap();
        }
        let mut i = 1_u64;
        let name = if split {
            "three_partitions"
        } else {
            "one_partition"
        };
        group.bench_function(name, |b| {
            b.iter(|| {
                for (_, (_, exec)) in parts.iter_mut() {
                    exec.run_tick(Tick::of(&cadence, i));
                }
                i += 1;
            })
        });
    }
    group.finish();
}

criterion_group!(benches, tick, swap, param_update, queue_crossing);
criterion_main!(benches);
