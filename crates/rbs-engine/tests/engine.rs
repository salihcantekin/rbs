//! The engine loop (ADR-0027): workers per partition, doorbell and cadence pacing,
//! restarts through the control loop, moves in two phases and shutdown order.
//!
//! Time is a `TestClock`; tests move it and wake the workers. They wait for other
//! threads with `eventually` (yielding with a generous real-time limit), never for a
//! duration, and wait for a tick by its index ([`support::Ticks`]), not by a counter a
//! worker bumps after the tick. A node that must outlast an engine timeout is held by a
//! gate the test opens once the engine gave up (`WorkerStats::stops`), so no outcome
//! depends on how fast threads are scheduled (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::thread::{
    ThreadClass, ThreadGrant, ThreadGuard, ThreadPolicy, ThreadPolicyError, ThreadRequest,
};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Engine, EngineConfig, EngineError, EngineEvent, EngineHandle, Pacing};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::runtime::{PER_TICK_EVENTS, RestartPolicy, Runtime, RuntimeEvent};
use support::{
    Gate, Probe, ReleaseLog, Role, TestNode, TickLog, Ticks, add, eventually, open_when,
};

const RENDER: PartitionId = PartitionId::RENDER;
const ENCODER: PartitionId = PartitionId(7);
const OUTPUT: PartitionId = PartitionId(3);
const LIMIT: Duration = Duration::from_secs(10);
const FRAME: Duration = Duration::from_nanos(16_666_667);
/// Swap timeout for tests that expect a timeout. The side that must time out is held by
/// a gate, so the timeout always fires; the setup steps that must not time out get at
/// least the 250 ms hang-guard floor of ADR-0032 rule 4.
const EXPECTED_TIMEOUT: Duration = Duration::from_millis(250);

/// Another runtime, to check that partitions keep their runtime types apart.
struct Other;
impl Runtime for Other {
    type Shared = ();
    type Local = ();
}

fn frames() -> Cadence {
    Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60)
}

fn cadence() -> Pacing {
    Pacing::cadence(frames())
}

/// The encoder partition ticks half a frame after render, so it always encodes the
/// frame render wrote before it (the order of the two threads never decides).
const ENCODER_OFFSET: Duration = Duration::from_nanos(8_333_333);

/// The cadence index due at `now` with `offset`, if any.
fn due_index(now: Timestamp, offset: Duration) -> Option<u64> {
    let since = now.as_nanos().checked_sub(offset.as_nanos() as u64)?;
    frames().index_at_or_before(Timestamp::from_nanos(since))
}

/// Advances `clock` by one frame in two halves: the encoder's tick, then render's.
/// Waits for each tick by index in each partition in `parts` that has a worker.
fn step_frame(engine: &Engine, clock: &TestClock, logs: &[(PartitionId, Duration, &TickLog)]) {
    for half in [ENCODER_OFFSET, FRAME - ENCODER_OFFSET] {
        clock.advance(half);
        let now = clock.now();
        for (p, offset, log) in logs {
            if !engine.has_worker(*p) {
                continue;
            }
            let Some(index) = due_index(now, *offset) else {
                continue;
            };
            engine.wake(*p);
            assert!(
                eventually(LIMIT, || log.next() > index),
                "{p} ran tick {index}"
            );
        }
    }
}

/// source (render) -> encoder (partition 7) -> packet sink (partition 3). The output
/// timer is an hour, so the output worker only ticks when its doorbell rings. Every
/// partition runs [`Ticks`], so a test can wait for a tick by its index.
struct Rig {
    clock: TestClock,
    logs: [Arc<TickLog>; 3],
    engine: Engine,
    graph: GraphDescription,
    source: NodeId,
    encoder: NodeId,
    sink: NodeId,
    sink_probe: Arc<Probe>,
    encoder_probe: Arc<Probe>,
    source_probe: Arc<Probe>,
}

impl Rig {
    fn new(log: Option<&ReleaseLog>) -> Self {
        Self::build(log, None, None, Duration::from_secs(10))
    }

    /// `gate`: the encoder's and the sink's `cool` wait for it. `stall`: the sink's
    /// `process` waits for it. `swap_timeout`: the engine's.
    fn build(
        log: Option<&ReleaseLog>,
        gate: Option<&Arc<Gate>>,
        stall: Option<&Arc<Gate>>,
        swap_timeout: Duration,
    ) -> Self {
        let clock = TestClock::new();
        let mut engine = Engine::with_clock(
            Arc::new(clock.clone()),
            EngineConfig::default().swap_timeout(swap_timeout),
        );
        let logs: [Arc<TickLog>; 3] = Default::default();
        engine
            .add_partition::<Ticks>(RENDER, Arc::clone(&logs[0]), (), cadence())
            .unwrap();
        engine
            .add_partition::<Ticks>(
                ENCODER,
                Arc::clone(&logs[1]),
                (),
                Pacing::cadence_after(frames(), ENCODER_OFFSET),
            )
            .unwrap();
        engine
            .add_partition::<Ticks>(
                OUTPUT,
                Arc::clone(&logs[2]),
                (),
                Pacing::doorbell(Duration::from_secs(3600)),
            )
            .unwrap();
        let mut graph = GraphDescription::new();
        let named = |role, name| {
            let mut node = TestNode::new(role, name);
            if let Some(log) = log {
                node = node.logging(log);
            }
            if let (Some(gate), Role::Encoder | Role::PacketSink) = (gate, role) {
                node = node.cooling_on(gate);
            }
            if let (Some(stall), Role::PacketSink) = (stall, role) {
                node = node.processing_on(stall);
            }
            node
        };
        let source = named(Role::Source, "source");
        let encoder = named(Role::Encoder, "encoder");
        let sink = named(Role::PacketSink, "sink");
        let (sink_probe, encoder_probe) = (Arc::clone(&sink.probe), Arc::clone(&encoder.probe));
        let source_probe = Arc::clone(&source.probe);
        let ids = (
            add(&mut graph, &source, RENDER),
            add(&mut graph, &encoder, ENCODER),
            add(&mut graph, &sink, OUTPUT),
        );
        graph.connect(ids.0, "out", ids.1, "in").unwrap();
        graph.connect(ids.1, "out", ids.2, "in").unwrap();
        engine
            .adopt::<Ticks>(RENDER, ids.0, Box::new(source))
            .unwrap();
        engine
            .adopt::<Ticks>(ENCODER, ids.1, Box::new(encoder))
            .unwrap();
        engine
            .adopt::<Ticks>(OUTPUT, ids.2, Box::new(sink))
            .unwrap();
        Self {
            clock,
            logs,
            engine,
            graph,
            source: ids.0,
            encoder: ids.1,
            sink: ids.2,
            sink_probe,
            encoder_probe,
            source_probe,
        }
    }

    /// Moves the clock one frame and waits until the cadence workers ran their ticks
    /// of it: the encoder's (half a frame in), then render's.
    fn frame(&mut self) {
        step_frame(
            &self.engine,
            &self.clock,
            &[
                (RENDER, Duration::ZERO, &self.logs[0]),
                (ENCODER, ENCODER_OFFSET, &self.logs[1]),
            ],
        );
    }

    fn seen(&self) -> u64 {
        self.sink_probe.seen.load(Ordering::Relaxed)
    }
}

