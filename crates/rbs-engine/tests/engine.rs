//! The engine loop (ADR-0027): workers per partition, doorbell and cadence pacing,
//! restarts through the control loop, moves in two phases and shutdown order.
//!
//! Time is a `TestClock`; tests move it and wake the workers. They wait for other
//! threads with `eventually` (yielding with a generous real-time limit), never for a
//! duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Engine, EngineConfig, EngineError, EngineEvent, EngineHandle, Pacing};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::runtime::{RestartPolicy, Runtime, RuntimeEvent};
use support::{Gate, Probe, ReleaseLog, Role, TestNode, add, eventually};

const RENDER: PartitionId = PartitionId::RENDER;
const ENCODER: PartitionId = PartitionId(7);
const OUTPUT: PartitionId = PartitionId(3);
const LIMIT: Duration = Duration::from_secs(10);
const FRAME: Duration = Duration::from_nanos(16_666_667);

/// Another runtime, to check that partitions keep their runtime types apart.
struct Other;
impl Runtime for Other {
    type Shared = ();
    type Local = ();
}

fn cadence() -> Pacing {
    Pacing::cadence(Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60))
}

/// source (render) -> encoder (partition 7) -> packet sink (partition 3). The output
/// timer is an hour, so the output worker only ticks when its doorbell rings.
struct Rig {
    clock: TestClock,
    engine: Engine,
    graph: GraphDescription,
    source: NodeId,
    encoder: NodeId,
    sink: NodeId,
    sink_probe: Arc<Probe>,
    encoder_probe: Arc<Probe>,
}

impl Rig {
    fn new(log: Option<&ReleaseLog>) -> Self {
        Self::build(log, None, Duration::from_secs(10))
    }

    /// `gate`: the encoder's and the sink's `cool` wait for it. `swap_timeout`: the
    /// engine's.
    fn build(log: Option<&ReleaseLog>, gate: Option<&Arc<Gate>>, swap_timeout: Duration) -> Self {
        let clock = TestClock::new();
        let mut engine = Engine::with_clock(
            Arc::new(clock.clone()),
            EngineConfig::default().swap_timeout(swap_timeout),
        );
        engine
            .add_partition::<()>(RENDER, Arc::new(()), (), cadence())
            .unwrap();
        engine
            .add_partition::<()>(ENCODER, Arc::new(()), (), cadence())
            .unwrap();
        engine
            .add_partition::<()>(
                OUTPUT,
                Arc::new(()),
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
            node
        };
        let source = named(Role::Source, "source");
        let encoder = named(Role::Encoder, "encoder");
        let sink = named(Role::PacketSink, "sink");
        let (sink_probe, encoder_probe) = (Arc::clone(&sink.probe), Arc::clone(&encoder.probe));
        let ids = (
            add(&mut graph, &source, RENDER),
            add(&mut graph, &encoder, ENCODER),
            add(&mut graph, &sink, OUTPUT),
        );
        graph.connect(ids.0, "out", ids.1, "in").unwrap();
        graph.connect(ids.1, "out", ids.2, "in").unwrap();
        engine.adopt::<()>(RENDER, ids.0, Box::new(source)).unwrap();
        engine
            .adopt::<()>(ENCODER, ids.1, Box::new(encoder))
            .unwrap();
        engine.adopt::<()>(OUTPUT, ids.2, Box::new(sink)).unwrap();
        Self {
            clock,
            engine,
            graph,
            source: ids.0,
            encoder: ids.1,
            sink: ids.2,
            sink_probe,
            encoder_probe,
        }
    }

    /// Moves the clock one frame and waits until the cadence workers ran that tick.
    fn frame(&mut self) {
        let before: Vec<u64> = [RENDER, ENCODER]
            .iter()
            .filter(|p| self.engine.has_worker(**p))
            .map(|p| self.engine.worker_stats(*p).unwrap().ticks())
            .collect();
        self.clock.advance(FRAME);
        let parts: Vec<PartitionId> = [RENDER, ENCODER]
            .into_iter()
            .filter(|p| self.engine.has_worker(*p))
            .collect();
        for p in &parts {
            self.engine.wake(*p);
        }
        for (p, before) in parts.iter().zip(before) {
            let stats = self.engine.worker_stats(*p).unwrap();
            assert!(eventually(LIMIT, || stats.ticks() > before), "{p} ticked");
        }
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
    // Render and encoder tick on the same cadence in either order, so the encoder
    // may find a frame replaced now and then (counted); most frames get through.
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

/// Runs `service` (and frames) until `cond` holds.
fn serve_until(rig: &mut Rig, mut cond: impl FnMut(&mut Rig) -> bool) -> bool {
    for _ in 0..200 {
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
    let mut rig = Rig::build(None, Some(&gate), Duration::from_millis(20));
    rig.engine.apply(&rig.graph).unwrap();
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
            .take::<()>(ENCODER, rig.encoder)
            .unwrap()
            .is_none()
    );
}

#[test]
fn retired_endpoints_are_dropped_after_a_swap_timeout() {
    let gate = Arc::new(Gate::default());
    let mut rig = Rig::build(None, Some(&gate), Duration::from_millis(20));
    rig.engine.apply(&rig.graph).unwrap();
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
            .take::<()>(OUTPUT, packets.reader)
            .unwrap()
            .is_none()
    );
    assert!(
        rig.engine
            .take::<()>(ENCODER, packets.writer)
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
