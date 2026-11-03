//! Cross-thread cost of the engine loop (ADR-0027): a source on the render worker, an
//! encoder on its own worker and a packet output on a doorbell worker, all on real
//! threads and the production clock. Reports latency per packet (mean, p50, p99,
//! max):
//!
//! - `doorbell_wake`: from the encoder's `process` to the output's `process` (packet
//!   queue, doorbell ring, unpark, output tick).
//! - `render_to_output`: from the render tick's cadence time to the output's `process`
//!   (includes the encoder's wait for its own cadence tick: about one frame without an
//!   offset, the offset with one).
//!
//! Run: `cargo bench -p rbs-engine --bench cross_thread`. Latency is a distribution,
//! not a per-iteration time, so this bench has its own small harness.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rbs_core::time::{Clock, MonotonicClock};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Engine, EngineConfig, Pacing};
use rbs_graph::description::{GraphDescription, PartitionId};
use support::{Probe, Role, TestNode, add};

const ENCODER: PartitionId = PartitionId(1);
const OUTPUT: PartitionId = PartitionId(2);
const SAMPLES: usize = 4096;

fn pipeline(rate: FrameRate, offset: Duration, stamp_at_encoder: bool) -> (Engine, Arc<Probe>) {
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let mut engine = Engine::with_clock(Arc::clone(&clock), EngineConfig::default());
    let frames = Cadence::frames(clock.now(), rate);
    engine
        .add_partition::<()>(
            PartitionId::RENDER,
            Arc::new(()),
            (),
            Pacing::cadence(frames),
        )
        .unwrap();
    engine
        .add_partition::<()>(
            ENCODER,
            Arc::new(()),
            (),
            Pacing::cadence_after(frames, offset),
        )
        .unwrap();
    engine
        .add_partition::<()>(OUTPUT, Arc::new(()), (), Pacing::output())
        .unwrap();
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    let mut encoder = TestNode::new(Role::Encoder, "encoder");
    if stamp_at_encoder {
        encoder = encoder.timed(Arc::clone(&clock));
    }
    let sink = TestNode::new(Role::PacketSink, "sink")
        .with_probe(Probe::with_samples(SAMPLES))
        .timed(Arc::clone(&clock));
    let probe = Arc::clone(&sink.probe);
    let s = add(&mut graph, &source, PartitionId::RENDER);
    let e = add(&mut graph, &encoder, ENCODER);
    let k = add(&mut graph, &sink, OUTPUT);
    graph.connect(s, "out", e, "in").unwrap();
    graph.connect(e, "out", k, "in").unwrap();
    engine
        .adopt::<()>(PartitionId::RENDER, s, Box::new(source))
        .unwrap();
    engine.adopt::<()>(ENCODER, e, Box::new(encoder)).unwrap();
    engine.adopt::<()>(OUTPUT, k, Box::new(sink)).unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    (engine, probe)
}

fn run(name: &str, rate: FrameRate, offset: Duration, stamp_at_encoder: bool, packets: u64) {
    let (mut engine, probe) = pipeline(rate, offset, stamp_at_encoder);
    // Warm-up: the first packets include thread start-up.
    let warm = 50;
    let deadline = Instant::now() + Duration::from_secs(300);
    while probe.seen.load(Ordering::Relaxed) < warm && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (seen0, sum0) = (
        probe.seen.load(Ordering::Relaxed),
        probe.latency_ns.load(Ordering::Relaxed),
    );
    while probe.seen.load(Ordering::Relaxed) < seen0 + packets && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let seen = probe.seen.load(Ordering::Relaxed);
    let got = seen - seen0;
    let mean = (probe.latency_ns.load(Ordering::Relaxed) - sum0) / got.max(1);
    let n = (got as usize).min(SAMPLES);
    let mut samples: Vec<u64> = (0..n)
        .map(|k| probe.samples[(seen as usize - 1 - k) % SAMPLES].load(Ordering::Relaxed))
        .collect();
    samples.sort_unstable();
    let pct = |p: f64| samples[((samples.len() as f64 - 1.0) * p) as usize];
    let stats: Vec<_> = [PartitionId::RENDER, ENCODER, OUTPUT]
        .iter()
        .map(|p| engine.worker_stats(*p).unwrap())
        .collect();
    let report = engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    println!(
        "cross_thread/{name}: packets {got}, mean {:.1} us, p50 {:.1} us, p99 {:.1} us, max {:.1} us; ticks render {} encoder {} output {}; late render {} encoder {}",
        mean as f64 / 1e3,
        pct(0.5) as f64 / 1e3,
        pct(0.99) as f64 / 1e3,
        pct(1.0) as f64 / 1e3,
        stats[0].ticks(),
        stats[1].ticks(),
        stats[2].ticks(),
        stats[0].late(),
        stats[1].late(),
    );
}

fn main() {
    // `cargo test --benches` passes `--bench`-less args; keep it quick there.
    let quick = !std::env::args().any(|a| a == "--bench");
    let (fast, slow) = if quick { (200, 30) } else { (4000, 600) };
    let quarter = FrameRate::FPS_60.frame_duration() / 4;
    run(
        "doorbell_wake_1khz",
        FrameRate::new(1000, 1).unwrap(),
        Duration::ZERO,
        true,
        fast,
    );
    run(
        "render_to_output_60fps_no_offset",
        FrameRate::FPS_60,
        Duration::ZERO,
        false,
        slow,
    );
    run(
        "render_to_output_60fps_quarter_offset",
        FrameRate::FPS_60,
        quarter,
        false,
        slow,
    );
}