#[test]
fn an_engine_that_runs_nothing_has_no_threads() {
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock), EngineConfig::default());
    engine
        .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
        .unwrap();
    engine
        .add_partition::<()>(OUTPUT, Arc::new(()), (), Pacing::output())
        .unwrap();
    assert_eq!(engine.workers(), 0);

    // A graph whose nodes are all Standby schedules nothing: still no thread, and the
    // control loop may sleep until the next edit.
    let mut graph = GraphDescription::new();
    let node = TestNode::new(Role::VideoSink, "sink");
    let id = graph.add_node(node.descriptor_arc()).unwrap();
    graph
        .set_state(id, rbs_core::lifecycle::Lifecycle::Standby)
        .unwrap();
    engine.adopt::<()>(RENDER, id, Box::new(node)).unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert_eq!(engine.workers(), 0);
    assert_eq!(engine.service().wait, None);

    // A handle that was never used starts no control thread either.
    let handle = EngineHandle::new(Engine::new(), Box::new(|_| {}));
    assert!(!handle.is_started());
}

#[test]
fn frames_and_packets_cross_three_worker_threads() {
    let mut rig = Rig::new(None);
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert_eq!(applied.new_queues.len(), 2);
    assert_eq!(rig.engine.workers(), 3);
    for _ in 0..20 {
        rig.frame();
    }
    // The encoder ticks half a frame after render, so every frame from the second on
    // gets through.
    assert!(
        eventually(LIMIT, || rig.seen() >= 10),
        "seen {}",
        rig.seen()
    );
    // The output's timer is an hour: everything it saw came through its doorbell.
    let packets = &rig.engine.crossings()[1].stats;
    assert_eq!(packets.overflows(), 0);
    assert!(eventually(LIMIT, || rig.seen() == packets.sent()));
    assert_eq!(packets.received(), packets.sent());
    let output = rig.engine.worker_stats(OUTPUT).unwrap();
    assert!(output.ticks() >= 1);
    // A ring can cost two wake-ups (the flag and the thread's park token), never more;
    // with an hour-long timer every tick came from a ring.
    assert!(
        output.ticks() <= 2 * packets.sent() + 2,
        "woken by rings only"
    );
}

#[test]
fn an_apply_starts_consumers_before_the_partitions_feeding_them() {
    // A slow device open in a downstream partition (a cold microphone, an encoder
    // session) must not leave render exporting into a queue nobody takes yet.
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock), EngineConfig::default());
    let logs: [Arc<TickLog>; 3] = Default::default();
    engine
        .add_partition::<Ticks>(RENDER, Arc::clone(&logs[0]), (), cadence())
        .unwrap();
    engine
        .add_partition::<Ticks>(
            ENCODER,
            Arc::clone(&logs[1]),
            (),
            Pacing::cadence_after(frames(), ENCODER_OFFSET),
        )
        .unwrap();
    engine
        .add_partition::<Ticks>(OUTPUT, Arc::clone(&logs[2]), (), Pacing::output())
        .unwrap();
    let stats: Vec<_> = [RENDER, ENCODER, OUTPUT]
        .iter()
        .map(|p| engine.worker_stats(*p).unwrap())
        .collect();
    // Worker starts seen by each prepare, all partitions summed.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let watcher = |name: &'static str| {
        let (stats, seen) = (stats.clone(), Arc::clone(&seen));
        move || {
            let starts: u64 = stats.iter().map(|s| s.starts()).sum();
            seen.lock().unwrap().push((name, starts));
        }
    };
    let source = TestNode::new(Role::Source, "source").preparing_with(watcher("source"));
    let encoder = TestNode::new(Role::Encoder, "encoder").preparing_with(watcher("encoder"));
    let sink = TestNode::new(Role::PacketSink, "sink").preparing_with(watcher("sink"));
    let mut graph = GraphDescription::new();
    let ids = (
        add(&mut graph, &source, RENDER),
        add(&mut graph, &encoder, ENCODER),
        add(&mut graph, &sink, OUTPUT),
    );
    graph.connect(ids.0, "out", ids.1, "in").unwrap();
    graph.connect(ids.1, "out", ids.2, "in").unwrap();
    engine
        .adopt::<Ticks>(RENDER, ids.0, Box::new(source))
        .unwrap();
    engine
        .adopt::<Ticks>(ENCODER, ids.1, Box::new(encoder))
        .unwrap();
    engine
        .adopt::<Ticks>(OUTPUT, ids.2, Box::new(sink))
        .unwrap();

    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    // Consumers first: each partition prepared while every partition it feeds already
    // ran its new schedule, and none that feeds it did.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("sink", 0), ("encoder", 1), ("source", 2)]
    );
    // Every partition runs its new schedule once `apply` returns (invariant 4).
    assert_eq!(engine.workers(), 3);
    let _ = engine.shutdown();
}

#[test]
fn a_branch_added_to_a_running_render_starts_downstream_first() {
    // The studio case: render already runs (preview); a recording adds an export in
    // render and an encoder and an output. Render's worker swaps its new schedule as
    // soon as it is sent, so it must be sent last.
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock), EngineConfig::default());
    let logs: [Arc<TickLog>; 3] = Default::default();
    engine
        .add_partition::<Ticks>(RENDER, Arc::clone(&logs[0]), (), cadence())
        .unwrap();
    engine
        .add_partition::<Ticks>(
            ENCODER,
            Arc::clone(&logs[1]),
            (),
            Pacing::cadence_after(frames(), ENCODER_OFFSET),
        )
        .unwrap();
    engine
        .add_partition::<Ticks>(OUTPUT, Arc::clone(&logs[2]), (), Pacing::output())
        .unwrap();
    let mut graph = GraphDescription::new();
    let canvas = TestNode::new(Role::Source, "canvas");
    let preview = TestNode::new(Role::VideoSink, "preview");
    let canvas_id = add(&mut graph, &canvas, RENDER);
    let preview_id = add(&mut graph, &preview, RENDER);
    graph.connect(canvas_id, "out", preview_id, "in").unwrap();
    engine
        .adopt::<Ticks>(RENDER, canvas_id, Box::new(canvas))
        .unwrap();
    engine
        .adopt::<Ticks>(RENDER, preview_id, Box::new(preview))
        .unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert_eq!(engine.workers(), 1);

    let downstream: Vec<_> = [ENCODER, OUTPUT]
        .iter()
        .map(|p| engine.worker_stats(*p).unwrap())
        .collect();
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let hook = {
        let seen = Arc::clone(&seen);
        move || {
            let starts: u64 = downstream.iter().map(|s| s.starts()).sum();
            seen.store(starts, Ordering::Relaxed);
        }
    };
    let export = TestNode::new(Role::Source, "export").preparing_with(hook);
    let encoder = TestNode::new(Role::Encoder, "encoder");
    let sink = TestNode::new(Role::PacketSink, "sink");
    let ids = (
        add(&mut graph, &export, RENDER),
        add(&mut graph, &encoder, ENCODER),
        add(&mut graph, &sink, OUTPUT),
    );
    graph.connect(ids.0, "out", ids.1, "in").unwrap();
    graph.connect(ids.1, "out", ids.2, "in").unwrap();
    engine
        .adopt::<Ticks>(RENDER, ids.0, Box::new(export))
        .unwrap();
    engine
        .adopt::<Ticks>(ENCODER, ids.1, Box::new(encoder))
        .unwrap();
    engine
        .adopt::<Ticks>(OUTPUT, ids.2, Box::new(sink))
        .unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    // When render prepared its part (and before its worker could swap it in), the
    // encoder's and the output's workers ran already.
    assert_eq!(seen.load(Ordering::Relaxed), 2);
    assert_eq!(engine.workers(), 3);
    let _ = engine.shutdown();
}

