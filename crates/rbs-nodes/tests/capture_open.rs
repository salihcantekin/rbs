//! The capture source node opens on a thread of its own (ADR-0152 decision 5, io-nodes
//! spec "Capture source node"): `prepare` never waits for `open` or `start`, the first
//! frame follows the open, a never-ending open fails after the start limit, and a source
//! released while it opens stops the session that open produced.
//!
//! The open is held by a gate the test opens; the waits on the opening thread are
//! conditions with a real-time hang guard, never a sleep (ADR-0032). Needs a GPU
//! adapter (software is fine); without one each test returns early.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Test code.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rbs_capture::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, CaptureTarget,
    TargetId, TargetKind,
};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::capability::CapabilitySet;
use rbs_core::fault::SourceFault;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{VideoFormat, VideoFrame};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::CaptureSourceNode;
use rbs_nodes::capture::{CaptureNodeStats, START_LIMIT};

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);
const HANG_GUARD: Duration = Duration::from_secs(10);

/// A gate the opening thread waits at in `start` until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

/// What the gated sessions did.
#[derive(Default)]
struct Calls {
    entered: AtomicUsize,
    started: AtomicUsize,
    stopped: AtomicUsize,
}

/// The synthetic adapter with a `start` that waits at a gate.
struct Gated {
    inner: TestCaptureFactory,
    gate: Arc<Gate>,
    calls: Arc<Calls>,
}

impl AdapterFactory for Gated {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.gated")
    }
    fn display_name(&self) -> &str {
        "Gated test capture"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.inner.probe()
    }
}

impl CaptureFactory for Gated {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        self.inner.enumerate(kind)
    }
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        Ok(Box::new(GatedSession {
            inner: self.inner.open(request)?,
            gate: Arc::clone(&self.gate),
            calls: Arc::clone(&self.calls),
        }))
    }
}

struct GatedSession {
    inner: Box<dyn CaptureSession>,
    gate: Arc<Gate>,
    calls: Arc<Calls>,
}

impl CaptureSession for GatedSession {
    fn format(&self) -> VideoFormat {
        self.inner.format()
    }
    fn start(&mut self) -> Result<(), CaptureError> {
        self.calls.entered.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        let started = self.inner.start();
        self.calls.started.fetch_add(1, Ordering::SeqCst);
        started
    }
    fn stop(&mut self) {
        self.calls.stopped.fetch_add(1, Ordering::SeqCst);
        self.inner.stop();
    }
    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
        self.inner.latest_frame()
    }
    fn stats(&self) -> CaptureStats {
        self.inner.stats()
    }
}

struct Sink(NodeDescriptor);

impl Node<GpuRuntime> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        std::hint::black_box(ctx.inputs().video(0, 0).map(|f| f.sequence()));
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

struct Rig {
    ctx: Arc<GpuContext>,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    graph: GraphDescription,
    clock: TestClock,
    tick: u64,
    capture: NodeId,
    stats: Arc<CaptureNodeStats>,
    gate: Arc<Gate>,
    calls: Arc<Calls>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Let a waiting opening thread finish.
        self.gate.open();
    }
}

fn gpu() -> Option<Arc<GpuContext>> {
    match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Some(Arc::new(ctx)),
        Err(e) => {
            eprintln!("SKIPPED: no GPU adapter ({e})");
            None
        }
    }
}

fn activate(graph: &mut GraphDescription, id: NodeId) {
    graph.set_state(id, Lifecycle::Standby).unwrap();
    graph.set_state(id, Lifecycle::Active).unwrap();
}

