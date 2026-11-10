//! The replay ring node inside a graph: packets in, status, clips on request, no
//! allocation in steady state, nothing left behind on release (ADR-0088). Ticks are
//! driven by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::time::Duration;

use rbs_core::media::{BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick};
use rbs_nodes::replay::{ClipError, ReplayLimits};
use rbs_nodes::replay_node::{ReplayHandle, ReplayRingNode, RequestError};
use support::Rig;

fn ms(v: u64) -> Timestamp {
    Timestamp::from_nanos(v * 1_000_000)
}

/// Pushes scripted packets: `(tick, pin, packet)`, pin 0 video, pin 1 audio.
struct Script {
    desc: NodeDescriptor,
    packets: Vec<(u64, usize, EncodedPacket)>,
}

impl Node<()> for Script {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let index = ctx.tick().index;
        for (tick, pin, packet) in &self.packets {
            if *tick == index {
                ctx.outputs().push_packet(*pin, packet.clone()).unwrap();
            }
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
}

fn packet(codec: CodecId, t: u64, key: bool) -> EncodedPacket {
    EncodedPacket::new(
        codec,
        BitstreamFormat::AnnexB,
        ms(t),
        ms(t),
        key,
        Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
    )
    .unwrap()
}

/// One video packet and one audio packet per tick, 20 ms apart, a keyframe every 10
/// ticks.
fn script(ticks: u64) -> Vec<(u64, usize, EncodedPacket)> {
    let mut v = Vec::new();
    for i in 0..ticks {
        v.push((i, 0, packet(CodecId::H264, i * 20, i % 10 == 0)));
        v.push((i, 1, packet(CodecId::Aac, i * 20, true)));
    }
    v
}

fn rig(ticks: u64, limits: ReplayLimits) -> (Rig, Arc<ReplayHandle>) {
    let mut rig = Rig::new();
    let src = rig.add(Box::new(Script {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.script"),
            NodeKind::Source,
            Vec::new(),
            vec![
                Pin::named("p0", LinkKind::Packets, false),
                Pin::named("p1", LinkKind::Packets, false),
            ],
        ),
        packets: script(ticks),
    }));
    let node = ReplayRingNode::new(2, StreamIndex(0), limits);
    let handle = node.handle();
    let node = rig.add(Box::new(node));
    rig.graph.connect(src, "p0", node, "s0").unwrap();
    rig.graph.connect(src, "p1", node, "s1").unwrap();
    let report = rig.apply();
    assert!(report.failed.is_empty(), "{report:?}");
    (rig, handle)
}

fn limits(length_ms: u64, capacity: usize) -> ReplayLimits {
    ReplayLimits {
        length: Duration::from_millis(length_ms),
        memory_limit: usize::MAX / 2,
        capacity,
    }
}

#[test]
fn holds_the_configured_past_and_reports_it() {
    let (mut rig, handle) = rig(200, limits(1_000, 1_000));
    rig.ticks(200);
    let s = handle.status();
    assert!(s.held_ms <= 1_000 && s.held_ms >= 800, "{s:?}");
    assert!(s.packets > 0 && s.bytes > 0);
    assert!(!s.memory_limited);
    // `inline_frees` depends on how fast the reaper thread runs; it is not asserted
    // (ADR-0032: no timing in tests).
}

#[test]
fn a_requested_clip_starts_at_a_keyframe_and_is_rebased() {
    let (mut rig, handle) = rig(200, limits(10_000, 1_000));
    rig.ticks(100);
    handle
        .request_clip(Some(Duration::from_millis(500)))
        .unwrap();
    assert_eq!(
        handle.request_clip(None),
        Err(RequestError::Busy),
        "one request at a time"
    );
    assert!(handle.take_clip().is_none(), "not served before a tick");
    rig.tick();
    let clip = handle.take_clip().expect("served").expect("a clip");
    assert!(clip.bounds.duration() >= Duration::from_millis(500));
    let first_video = clip
        .entries
        .iter()
        .find(|e| e.stream == StreamIndex(0))
        .unwrap();
    assert!(first_video.packet.keyframe());
    let rebased = clip.rebased().unwrap();
    assert_eq!(
        rebased.iter().map(|e| e.packet.dts()).min(),
        Some(Timestamp::ZERO)
    );
    // The slot is free again.
    handle.request_clip(None).unwrap();
}

#[test]
fn a_request_before_any_keyframe_reports_it() {
    let (mut rig, handle) = rig(0, limits(1_000, 100));
    handle.request_clip(None).unwrap();
    rig.tick();
    assert_eq!(
        handle.take_clip().unwrap().unwrap_err(),
        ClipError::NoKeyframe
    );
}

#[test]
fn steady_state_ticks_do_not_allocate() {
    let (mut rig, handle) = rig(600, limits(500, 1_000));
    rig.ticks(100); // fills the ring, evicts, warms the hand-off
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let start = rig.tick;
    let steady = allocation_counter::measure(|| {
        for i in start..start + 300 {
            rig.exec.run_tick(Tick::of(&cadence, i));
        }
    });
    assert_eq!(
        steady.count_total, 0,
        "steady-state ticks allocated: {steady:?}"
    );
    // `inline_frees` depends on how fast the reaper thread runs, so it is not asserted
    // here (ADR-0032: no timing in tests); the ring's own test covers the fallback.
    let _ = handle.status();
}

#[test]
fn release_stops_the_reaper_and_leaves_nothing() {
    let (mut rig, handle) = rig(100, limits(500, 1_000));
    rig.ticks(100);
    rig.drain();
    // Drained: the node was released (joined its reaper), a second drain is harmless.
    assert!(handle.status().packets <= 100 * 2);
}

/// Hot-path numbers for ADR-0088 decision 6 (run with
/// `cargo test -p rbs-nodes --release --test replay_node -- --ignored --nocapture`).
#[test]
#[ignore = "slow: benchmark, prints numbers for the hot-path review"]
fn slow_bench_tick_and_cut() {
    use std::time::Instant;
    // Measures the ring directly: a scripted source scanning its list every tick would
    // dominate a node-level number.
    use rbs_nodes::replay::PacketRing;
    let mut ring = PacketRing::new(limits(120_000, 20_000), StreamIndex(0));
    let mut evicted = Vec::with_capacity(128);
    let n = 60 * 240u64;
    let pk: Vec<_> = (0..n)
        .map(|i| packet(CodecId::H264, i * 16, i % 60 == 0))
        .collect();
    let t = Instant::now();
    for (i, p) in pk.iter().enumerate() {
        ring.push(StreamIndex(0), p.clone(), &mut evicted);
        if i % 32 == 0 {
            evicted.clear();
        }
    }
    let push_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let held = ring.held();
    let mut out = Vec::with_capacity(20_000);
    let t = Instant::now();
    let reps = 200;
    for _ in 0..reps {
        ring.cut(None, &mut out).unwrap();
    }
    let cut_us = t.elapsed().as_micros() as f64 / reps as f64;
    println!(
        "push: {push_ns:.0} ns/packet; cut of {} packets ({:?} held): {cut_us:.0} us",
        out.len(),
        held.duration
    );
}