#[test]
fn a_late_cadence_worker_runs_the_newest_tick_and_counts_the_skipped_ones() {
    let mut rig = Rig::new(None);
    rig.engine.apply(&rig.graph).unwrap();
    rig.frame();
    let stats = rig.engine.worker_stats(RENDER).unwrap();
    let (ticks, late) = (stats.ticks(), stats.late());
    // Five frames pass at once.
    rig.clock.advance(FRAME * 5);
    rig.engine.wake(RENDER);
    assert!(eventually(LIMIT, || stats.ticks() > ticks));
    assert_eq!(stats.ticks(), ticks + 1);
    assert_eq!(stats.late(), late + 4);
}

#[test]
fn an_encoder_moves_to_the_output_partition_in_two_phases() {
    let mut rig = Rig::new(None);
    rig.engine.apply(&rig.graph).unwrap();
    for _ in 0..3 {
        rig.frame();
    }
    rig.graph.set_partition(rig.encoder, OUTPUT).unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert_eq!(applied.moved, vec![(rig.encoder, ENCODER, OUTPUT)]);
    for report in applied.reports.values() {
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }
    // The encoder partition runs nothing now, so its worker stopped (ADR-0013).
    assert!(!rig.engine.has_worker(ENCODER));
    assert_eq!(rig.engine.crossings().len(), 1);

    // Output now runs the encoder: video reaches it through a mailbox that does not
    // ring, so the output is woken here like a timer would.
    let before = rig.encoder_probe.seen.load(Ordering::Relaxed);
    for _ in 0..5 {
        rig.frame();
        let stats = rig.engine.worker_stats(OUTPUT).unwrap();
        let t = stats.ticks();
        rig.engine.wake(OUTPUT);
        assert!(eventually(LIMIT, || stats.ticks() > t));
    }
    assert!(rig.encoder_probe.seen.load(Ordering::Relaxed) >= before + 4);
    assert!(rig.seen() >= before + 4);
}

#[test]
fn a_bypassed_node_restarts_through_the_control_loop() {
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(
        Arc::new(clock.clone()),
        EngineConfig::default().swap_timeout(LIMIT),
    );
    engine
        .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
        .unwrap();
    engine
        .set_restart_policy(
            RENDER,
            RestartPolicy::backoff(Duration::from_secs(1), Duration::from_secs(1)),
        )
        .unwrap();
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    // The first instance fails every tick, so the executor bypasses it.
    let sink = TestNode::new(Role::VideoSink, "sink").failing(1);
    let sink_probe = Arc::clone(&sink.probe);
    let s = add(&mut graph, &source, RENDER);
    let k = add(&mut graph, &sink, RENDER);
    graph.connect(s, "out", k, "in").unwrap();
    engine.adopt::<()>(RENDER, s, Box::new(source)).unwrap();
    engine.adopt::<()>(RENDER, k, Box::new(sink)).unwrap();
    engine.apply(&graph).unwrap();

    let stats = engine.worker_stats(RENDER).unwrap();
    let mut events: Vec<RuntimeEvent> = Vec::new();
    let step = |engine: &mut Engine, events: &mut Vec<RuntimeEvent>| {
        let t = stats.ticks();
        clock.advance(FRAME);
        engine.wake(RENDER);
        assert!(eventually(LIMIT, || stats.ticks() > t));
        let serviced = engine.service();
        assert!(serviced.errors.is_empty(), "{:?}", serviced.errors);
        assert!(
            serviced.wait.is_some(),
            "a worker runs, so the loop keeps looking"
        );
        events.extend(serviced.events.into_iter().map(|(_, e)| e));
    };
    // Eight failed ticks bypass the node; the control loop sees it and takes it out.
    for _ in 0..12 {
        step(&mut engine, &mut events);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Failed { node, .. } if *node == k)),
        "{events:?}"
    );
    assert!(engine.fault(k).is_some());
    // After the one-second back-off it is prepared again.
    clock.advance(Duration::from_secs(1));
    for _ in 0..3 {
        step(&mut engine, &mut events);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Restarted { node, .. } if *node == k)),
        "{events:?}"
    );
    assert_eq!(sink_probe.prepares.load(Ordering::Relaxed), 2);
    let seen = sink_probe.seen.load(Ordering::Relaxed);
    step(&mut engine, &mut events);
    assert!(sink_probe.seen.load(Ordering::Relaxed) > seen, "runs again");
}

#[test]
fn shutdown_drains_in_pipeline_order_and_stops_every_worker() {
    let log: ReleaseLog = Arc::new(Mutex::new(Vec::new()));
    let mut rig = Rig::new(Some(&log));
    rig.engine.apply(&rig.graph).unwrap();
    for _ in 0..3 {
        rig.frame();
    }
    // Partition ids are not in pipeline order (render 0, output 3, encoder 7).
    assert_eq!(rig.engine.pipeline_order(), vec![RENDER, ENCODER, OUTPUT]);
    let report = rig.engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.order, vec![RENDER, ENCODER, OUTPUT]);
    assert_eq!(*log.lock().unwrap(), vec!["source", "encoder", "sink"]);
    assert_eq!(rig.engine.workers(), 0);
    // Every packet the encoder made before it stopped reached the output.
    let packets = rig.engine.crossings().len();
    assert_eq!(packets, 0, "queues are dropped");
    assert_eq!(rig.seen(), rig.encoder_probe.seen.load(Ordering::Relaxed));
    assert_eq!(
        rig.engine.apply(&rig.graph).unwrap_err(),
        EngineError::ShutDown
    );
    let _ = (rig.source, rig.sink);
}