impl Rig {
    /// A gated capture of `target` linked to a sink, applied: `prepare` has run.
    fn new(target: &str) -> Option<Self> {
        let ctx = gpu()?;
        let (mut ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        let clock = TestClock::new();
        let gate = Arc::new(Gate::default());
        let calls = Arc::new(Calls::default());
        let factory = Arc::new(Gated {
            inner: TestCaptureFactory::new(),
            gate: Arc::clone(&gate),
            calls: Arc::clone(&calls),
        });
        let request = CaptureRequest::new(TargetId::new(target), Arc::new(clock.clone()))
            .with_frame_rate(FrameRate::FPS_30);
        let node = CaptureSourceNode::new(factory, request);
        let stats = node.stats();
        let mut graph = GraphDescription::new();
        let capture = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        activate(&mut graph, capture);
        ctl.adopt(capture, Box::new(node)).unwrap();
        let sink = Sink(NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ));
        let s = graph.add_node(Arc::new(sink.0.clone())).unwrap();
        activate(&mut graph, s);
        ctl.adopt(s, Box::new(sink)).unwrap();
        graph.connect(capture, "video", s, "in").unwrap();
        let mut rig = Self {
            ctx,
            ctl,
            exec,
            graph,
            clock,
            tick: 0,
            capture,
            stats,
            gate,
            calls,
        };
        rig.apply();
        Some(rig)
    }

    fn apply(&mut self) {
        let report = self.ctl.apply(&self.graph).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        self.exec.swap_pending();
        let _ = self.ctl.collect();
    }

    /// One render tick after moving the clock by one frame.
    fn tick(&mut self) {
        self.clock.advance(FRAME);
        self.tick_now();
    }

    /// One render tick at the clock's current time.
    fn tick_now(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let mut tick = Tick::of(&cadence, self.tick);
        tick.time = self.clock.now();
        self.exec.run_tick(tick);
        self.tick += 1;
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
    }

    fn frames(&self) -> u64 {
        self.stats.frames.load(Ordering::Relaxed)
    }

    /// Ticks without moving the clock until the session the opening thread hands over
    /// puts its first frame on the link. The hand-off is a condition on another thread;
    /// the clock stays put, so what the frame says does not depend on the wait.
    fn tick_until_first_frame(&mut self) {
        let guard = Instant::now();
        while self.frames() == 0 {
            assert!(guard.elapsed() < HANG_GUARD, "no first frame");
            self.tick_now();
            std::thread::yield_now();
        }
    }
}

