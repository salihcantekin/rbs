//! The capture source node on the render runtime (hot path, P15; ADR-0152 phase 4).
//!
//! - `capture/no_frame`: one render tick of a capture source whose session has no new
//!   frame (the slot keeps the last one).
//! - `capture/new_frame_720p`: one render tick that takes a new 1280 x 720 BGRA
//!   system frame and uploads it (CPU side; the GPU work is waited for outside the
//!   timed part).
//!
//! Uses the synthetic capture adapter on a `TestClock`, so a frame is due exactly when
//! the bench moves the clock. Needs a GPU adapter (software is fine).
//! Run: `cargo bench -p rbs-nodes --bench capture`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{PixelFormat, Size};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::CaptureSourceNode;
use rbs_nodes::capture::CaptureNodeStats;

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
    _ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    clock: TestClock,
    cadence: Cadence,
    tick: u64,
    stats: Arc<CaptureNodeStats>,
}

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);

impl Rig {
    fn new(ctx: Arc<GpuContext>) -> Self {
        let (mut ctl, mut exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        let clock = TestClock::new();
        let factory = Arc::new(TestCaptureFactory::new());
        let request = CaptureRequest::new(TargetId::new("test-display-0"), Arc::new(clock.clone()))
            .with_frame_rate(FrameRate::FPS_30)
            .with_size(Size::new(1280, 720))
            .with_pixel_formats(&[PixelFormat::Bgra8]);
        let node = CaptureSourceNode::new(factory, request);
        let stats = node.stats();
        let mut graph = GraphDescription::new();
        let id = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        ctl.adopt(id, Box::new(node)).unwrap();
        let sink = Sink(NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.bench.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ));
        let s = graph.add_node(Arc::new(sink.0.clone())).unwrap();
        graph.set_state(s, Lifecycle::Standby).unwrap();
        graph.set_state(s, Lifecycle::Active).unwrap();
        ctl.adopt(s, Box::new(sink)).unwrap();
        graph.connect(id, "video", s, "in").unwrap();
        assert!(ctl.apply(&graph).unwrap().failed.is_empty());
        exec.swap_pending();
        let _ = ctl.collect();
        let mut rig = Self {
            ctx,
            _ctl: ctl,
            exec,
            clock,
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30),
            tick: 0,
            stats,
        };
        // Tick until the first frame is on the link (the open may finish on another
        // thread). A hang guard, not a timing wait.
        let guard = Instant::now();
        while rig.stats.frames.load(Ordering::Relaxed) == 0 {
            rig.clock.advance(FRAME);
            rig.tick();
            assert!(guard.elapsed() < Duration::from_secs(10), "no first frame");
            std::thread::yield_now();
        }
        rig
    }

    fn tick(&mut self) -> Duration {
        let t = Instant::now();
        self.exec.run_tick(Tick::of(&self.cadence, self.tick));
        let took = t.elapsed();
        self.tick += 1;
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        took
    }
}

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            eprintln!("SKIPPED: no GPU adapter ({e})");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());

    let mut group = c.benchmark_group("capture");
    {
        let mut r = Rig::new(Arc::clone(&ctx));
        group.bench_function("no_frame", |b| {
            b.iter(|| {
                r.exec.run_tick(Tick::of(&r.cadence, r.tick));
                r.tick += 1;
            })
        });
    }
    group.sample_size(20);
    {
        let mut r = Rig::new(Arc::clone(&ctx));
        group.bench_function("new_frame_720p", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    r.clock.advance(FRAME);
                    total += r.tick();
                }
                total
            })
        });
        assert!(r.stats.uploaded.load(Ordering::Relaxed) > 0);
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
