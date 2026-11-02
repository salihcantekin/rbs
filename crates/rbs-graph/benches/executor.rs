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
    group.finish();
}

criterion_group!(benches, tick, swap, param_update);
criterion_main!(benches);