#[test]
fn shutdown_encodes_the_last_frame_render_wrote() {
    let mut rig = Rig::new(None);
    rig.engine.apply(&rig.graph).unwrap();
    for _ in 0..3 {
        rig.frame();
    }
    let ran = rig.source_probe.last_frame_ns.load(Ordering::Relaxed);
    for probe in [&rig.source_probe, &rig.encoder_probe, &rig.sink_probe] {
        assert_eq!(probe.draining_ticks.load(Ordering::Relaxed), 0, "running");
    }
    let report = rig.engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // Final ticks are marked: exactly one on each cadence partition, at least one on
    // the doorbell-paced output (ADR-0043).
    assert_eq!(rig.source_probe.draining_ticks.load(Ordering::Relaxed), 1);
    assert_eq!(rig.encoder_probe.draining_ticks.load(Ordering::Relaxed), 1);
    assert!(rig.sink_probe.draining_ticks.load(Ordering::Relaxed) >= 1);
    // Render ran a final tick (the next index, at once), the encoder a final tick that
    // encoded its frame, and the output passed the packet on.
    let last = rig.source_probe.last_frame_ns.load(Ordering::Relaxed);
    assert!(last > ran, "render ran a final tick");
    assert_eq!(
        rig.encoder_probe.last_frame_ns.load(Ordering::Relaxed),
        last
    );
    assert_eq!(rig.seen(), rig.encoder_probe.seen.load(Ordering::Relaxed));
    for probe in [&rig.source_probe, &rig.encoder_probe, &rig.sink_probe] {
        assert_eq!(probe.repeated_indices.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn shutdown_passes_every_queued_packet_not_just_one_ticks_worth() {
    // The sink's first `process` stalls (a slow output) for a while, so packets pile up
    // in the queue: far more than one tick passes (PER_TICK_EVENTS). Shutdown starts
    // while it is stalled, normally; the output's drain then waits for it and passes
    // everything queued. (If the stall ends early the output passes them anyway.)
    let stall = Arc::new(Gate::holding(LIMIT));
    let mut rig = Rig::build(None, None, Some(&stall), LIMIT);
    rig.engine.apply(&rig.graph).unwrap();
    let backlog = 4 * PER_TICK_EVENTS as u64;
    // The encoder ticks after render in every frame, so it encodes one frame per frame
    // from the second on.
    for _ in 0..backlog + 2 {
        rig.frame();
    }
    assert!(rig.encoder_probe.seen.load(Ordering::Relaxed) > backlog);
    assert!(rig.seen() < PER_TICK_EVENTS as u64, "the sink is stalled");
    // The stall ends once shutdown ran render's final tick: shutdown has started, and
    // the output drains only after render and the encoder did.
    let source_ticks = Arc::clone(&rig.source_probe);
    let before = source_ticks.ticks.load(Ordering::Relaxed);
    let opener = open_when(&stall, LIMIT, move || {
        source_ticks.ticks.load(Ordering::Relaxed) > before
    });
    let report = rig.engine.shutdown();
    assert!(opener.join().unwrap(), "render ran its final tick");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(rig.seen(), rig.encoder_probe.seen.load(Ordering::Relaxed));
}

/// Takes `id` out of the graph (Standby first: an Active node cannot be removed).
fn remove(graph: &mut GraphDescription, id: NodeId) {
    graph.set_state(id, Lifecycle::Standby).unwrap();
    graph.remove_node(id).unwrap();
}

#[test]
fn a_branch_removed_with_apply_draining_drains_while_render_keeps_running() {
    // ADR-0041: a recording's encoder and output leave a running engine. A preview
    // (a video sink on render) keeps the source scheduled.
    let log: ReleaseLog = Arc::new(Mutex::new(Vec::new()));
    let mut rig = Rig::new(Some(&log));
    let preview = TestNode::new(Role::VideoSink, "preview");
    let preview_probe = Arc::clone(&preview.probe);
    let preview_id = add(&mut rig.graph, &preview, RENDER);
    rig.graph
        .connect(rig.source, "out", preview_id, "in")
        .unwrap();
    rig.engine
        .adopt::<Ticks>(RENDER, preview_id, Box::new(preview))
        .unwrap();
    let full = rig.graph.clone();
    let setup = rig.engine.apply(&full).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    for _ in 0..4 {
        rig.frame();
    }
    let encoded = rig.encoder_probe.seen.load(Ordering::Relaxed);
    assert!(encoded > 0);

    let mut without = full.clone();
    remove(&mut without, rig.encoder);
    remove(&mut without, rig.sink);
    let applied = rig.engine.apply_draining(&without).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    // The encoder ran its final tick before the sink drained: every packet it made
    // reached the sink, and both were released, the encoder first.
    assert_eq!(*log.lock().unwrap(), vec!["encoder", "sink"]);
    assert_eq!(rig.seen(), rig.encoder_probe.seen.load(Ordering::Relaxed));
    assert!(rig.seen() >= encoded);
    // Only the drained partitions' final ticks are marked; render keeps running.
    assert_eq!(rig.encoder_probe.draining_ticks.load(Ordering::Relaxed), 1);
    assert!(rig.sink_probe.draining_ticks.load(Ordering::Relaxed) >= 1);
    assert_eq!(preview_probe.draining_ticks.load(Ordering::Relaxed), 0);
    assert_eq!(rig.source_probe.draining_ticks.load(Ordering::Relaxed), 0);
    assert!(!rig.engine.has_worker(ENCODER));
    assert!(!rig.engine.has_worker(OUTPUT));
    assert!(rig.engine.crossings().is_empty());

    // Render never stopped: the preview keeps getting frames.
    assert!(rig.engine.has_worker(RENDER));
    let shown = preview_probe.seen.load(Ordering::Relaxed);
    rig.frame();
    rig.frame();
    assert!(preview_probe.seen.load(Ordering::Relaxed) > shown);
    assert_eq!(*log.lock().unwrap(), vec!["encoder", "sink"]);

    // The drained partitions take a branch again: their workers start and run.
    let applied = rig.engine.apply(&full).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    let before = rig.seen();
    for _ in 0..4 {
        rig.frame();
    }
    assert!(rig.engine.has_worker(ENCODER));
    // The output's worker runs on its doorbell, not on the frames the test steps:
    // wait for it to pass a packet instead of assuming it already did.
    assert!(
        eventually(LIMIT, || rig.seen() > before),
        "the output runs again"
    );
    let report = rig.engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn apply_without_draining_runs_no_final_tick() {
    let mut rig = Rig::new(None);
    rig.engine.apply(&rig.graph).unwrap();
    for _ in 0..3 {
        rig.frame();
    }
    let ticks = rig.encoder_probe.ticks.load(Ordering::Relaxed);
    let mut without = rig.graph.clone();
    remove(&mut without, rig.encoder);
    remove(&mut without, rig.sink);
    remove(&mut without, rig.source);
    let applied = rig.engine.apply(&without).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    assert_eq!(rig.encoder_probe.ticks.load(Ordering::Relaxed), ticks);
    assert_eq!(rig.engine.workers(), 0);
}

#[test]
fn a_drain_that_times_out_is_reported_and_still_releases_everything() {
    // The sink's first `process` stalls far longer than the swap timeout, and shutdown
    // starts while it does: the output's final ticks miss the timeout.
    let log: ReleaseLog = Arc::new(Mutex::new(Vec::new()));
    let stall = Arc::new(Gate::holding(LIMIT));
    let mut rig = Rig::build(Some(&log), None, Some(&stall), EXPECTED_TIMEOUT);
    let setup = rig.engine.apply(&rig.graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    // The encoder ticks after render (half a frame later), so it reads a frame from the
    // second frame on; the output ticks on its first packet.
    for _ in 0..20 {
        rig.frame();
        if rig.encoder_probe.seen.load(Ordering::Relaxed) > 0 {
            break;
        }
    }
    let output = rig.engine.worker_stats(OUTPUT).unwrap();
    // The sink is inside its stalled `process` once the output ticked.
    assert!(eventually(LIMIT, || rig
        .sink_probe
        .ticks
        .load(Ordering::Relaxed)
        == 1));
    // The stall ends only once the engine gave up on the output's final ticks and
    // stops its worker, so the drain times out however the threads are scheduled.
    let stops = rig.engine.worker_stats(OUTPUT).unwrap();
    let opener = open_when(&stall, LIMIT, move || stops.stops() > 0);
    let report = rig.engine.shutdown();
    assert!(opener.join().unwrap(), "the output worker was stopped");
    assert!(
        report.errors.contains(&EngineError::DrainTimeout(OUTPUT)),
        "{:?}",
        report.errors
    );
    // A starved machine may also miss the timeout elsewhere; that is reported too.
    assert!(
        report.errors.iter().all(|e| matches!(
            e,
            EngineError::DrainTimeout(_) | EngineError::SwapTimeout(_)
        )),
        "{:?}",
        report.errors
    );
    assert_eq!(rig.engine.workers(), 0);
    assert_eq!(*log.lock().unwrap(), vec!["source", "encoder", "sink"]);
    assert!(output.ticks() >= 1);
}

#[test]
fn a_doorbell_worker_ticks_on_its_timer_without_a_ring() {
    // A partition with no packet queue: nothing ever rings its doorbell.
    let mut engine = Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default());
    engine
        .add_partition::<()>(
            OUTPUT,
            Arc::new(()),
            (),
            Pacing::doorbell(Duration::from_millis(1)),
        )
        .unwrap();
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    let sink = TestNode::new(Role::VideoSink, "sink");
    let probe = Arc::clone(&sink.probe);
    let ids = (
        add(&mut graph, &source, OUTPUT),
        add(&mut graph, &sink, OUTPUT),
    );
    graph.connect(ids.0, "out", ids.1, "in").unwrap();
    engine.adopt::<()>(OUTPUT, ids.0, Box::new(source)).unwrap();
    engine.adopt::<()>(OUTPUT, ids.1, Box::new(sink)).unwrap();
    engine.apply(&graph).unwrap();
    assert!(engine.crossings().is_empty());
    // The apply's wake-up rings once; every further tick is the timer's.
    let stats = engine.worker_stats(OUTPUT).unwrap();
    assert!(
        eventually(LIMIT, || stats.ticks() >= 5),
        "{}",
        stats.ticks()
    );
    assert!(probe.ticks.load(Ordering::Relaxed) >= 4);
}

#[test]
fn tick_indices_never_repeat_across_worker_restarts() {
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default());
    let log = Arc::new(TickLog::default());
    engine
        .add_partition::<Ticks>(RENDER, Arc::clone(&log), (), cadence())
        .unwrap();
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    let sink = TestNode::new(Role::VideoSink, "sink");
    let probe = Arc::clone(&sink.probe);
    let ids = (
        add(&mut graph, &source, RENDER),
        add(&mut graph, &sink, RENDER),
    );
    graph.connect(ids.0, "out", ids.1, "in").unwrap();
    engine
        .adopt::<Ticks>(RENDER, ids.0, Box::new(source))
        .unwrap();
    engine
        .adopt::<Ticks>(RENDER, ids.1, Box::new(sink))
        .unwrap();
    let stats = engine.worker_stats(RENDER).unwrap();
    // Waits for each tick by its index: `stats.ticks()` is bumped after a tick, so a
    // count read right after `apply` may still miss the tick that swapped, and the next
    // frame would then be skipped as late.
    let frames = |engine: &Engine, n: u64| {
        for _ in 0..n {
            clock.advance(FRAME);
            let index = frames().index_at_or_before(clock.now()).unwrap();
            engine.wake(RENDER);
            assert!(eventually(LIMIT, || log.next() > index), "tick {index}");
        }
    };
    for round in 1..=3 {
        let applied = engine.apply(&graph).unwrap();
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert!(engine.has_worker(RENDER));
        assert_eq!(stats.starts(), round);
        frames(&engine, 3);
        // The worker's next index is the runtime's: the tick it ran last, plus one.
        // `stats` counts a tick after it ran and the log during it, so wait for the
        // count instead of reading it once.
        assert!(
            eventually(LIMIT, || stats.next_index() == log.next()),
            "next index {} after the tick logged {}",
            stats.next_index(),
            log.next()
        );
        // Standby: the schedule is empty and the worker stops. The clock does not
        // move, so the restarted worker finds the tick it last ran still current.
        graph.set_state(ids.1, Lifecycle::Standby).unwrap();
        engine.apply(&graph).unwrap();
        assert!(!engine.has_worker(RENDER));
        assert_eq!(stats.next_index(), log.next());
        graph.set_state(ids.1, Lifecycle::Active).unwrap();
    }
    assert_eq!(probe.ticks.load(Ordering::Relaxed), 9);
    assert_eq!(probe.repeated_indices.load(Ordering::Relaxed), 0);
    assert_eq!(stats.late(), 0, "no frame was skipped");
}

#[test]
fn partitions_keep_their_runtime_types_apart() {
    let mut engine = Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default());
    engine
        .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
        .unwrap();
    engine
        .add_partition::<Other>(OUTPUT, Arc::new(()), (), Pacing::output())
        .unwrap();
    assert_eq!(
        engine.add_partition::<()>(RENDER, Arc::new(()), (), cadence()),
        Err(EngineError::DuplicatePartition(RENDER))
    );
    let mut graph = GraphDescription::new();
    let node = TestNode::new(Role::VideoSink, "sink");
    let id = add(&mut graph, &node, OUTPUT);
    assert_eq!(
        engine.adopt::<()>(OUTPUT, id, Box::new(TestNode::new(Role::VideoSink, "x"))),
        Err(EngineError::WrongRuntime(OUTPUT))
    );
    engine.adopt::<Other>(OUTPUT, id, Box::new(node)).unwrap();

    // Moving the node to a partition of another runtime keeps it where it was.
    engine.apply(&graph).unwrap();
    graph.set_partition(id, RENDER).unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert_eq!(
        applied.errors,
        vec![EngineError::MoveWrongRuntime {
            node: id,
            from: OUTPUT,
            to: RENDER
        }]
    );
    assert!(engine.take::<Other>(OUTPUT, id).unwrap().is_some());

    // Nodes in a partition that was never added: nothing is applied.
    graph.set_partition(id, PartitionId(99)).unwrap();
    assert_eq!(
        engine.apply(&graph).unwrap_err(),
        EngineError::UnknownPartition(PartitionId(99))
    );
}

#[test]
fn a_handle_never_blocks_and_reports_through_its_callback() {
    let (tx, rx) = mpsc::channel::<EngineEvent>();
    let tx = Mutex::new(tx);
    let clock = TestClock::new();
    let engine = Engine::with_clock(
        Arc::new(clock.clone()),
        EngineConfig::default().swap_timeout(LIMIT),
    );
    let handle = EngineHandle::new(
        engine,
        Box::new(move |e| {
            let _ = tx.lock().unwrap().send(e);
        }),
    );
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    let sink = TestNode::new(Role::VideoSink, "sink");
    let probe = Arc::clone(&sink.probe);
    let s = add(&mut graph, &source, RENDER);
    let k = add(&mut graph, &sink, RENDER);
    graph.connect(s, "out", k, "in").unwrap();
    handle
        .run(move |engine| {
            engine
                .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
                .unwrap();
            engine.adopt::<()>(RENDER, s, Box::new(source)).unwrap();
            engine.adopt::<()>(RENDER, k, Box::new(sink)).unwrap();
        })
        .unwrap();
    assert!(handle.is_started());
    handle.apply(graph).unwrap();
    match rx.recv_timeout(LIMIT).unwrap() {
        EngineEvent::Applied(applied) => assert!(applied.errors.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
    clock.advance(FRAME);
    assert!(eventually(LIMIT, || probe.seen.load(Ordering::Relaxed) >= 1));
    handle.shutdown().unwrap();
    let report = loop {
        match rx.recv_timeout(LIMIT).unwrap() {
            EngineEvent::ShutDown(report) => break report,
            _ => continue,
        }
    };
    assert_eq!(report.order, vec![RENDER]);
    assert_eq!(
        handle.apply(GraphDescription::new()),
        Err(EngineError::ShutDown)
    );
    let _ = clock.now();
}

/// Runs the control loop (and frames) until `cond` holds or `LIMIT` real time passed.
/// Bounded by time, not rounds: a round can take microseconds while the workers it
/// waits for are not scheduled for milliseconds on a loaded machine.
fn serve_until(rig: &mut Rig, mut cond: impl FnMut(&mut Rig) -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < LIMIT {
        let serviced = rig.engine.service();
        assert!(
            serviced
                .errors
                .iter()
                .all(|e| matches!(e, EngineError::SwapTimeout(_))),
            "{:?}",
            serviced.errors
        );
        if cond(rig) {
            return true;
        }
        rig.frame();
        std::thread::yield_now();
    }
    false
}

#[test]
fn nodes_swapping_partitions_in_one_edit_both_run() {
    let mut rig = Rig::new(None);
    rig.engine.apply(&rig.graph).unwrap();
    rig.frame();
    // The encoder moves into the output partition and the sink into the encoder's: each
    // partition both loses and gains a node.
    rig.graph.set_partition(rig.encoder, OUTPUT).unwrap();
    rig.graph.set_partition(rig.sink, ENCODER).unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    // The last report of each partition is the one sent after the moves.
    for (p, report) in &applied.reports {
        assert!(report.failed.is_empty(), "{p}: {:?}", report.failed);
    }
    let encoded = rig.encoder_probe.seen.load(Ordering::Relaxed);
    for _ in 0..4 {
        rig.frame();
        // The output partition (doorbell) now runs the encoder; video does not ring,
        // so it is woken here like its timer would.
        let stats = rig.engine.worker_stats(OUTPUT).unwrap();
        let t = stats.ticks();
        rig.engine.wake(OUTPUT);
        assert!(eventually(LIMIT, || stats.ticks() > t));
    }
    assert!(
        rig.encoder_probe.seen.load(Ordering::Relaxed) > encoded,
        "encoder runs"
    );
    // The sink, now on the encoder partition's cadence worker, gets the packets.
    let seen = rig.seen();
    rig.frame();
    assert!(eventually(LIMIT, || rig.seen() > seen), "sink runs");
}

#[test]
fn a_move_held_up_by_a_swap_timeout_finishes_from_the_control_loop() {
    let gate = Arc::new(Gate::default());
    let mut rig = Rig::build(None, Some(&gate), None, EXPECTED_TIMEOUT);
    let setup = rig.engine.apply(&rig.graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    rig.frame();
    // The encoder's `cool` blocks, so its old partition does not swap in time.
    rig.graph.set_partition(rig.encoder, OUTPUT).unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(
        applied.errors.contains(&EngineError::SwapTimeout(ENCODER)),
        "{:?}",
        applied.errors
    );
    assert!(
        !applied
            .errors
            .iter()
            .any(|e| matches!(e, EngineError::MoveNotIdle { .. })),
        "the move waits, it does not fail: {:?}",
        applied.errors
    );
    gate.open();
    let before = rig.encoder_probe.prepares.load(Ordering::Relaxed);
    let encoder_probe = Arc::clone(&rig.encoder_probe);
    assert!(serve_until(&mut rig, |rig| {
        rig.engine.wake(OUTPUT);
        encoder_probe.prepares.load(Ordering::Relaxed) > before
            && encoder_probe.seen.load(Ordering::Relaxed) > 0
            && !rig.engine.has_worker(ENCODER)
    }));
    assert!(
        rig.engine
            .take::<Ticks>(ENCODER, rig.encoder)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_drain_held_up_by_a_swap_timeout_runs_when_the_control_loop_sends_the_part() {
    // A packet sink on the encoder partition stalls in its first `process`, so a swap
    // of that partition stays in flight past the swap timeout.
    let stall = Arc::new(Gate::default());
    let mut rig = Rig::build(None, None, None, EXPECTED_TIMEOUT);
    let staller = TestNode::new(Role::PacketSink, "staller").processing_on(&stall);
    let staller_probe = Arc::clone(&staller.probe);
    let staller_id = add(&mut rig.graph, &staller, ENCODER);
    rig.graph
        .connect(rig.encoder, "out", staller_id, "in")
        .unwrap();
    rig.engine
        .adopt::<Ticks>(ENCODER, staller_id, Box::new(staller))
        .unwrap();
    let setup = rig.engine.apply(&rig.graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    // Encoder tick 0 becomes due; the staller is inside it.
    rig.clock.advance(FRAME);
    rig.engine.wake(ENCODER);
    assert!(eventually(LIMIT, || staller_probe
        .ticks
        .load(Ordering::Relaxed)
        == 1));
    // An edit that keeps the partition's nodes: its swap waits for the stalled tick.
    let extra = TestNode::new(Role::PacketSink, "extra");
    let extra_id = add(&mut rig.graph, &extra, ENCODER);
    rig.graph
        .connect(rig.encoder, "out", extra_id, "in")
        .unwrap();
    rig.engine
        .adopt::<Ticks>(ENCODER, extra_id, Box::new(extra))
        .unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(
        applied.errors.contains(&EngineError::SwapTimeout(ENCODER)),
        "{:?}",
        applied.errors
    );
    // Removing the branch cannot send the partition's empty part now either.
    let mut without = rig.graph.clone();
    for id in [rig.encoder, rig.sink, staller_id, extra_id] {
        remove(&mut without, id);
    }
    let applied = rig.engine.apply_draining(&without).unwrap();
    assert!(
        applied.errors.contains(&EngineError::SwapTimeout(ENCODER)),
        "{:?}",
        applied.errors
    );
    // The clock stands still from here: tick 1 is not due, so only a final tick (run at
    // once, ahead of its time) can follow the stalled tick 0.
    stall.open();
    let start = std::time::Instant::now();
    while rig.engine.has_worker(ENCODER) && start.elapsed() < LIMIT {
        let serviced = rig.engine.service();
        assert!(
            serviced
                .errors
                .iter()
                .all(|e| matches!(e, EngineError::SwapTimeout(_))),
            "{:?}",
            serviced.errors
        );
        std::thread::yield_now();
    }
    assert!(!rig.engine.has_worker(ENCODER));
    assert_eq!(
        rig.logs[1].next(),
        2,
        "the encoder ran its final tick before its empty part was sent"
    );
    let stats = rig.engine.worker_stats(ENCODER).unwrap();
    assert_eq!(
        stats.next_index(),
        2,
        "the final tick counts like any other"
    );
    let report = rig.engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn retired_endpoints_are_dropped_after_a_swap_timeout() {
    let gate = Arc::new(Gate::default());
    let mut rig = Rig::build(None, Some(&gate), None, EXPECTED_TIMEOUT);
    let setup = rig.engine.apply(&rig.graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    rig.frame();
    let packets = rig.engine.crossings()[1].clone();
    // Dropping the encoder -> sink link retires its queue and starves the sink, whose
    // `cool` blocks past the swap timeout.
    let link = *rig
        .graph
        .links()
        .iter()
        .find(|l| l.to.node == rig.sink)
        .unwrap();
    assert!(rig.graph.disconnect(link));
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.contains(&EngineError::SwapTimeout(OUTPUT)));
    assert!(
        rig.engine.service().wait.is_some(),
        "retiring keeps the loop awake"
    );
    gate.open();
    assert!(serve_until(&mut rig, |rig| {
        rig.engine.wake(OUTPUT);
        rig.engine.service().wait.is_none() || !rig.engine.has_worker(OUTPUT)
    }));
    for _ in 0..3 {
        rig.engine.service();
    }
    // The reader was taken back and dropped by the engine.
    assert!(
        rig.engine
            .take::<Ticks>(OUTPUT, packets.reader)
            .unwrap()
            .is_none()
    );
    assert!(
        rig.engine
            .take::<Ticks>(ENCODER, packets.writer)
            .unwrap()
            .is_none()
    );
}

/// A runtime that counts `begin_tick` calls.
struct Spy;
impl Runtime for Spy {
    type Shared = AtomicU64;
    type Local = ();
    fn begin_tick(shared: &AtomicU64, _: &mut (), _: rbs_graph::runtime::Tick) {
        shared.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn an_idle_partition_swaps_without_running_a_tick() {
    let ticks = Arc::new(AtomicU64::new(0));
    let mut engine = Engine::with_clock(Arc::new(TestClock::new()), EngineConfig::default());
    engine
        .add_partition::<Spy>(RENDER, Arc::clone(&ticks), (), cadence())
        .unwrap();
    let mut graph = GraphDescription::new();
    let node = TestNode::new(Role::VideoSink, "sink");
    let id = graph.add_node(node.descriptor_arc()).unwrap();
    graph
        .set_state(id, rbs_core::lifecycle::Lifecycle::Standby)
        .unwrap();
    engine.adopt::<Spy>(RENDER, id, Box::new(node)).unwrap();
    for _ in 0..3 {
        let applied = engine.apply(&graph).unwrap();
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
    }
    let report = engine.shutdown();
    assert!(report.errors.is_empty());
    assert_eq!(engine.workers(), 0);
    assert_eq!(
        ticks.load(Ordering::Relaxed),
        0,
        "no tick ran on the control thread"
    );
}

#[test]
fn a_move_out_of_a_partition_whose_part_was_not_sent_yet_still_happens() {
    let gate = Arc::new(Gate::default());
    let mut rig = Rig::build(None, None, None, EXPECTED_TIMEOUT);
    // A second encoder in the encoder partition whose `cool` blocks.
    let slow = TestNode::new(Role::Encoder, "slow").cooling_on(&gate);
    let slow_sink = TestNode::new(Role::PacketSink, "slow-sink");
    let e2 = add(&mut rig.graph, &slow, ENCODER);
    let k2 = add(&mut rig.graph, &slow_sink, OUTPUT);
    rig.graph.connect(rig.source, "out", e2, "in").unwrap();
    rig.graph.connect(e2, "out", k2, "in").unwrap();
    rig.engine
        .adopt::<Ticks>(ENCODER, e2, Box::new(slow))
        .unwrap();
    rig.engine
        .adopt::<Ticks>(OUTPUT, k2, Box::new(slow_sink))
        .unwrap();
    let setup = rig.engine.apply(&rig.graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    rig.frame();
    // Edit 1: the slow encoder leaves; its partition does not swap in time.
    rig.graph
        .set_state(e2, rbs_core::lifecycle::Lifecycle::Standby)
        .unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.contains(&EngineError::SwapTimeout(ENCODER)));
    // Edit 2, still blocked: the other encoder moves out, so its old part is not sent.
    rig.graph.set_partition(rig.encoder, OUTPUT).unwrap();
    let applied = rig.engine.apply(&rig.graph).unwrap();
    assert!(applied.errors.contains(&EngineError::SwapTimeout(ENCODER)));
    gate.open();
    let probe = Arc::clone(&rig.encoder_probe);
    let before = probe.ticks.load(Ordering::Relaxed);
    assert!(serve_until(&mut rig, |rig| {
        rig.engine.wake(OUTPUT);
        probe.ticks.load(Ordering::Relaxed) > before + 2
    }));
    assert!(
        rig.engine
            .take::<Ticks>(ENCODER, rig.encoder)
            .unwrap()
            .is_none()
    );
}

#[test]
fn shutdown_after_a_timed_out_swap_leaves_no_worker() {
    // `cool` waits for a gate that opens only once shutdown stops the worker: the
    // shutdown's first wait times out while a schedule with nodes is in flight.
    let gate = Arc::new(Gate::holding(LIMIT));
    let mut engine = Engine::with_clock(
        Arc::new(TestClock::new()),
        EngineConfig::default().swap_timeout(EXPECTED_TIMEOUT),
    );
    engine
        .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
        .unwrap();
    let log: ReleaseLog = Arc::new(Mutex::new(Vec::new()));
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source").logging(&log);
    let a = TestNode::new(Role::VideoSink, "a")
        .cooling_on(&gate)
        .logging(&log);
    let b = TestNode::new(Role::VideoSink, "b").logging(&log);
    let s = add(&mut graph, &source, RENDER);
    let ka = add(&mut graph, &a, RENDER);
    let kb = add(&mut graph, &b, RENDER);
    graph.connect(s, "out", ka, "in").unwrap();
    graph.connect(s, "out", kb, "in").unwrap();
    engine.adopt::<()>(RENDER, s, Box::new(source)).unwrap();
    engine.adopt::<()>(RENDER, ka, Box::new(a)).unwrap();
    engine.adopt::<()>(RENDER, kb, Box::new(b)).unwrap();
    let setup = engine.apply(&graph).unwrap();
    assert!(setup.errors.is_empty(), "{:?}", setup.errors);
    // `a` leaves; its `cool` blocks, so a schedule with `source` and `b` is in flight.
    graph
        .set_state(ka, rbs_core::lifecycle::Lifecycle::Standby)
        .unwrap();
    let applied = engine.apply(&graph).unwrap();
    assert!(applied.errors.contains(&EngineError::SwapTimeout(RENDER)));
    let stops = engine.worker_stats(RENDER).unwrap();
    let opener = open_when(&gate, LIMIT, move || stops.stops() > 0);
    let report = engine.shutdown();
    assert!(opener.join().unwrap(), "the worker was stopped");
    assert!(report.errors.contains(&EngineError::SwapTimeout(RENDER)));
    assert_eq!(engine.workers(), 0, "no worker after shutdown");
    let mut released = log.lock().unwrap().clone();
    released.sort_unstable();
    assert_eq!(released, vec!["a", "b", "source"], "every node released");
}

/// A runtime whose `Local` is bound to the thread that attached it (ADR-0030).
struct Affine;

#[derive(Default)]
struct AffineLocal {
    attached: Option<std::thread::ThreadId>,
    log: Arc<AffineLog>,
}

#[derive(Default)]
struct AffineLog {
    attaches: AtomicU64,
    detaches: AtomicU64,
    /// Ticks or detaches on a thread that had not attached.
    wrong_thread: AtomicU64,
}

impl Runtime for Affine {
    type Shared = ();
    type Local = AffineLocal;

    fn attach(_: &(), local: &mut AffineLocal) {
        if local.attached.is_some() {
            local.log.wrong_thread.fetch_add(1, Ordering::Relaxed);
        }
        local.attached = Some(std::thread::current().id());
        local.log.attaches.fetch_add(1, Ordering::Relaxed);
    }

    fn detach(_: &(), local: &mut AffineLocal) {
        if local.attached != Some(std::thread::current().id()) {
            local.log.wrong_thread.fetch_add(1, Ordering::Relaxed);
        }
        local.attached = None;
        local.log.detaches.fetch_add(1, Ordering::Relaxed);
    }

    fn begin_tick(_: &(), local: &mut AffineLocal, _: rbs_graph::runtime::Tick) {
        if local.attached != Some(std::thread::current().id()) {
            local.log.wrong_thread.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records each request and checks that its guard is dropped on the same thread.
#[derive(Default)]
struct RecordingPolicy {
    requests: Mutex<Vec<(String, ThreadClass)>>,
    restored: Arc<AtomicU64>,
    restored_elsewhere: Arc<AtomicU64>,
}

impl ThreadPolicy for RecordingPolicy {
    fn enter(&self, request: &ThreadRequest<'_>) -> Result<ThreadGuard, ThreadPolicyError> {
        self.requests
            .lock()
            .unwrap()
            .push((request.name.to_owned(), request.class));
        if request.class == ThreadClass::Responsive {
            return Err(ThreadPolicyError::Denied("test"));
        }
        let me = std::thread::current().id();
        let (restored, elsewhere) = (
            Arc::clone(&self.restored),
            Arc::clone(&self.restored_elsewhere),
        );
        Ok(ThreadGuard::restoring(ThreadGrant::RealTime, move || {
            restored.fetch_add(1, Ordering::Relaxed);
            if std::thread::current().id() != me {
                elsewhere.fetch_add(1, Ordering::Relaxed);
            }
        }))
    }
}

#[test]
fn workers_enter_the_thread_policy_and_attach_the_runtime_on_every_thread() {
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default());
    let policy = Arc::new(RecordingPolicy::default());
    engine.set_thread_policy(Some(Arc::clone(&policy) as Arc<dyn ThreadPolicy>));
    let log = Arc::new(AffineLog::default());
    let local = AffineLocal {
        attached: None,
        log: Arc::clone(&log),
    };
    engine
        .add_partition::<Affine>(RENDER, Arc::new(()), local, cadence())
        .unwrap();
    engine
        .add_partition::<()>(
            OUTPUT,
            Arc::new(()),
            (),
            Pacing::doorbell(Duration::from_millis(1)),
        )
        .unwrap();
    let mut graph = GraphDescription::new();
    let source = TestNode::new(Role::Source, "source");
    let sink = TestNode::new(Role::VideoSink, "sink");
    let ids = (
        add(&mut graph, &source, RENDER),
        add(&mut graph, &sink, RENDER),
    );
    graph.connect(ids.0, "out", ids.1, "in").unwrap();
    engine
        .adopt::<Affine>(RENDER, ids.0, Box::new(source))
        .unwrap();
    engine
        .adopt::<Affine>(RENDER, ids.1, Box::new(sink))
        .unwrap();
    let (out_source, out_sink) = (
        TestNode::new(Role::Source, "out-source"),
        TestNode::new(Role::VideoSink, "out-sink"),
    );
    let out = (
        add(&mut graph, &out_source, OUTPUT),
        add(&mut graph, &out_sink, OUTPUT),
    );
    graph.connect(out.0, "out", out.1, "in").unwrap();
    engine
        .adopt::<()>(OUTPUT, out.0, Box::new(out_source))
        .unwrap();
    engine
        .adopt::<()>(OUTPUT, out.1, Box::new(out_sink))
        .unwrap();

    let render = engine.worker_stats(RENDER).unwrap();
    let output = engine.worker_stats(OUTPUT).unwrap();
    for round in 1..=2 {
        engine.apply(&graph).unwrap();
        assert_eq!(render.starts(), round);
        let before = render.ticks();
        clock.advance(FRAME);
        engine.wake(RENDER);
        assert!(eventually(LIMIT, || render.ticks() > before));
        // The render worker stops (its sink goes Standby) and restarts on a new thread.
        graph.set_state(ids.1, Lifecycle::Standby).unwrap();
        engine.apply(&graph).unwrap();
        assert!(!engine.has_worker(RENDER));
        graph.set_state(ids.1, Lifecycle::Active).unwrap();
    }
    // A denied policy leaves the worker at its default class; it still ticks.
    assert!(eventually(LIMIT, || output.ticks() >= 3));
    assert_eq!(output.thread_grant(), None);
    assert_eq!(output.policy_errors(), 1);
    assert_eq!(render.thread_grant(), Some(ThreadGrant::RealTime));
    let report = engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let requests = policy.requests.lock().unwrap().clone();
    let render_requests: Vec<_> = requests.iter().filter(|(n, _)| n == "rbs-render").collect();
    assert_eq!(render_requests.len(), 2, "{requests:?}");
    for (_, class) in render_requests {
        match class {
            ThreadClass::Periodic { period, .. } => {
                assert!(
                    period.abs_diff(FRAME) < Duration::from_micros(1),
                    "{class:?}"
                );
            }
            other => panic!("render asked for {other:?}"),
        }
    }
    assert!(requests.contains(&("rbs-partition-3".to_owned(), ThreadClass::Responsive)));
    // Each granted guard was undone once, on its own thread.
    assert_eq!(policy.restored.load(Ordering::Relaxed), 2);
    assert_eq!(policy.restored_elsewhere.load(Ordering::Relaxed), 0);
    // Every thread that ran the render executor attached it first and detached it
    // after; nothing ran it unattached.
    let attaches = log.attaches.load(Ordering::Relaxed);
    assert!(attaches >= 2, "{attaches}");
    assert_eq!(attaches, log.detaches.load(Ordering::Relaxed));
    assert_eq!(log.wrong_thread.load(Ordering::Relaxed), 0);
}

/// Panics in `enter` for responsive threads and in the restore for periodic ones.
struct PanickingPolicy;

impl ThreadPolicy for PanickingPolicy {
    fn enter(&self, request: &ThreadRequest<'_>) -> Result<ThreadGuard, ThreadPolicyError> {
        if request.class == ThreadClass::Responsive {
            panic!("enter panicked");
        }
        Ok(ThreadGuard::restoring(ThreadGrant::Raised, || {
            panic!("restore panicked")
        }))
    }
}

#[test]
fn a_panicking_thread_policy_loses_no_executor() {
    let clock = TestClock::new();
    let mut engine = Engine::with_clock(Arc::new(clock.clone()), EngineConfig::default());
    engine.set_thread_policy(Some(Arc::new(PanickingPolicy)));
    engine
        .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
        .unwrap();
    engine
        .add_partition::<()>(
            OUTPUT,
            Arc::new(()),
            (),
            Pacing::doorbell(Duration::from_millis(1)),
        )
        .unwrap();
    let mut graph = GraphDescription::new();
    let mut ids = Vec::new();
    for p in [RENDER, OUTPUT] {
        let (source, sink) = (
            TestNode::new(Role::Source, "source"),
            TestNode::new(Role::VideoSink, "sink"),
        );
        let pair = (add(&mut graph, &source, p), add(&mut graph, &sink, p));
        graph.connect(pair.0, "out", pair.1, "in").unwrap();
        engine.adopt::<()>(p, pair.0, Box::new(source)).unwrap();
        engine.adopt::<()>(p, pair.1, Box::new(sink)).unwrap();
        ids.push(pair);
    }
    let render = engine.worker_stats(RENDER).unwrap();
    let output = engine.worker_stats(OUTPUT).unwrap();
    for round in 1..=2 {
        engine.apply(&graph).unwrap();
        assert!(engine.has_worker(RENDER), "round {round}");
        let before = render.ticks();
        clock.advance(FRAME);
        engine.wake(RENDER);
        assert!(
            eventually(LIMIT, || render.ticks() > before),
            "round {round}"
        );
        // Stopping runs the panicking restore; the executor must come back.
        graph.set_state(ids[0].1, Lifecycle::Standby).unwrap();
        let applied = engine.apply(&graph).unwrap();
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        assert!(!engine.has_worker(RENDER));
        graph.set_state(ids[0].1, Lifecycle::Active).unwrap();
    }
    assert_eq!(render.starts(), 2);
    assert_eq!(render.thread_grant(), Some(ThreadGrant::Raised));
    // A panic in `enter` counts as an error; the worker runs anyway.
    assert!(eventually(LIMIT, || output.ticks() >= 3));
    assert_eq!(output.policy_errors(), 1);
    let report = engine.shutdown();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}
