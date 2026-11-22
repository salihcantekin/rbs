//! The capture source node lends the newest frame's picture mapping to the source
//! entry's cell (ADR-0165 decision 5, io-nodes spec "Capture source node"): written
//! only while the switch is on, cleared when it goes off and on release, never
//! written without a switch. Uses the reference adapter opened inline, so every tick
//! is deterministic (ADR-0032). Needs a GPU adapter (software is fine); without one
//! each test returns early.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Test code.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_capture::{CaptureRequest, Cover, MappingCell, MappingSwitch, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::Size;
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
use rbs_nodes::capture::CaptureNodeStats;

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Pulls the capture's output, so the capture is scheduled.
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
}

impl Rig {
    /// An inline capture of `target`, Active and prepared. `switch` goes into the
    /// request when given; `cell` is lent to the node.
    fn new(target: &str, switch: Option<&MappingSwitch>, cell: &MappingCell) -> Option<Self> {
        let ctx = match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Arc::new(ctx),
            Err(e) => {
                eprintln!("SKIPPED: no GPU adapter ({e})");
                return None;
            }
        };
        let (mut ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        let clock = TestClock::new();
        let mut request = CaptureRequest::new(TargetId::new(target), Arc::new(clock.clone()))
            .with_frame_rate(FrameRate::FPS_30);
        if let Some(switch) = switch {
            request = request.with_mapping(switch.clone());
        }
        let node = CaptureSourceNode::new(Arc::new(TestCaptureFactory::new()), request)
            .open_inline(true)
            .with_mapping_cell(cell.clone());
        let stats = node.stats();
        let mut graph = GraphDescription::new();
        let capture = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        graph.set_state(capture, Lifecycle::Standby).unwrap();
        graph.set_state(capture, Lifecycle::Active).unwrap();
        ctl.adopt(capture, Box::new(node)).unwrap();
        let sink = Sink(NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ));
        let s = graph.add_node(Arc::new(sink.0.clone())).unwrap();
        graph.set_state(s, Lifecycle::Standby).unwrap();
        graph.set_state(s, Lifecycle::Active).unwrap();
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

    /// One render tick one frame later; returns the frame count after it.
    fn tick(&mut self) -> u64 {
        self.clock.advance(FRAME);
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let mut tick = Tick::of(&cadence, self.tick);
        tick.time = self.clock.now();
        self.exec.run_tick(tick);
        self.tick += 1;
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        self.stats.frames.load(Ordering::Relaxed)
    }

    /// Ticks until one more frame went out (bounded: the reference adapter delivers a
    /// frame on every tick that moves the clock by one frame).
    fn next_frame(&mut self) {
        let before = self.stats.frames.load(Ordering::Relaxed);
        for _ in 0..4 {
            if self.tick() > before {
                return;
            }
        }
        panic!("no frame in four ticks");
    }

    /// One render tick at the same time: the reference adapter has no new frame.
    fn tick_still(&mut self) -> u64 {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let mut tick = Tick::of(&cadence, self.tick);
        tick.time = self.clock.now();
        self.exec.run_tick(tick);
        self.tick += 1;
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        self.stats.frames.load(Ordering::Relaxed)
    }

    fn release(&mut self) {
        self.graph
            .set_state(self.capture, Lifecycle::Standby)
            .unwrap();
        self.apply();
    }
}

#[test]
fn the_cell_follows_the_switch_and_is_cleared_on_release() {
    let switch = MappingSwitch::new();
    let cell = MappingCell::new();
    let Some(mut rig) = Rig::new("test-window-0", Some(&switch), &cell) else {
        return;
    };
    rig.next_frame();
    assert_eq!(cell.get(), None, "nothing while the switch is off");

    switch.set(true);
    rig.next_frame();
    let mapping = cell.get().expect("a mapping while the switch is on");
    assert_eq!(mapping.cover, Cover::Clear);
    assert_eq!(mapping.picture, Size::new(640, 360));
    assert!(mapping.frame_time <= rig.clock.now());
    let first = mapping.frame_time;
    rig.next_frame();
    assert!(
        cell.get().expect("still mapped").frame_time > first,
        "each new frame's mapping replaces the last"
    );

    switch.set(false);
    rig.next_frame();
    assert_eq!(cell.get(), None, "cleared when the switch goes off");

    switch.set(true);
    rig.next_frame();
    assert!(cell.get().is_some());
    rig.release();
    assert_eq!(cell.get(), None, "cleared on release");
}

#[test]
fn without_a_switch_the_cell_is_never_written() {
    let cell = MappingCell::new();
    let Some(mut rig) = Rig::new("test-display-0", None, &cell) else {
        return;
    };
    for _ in 0..3 {
        rig.next_frame();
    }
    assert_eq!(cell.get(), None);
}

/// A tick without a new frame publishes the mapping again, so a covered test that
/// changed while a window's content stood still reaches the filters (capture-port.md
/// rule 4: WGC and ScreenCaptureKit send no frame then).
#[test]
fn a_tick_without_a_frame_publishes_the_mapping_again() {
    let switch = MappingSwitch::new();
    switch.set(true);
    let cell = MappingCell::new();
    let Some(mut rig) = Rig::new("test-window-0", Some(&switch), &cell) else {
        return;
    };
    rig.next_frame();
    let mapping = cell.get().expect("mapped");
    // Stands in for an older value: the next tick must overwrite it.
    cell.publish(Some(mapping.with_cover(Cover::Covered)));
    let frames = rig.stats.frames.load(Ordering::Relaxed);
    assert_eq!(rig.tick_still(), frames, "no new frame on a still tick");
    let again = cell.get().expect("still mapped");
    assert_eq!(again.cover, Cover::Clear);
    assert_eq!(again.frame_time, mapping.frame_time);
}
