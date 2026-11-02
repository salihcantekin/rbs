//! Tick cost of the executor for a canvas-shaped graph (hot path: render loop).
//! Measures the graph's own overhead: the nodes do almost nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests and benches fail by panicking.

#[path = "support/lean_nodes.rs"]
mod lean_nodes;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::runtime::{Tick, channel};

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

criterion_group!(benches, tick, swap);
criterion_main!(benches);