/// Waits until `done` holds, for a condition on the opening thread. A hang guard only.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    let guard = Instant::now();
    while !done() {
        assert!(guard.elapsed() < HANG_GUARD, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

#[test]
fn prepare_returns_while_the_open_waits_and_the_first_frame_follows_it() {
    let Some(mut rig) = Rig::new("test-display-0") else {
        return;
    };
    // `apply` (which ran `prepare`) came back with the open still at the gate.
    let calls = Arc::clone(&rig.calls);
    wait_for("the open to reach the gate", || {
        calls.entered.load(Ordering::SeqCst) == 1
    });
    for _ in 0..5 {
        rig.tick();
    }
    assert_eq!(rig.frames(), 0, "no frame before the open finished");
    assert_eq!(rig.stats.picture_size(), None);
    assert_eq!(rig.stats.warm_up(), None);

    rig.gate.open();
    rig.tick_until_first_frame();
    assert_eq!(rig.frames(), 1);
    assert_eq!(calls.started.load(Ordering::SeqCst), 1);
    assert_eq!(rig.stats.picture_size(), Some((1280, 720)));
    // Prepared at time 0; the session started five frames later, and its first frame
    // is stamped then (kept in whole microseconds).
    let five = u64::try_from((FRAME * 5).as_micros()).unwrap();
    assert_eq!(rig.stats.warm_up(), Some(Duration::from_micros(five)));
    assert_eq!(rig.stats.fault(), None);
}

#[test]
fn an_open_that_never_returns_fails_after_the_start_limit() {
    let Some(mut rig) = Rig::new("test-display-0") else {
        return;
    };
    let calls = Arc::clone(&rig.calls);
    wait_for("the open to reach the gate", || {
        calls.entered.load(Ordering::SeqCst) == 1
    });
    rig.tick();
    assert_eq!(rig.stats.fault(), None, "waiting is not a fault");
    rig.clock.advance(START_LIMIT);
    rig.tick();
    assert_eq!(rig.stats.fault(), Some(SourceFault::Other));
    assert_eq!(rig.frames(), 0);
    // A restart while that open still has not returned starts no second thread.
    let _ = rig.ctl.collect();
    rig.graph
        .set_state(rig.capture, Lifecycle::Standby)
        .unwrap();
    rig.apply();
    rig.tick();
    let _ = rig.ctl.collect();
    rig.graph.set_state(rig.capture, Lifecycle::Active).unwrap();
    let report = rig.ctl.apply(&rig.graph).unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(rig.calls.entered.load(Ordering::SeqCst), 1);
}

#[test]
fn a_source_released_while_it_opens_stops_the_session_that_open_made() {
    let Some(mut rig) = Rig::new("test-camera-0") else {
        return;
    };
    let calls = Arc::clone(&rig.calls);
    wait_for("the open to reach the gate", || {
        calls.entered.load(Ordering::SeqCst) == 1
    });
    rig.graph
        .set_state(rig.capture, Lifecycle::Standby)
        .unwrap();
    rig.apply();
    rig.tick();
    let _ = rig.ctl.collect();
    rig.gate.open();
    wait_for("the abandoned session to stop", || {
        calls.stopped.load(Ordering::SeqCst) == 1
    });
    assert_eq!(calls.started.load(Ordering::SeqCst), 1);
    assert_eq!(rig.frames(), 0);
}

#[test]
fn a_source_shown_again_while_its_last_open_runs_opens_after_it() {
    let Some(mut rig) = Rig::new("test-camera-0") else {
        return;
    };
    let calls = Arc::clone(&rig.calls);
    wait_for("the open to reach the gate", || {
        calls.entered.load(Ordering::SeqCst) == 1
    });
    rig.graph
        .set_state(rig.capture, Lifecycle::Standby)
        .unwrap();
    rig.apply();
    rig.tick();
    let _ = rig.ctl.collect();
    rig.graph.set_state(rig.capture, Lifecycle::Active).unwrap();
    rig.apply();
    // The second open waits for the first; nothing opens twice at once.
    assert_eq!(calls.entered.load(Ordering::SeqCst), 1);
    rig.gate.open();
    wait_for("the second open to finish", || {
        calls.started.load(Ordering::SeqCst) == 2
    });
    assert_eq!(
        calls.stopped.load(Ordering::SeqCst),
        1,
        "the first was stopped"
    );
    rig.tick_until_first_frame();
    assert_eq!(rig.frames(), 1);
}

#[test]
fn a_failed_open_reports_its_fault() {
    let Some(mut rig) = Rig::new("no-such-target") else {
        return;
    };
    let stats = Arc::clone(&rig.stats);
    let guard = Instant::now();
    while stats.fault().is_none() {
        assert!(guard.elapsed() < HANG_GUARD, "no fault");
        rig.tick();
        std::thread::yield_now();
    }
    assert_eq!(stats.fault(), Some(SourceFault::TargetGone));
    assert_eq!(rig.calls.entered.load(Ordering::SeqCst), 0);
}

/// A tick while the open is pending (a wait-free pop of an empty ring) allocates no
/// more than a steady tick of the running capture with no new frame (P15).
#[test]
fn ticks_while_opening_allocate_no_more_than_steady_ticks() {
    let Some(mut rig) = Rig::new("test-display-0") else {
        return;
    };
    let calls = Arc::clone(&rig.calls);
    wait_for("the open to reach the gate", || {
        calls.entered.load(Ordering::SeqCst) == 1
    });
    rig.tick_now();
    let count = |rig: &mut Rig| {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let start = rig.tick;
        let now = rig.clock.now();
        let total = allocation_counter::measure(|| {
            for i in start..start + 100 {
                let mut tick = Tick::of(&cadence, i);
                tick.time = now;
                rig.exec.run_tick(tick);
            }
        })
        .count_total;
        rig.tick += 100;
        total
    };
    let opening = count(&mut rig);
    assert_eq!(rig.frames(), 0);
    rig.gate.open();
    rig.tick_until_first_frame();
    // The clock stays put: the session has no new frame on these ticks.
    let steady = count(&mut rig);
    assert_eq!(rig.frames(), 1);
    assert!(opening <= steady, "opening {opening}, steady {steady}");
}
