//! Allocation guard: the executor's tick and its schedule swap must not allocate
//! (performance budgets, hard rules; ADR-0020).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests and benches fail by panicking.

#[path = "../benches/support/lean_nodes.rs"]
mod lean_nodes;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::runtime::{Tick, channel};

#[test]
fn tick_and_swap_do_not_allocate() {
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let (mut graph, packets) = lean_nodes::canvas(&mut ctl, 8);
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    ctl.apply(&graph).expect("apply");
    exec.run_tick(Tick::of(&cadence, 0));
    ctl.collect().expect("executor").expect("swapped");
    // Warm the frame and packet paths once (first share of a static `Bytes` is free,
    // but keep the first tick out of the measurement anyway).
    exec.run_tick(Tick::of(&cadence, 1));

    let steady = allocation_counter::measure(|| {
        for i in 2..1_002 {
            exec.run_tick(Tick::of(&cadence, i));
        }
    });
    assert_eq!(steady.count_total, 0, "steady-state ticks allocated");
    assert_eq!(packets.load(Ordering::Relaxed), 1_001);

    // A live edit: prepared and laid out on this (control) thread, swapped on the
    // executor after its tick. The tick that swaps must not allocate either.
    lean_nodes::add_layer(&mut ctl, &mut graph);
    ctl.apply(&graph).expect("apply");
    let swap = allocation_counter::measure(|| {
        exec.run_tick(Tick::of(&cadence, 1_002));
    });
    assert_eq!(swap.count_total, 0, "the swapping tick allocated");
    let left = ctl.collect().expect("executor").expect("swapped");
    assert!(left.is_empty());
    let after = allocation_counter::measure(|| {
        exec.run_tick(Tick::of(&cadence, 1_003));
    });
    assert_eq!(after.count_total, 0);
    assert_eq!(exec.scheduled().count(), 1 + 9 * 2 + 3);
}
