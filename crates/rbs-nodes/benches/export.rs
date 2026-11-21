//! One render tick of a held stream's picture path (hot path, P15; ADR-0124 phase
//! 2b): capture -> compositor -> video export -> sink, GPU waited on each tick so a
//! readback completes by the next one. `run` is a stream not held, `freeze` a stream
//! held on its last frame, `scene` a stream held on a scene (a second compositor on
//! the export's host-only "hold" input, read back in place of the program's).
//!
//! Runs on any adapter, software included; the numbers are the whole tick, so they
//! compare variants on one machine, not machines.
//!
//! Run: `cargo bench -p rbs-nodes --bench export`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::PixelFormat;
use rbs_core::pause::{HoldPicture, HoldSwitch};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuRequest, YuvFormat};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{CaptureSourceNode, ExportHold, VideoExportNode};

const W: u32 = 1280;
const H: u32 = 720;

/// Takes the frame on its input and lets it go.
struct Sink {
    desc: NodeDescriptor,
}

impl Node<GpuRuntime> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        black_box(ctx.inputs().video(0, 0).map(|f| f.timestamp()));
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

struct Rig {
    graph: GraphDescription,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    ctx: Arc<GpuContext>,
    clock: TestClock,
    tick: u64,
}

impl Rig {
    fn add(&mut self, node: Box<dyn Node<GpuRuntime>>) -> NodeId {
        let id = self
            .graph
            .add_node(Arc::new(node.descriptor().clone()))
            .unwrap();
        self.graph.set_state(id, Lifecycle::Standby).unwrap();
        self.graph.set_state(id, Lifecycle::Active).unwrap();
        self.ctl.adopt(id, node).unwrap();
        id
    }

    fn tick(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        let tick = Tick::of(&cadence, self.tick);
        self.clock.set(tick.time);
        self.exec.run_tick(tick);
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        self.tick += 1;
        let _ = self.ctl.collect();
    }
}

/// The pipeline, held as `picture` (`None`: not held). `scene` links a second
/// compositor to the export's "hold" input.
fn rig(ctx: &Arc<GpuContext>, picture: Option<HoldPicture>, scene: bool) -> Rig {
    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(ctx), RenderLocal::default());
    let clock = TestClock::new();
    let mut rig = Rig {
        graph: GraphDescription::new(),
        ctl,
        exec,
        ctx: Arc::clone(ctx),
        clock: clock.clone(),
        tick: 0,
    };
    let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock))
        .with_frame_rate(FrameRate::FPS_60)
        .with_pixel_formats(&[PixelFormat::Bgra8]);
    let capture = rig.add(Box::new(
        CaptureSourceNode::new(Arc::new(TestCaptureFactory::new()), request).open_inline(true),
    ));
    let canvas = CanvasDesc {
        width: W,
        height: H,
    };
    let layer = |producer| vec![NodeLayer::new(producer, 0, 0.0, 0.0, W as f32, H as f32)];
    let (program, _) = CompositorNode::new(canvas, 4, layer(capture));
    let program = rig.add(Box::new(program));
    let switch = Arc::new(HoldSwitch::default());
    let export = VideoExportNode::new(YuvFormat::Nv12, W, H, W, H)
        .with_hold(Arc::new(ExportHold::new(Arc::clone(&switch))));
    let end = export.end();
    let export = rig.add(Box::new(export));
    let sink = rig.add(Box::new(Sink {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.bench.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
    }));
    rig.graph
        .connect(capture, "video", program, "layers")
        .unwrap();
    rig.graph
        .connect(program, "canvas", export, "canvas")
        .unwrap();
    rig.graph.connect(export, "video", sink, "in").unwrap();
    // SCENE BEGIN
    if scene {
        // The hold scene draws the same capture, as a scene sharing a source does.
        let (hold, _) = CompositorNode::new(canvas, 4, layer(capture));
        let hold = rig.add(Box::new(hold));
        rig.graph.connect(capture, "video", hold, "layers").unwrap();
        rig.graph.connect(hold, "canvas", export, "hold").unwrap();
    }
    // SCENE END
    rig.ctl.apply(&rig.graph).unwrap();
    rig.exec.swap_pending();
    let _ = rig.ctl.collect();
    for _ in 0..4 {
        rig.tick();
    }
    if let Some(picture) = picture {
        switch.hold(picture);
        end.wake_at(Timestamp::ZERO);
        for _ in 0..4 {
            rig.tick();
        }
    }
    rig
}

fn ticks(c: &mut Criterion) {
    let Ok(ctx) = GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) else {
        eprintln!("SKIPPED: no GPU adapter");
        return;
    };
    let ctx = Arc::new(ctx);
    let mut group = c.benchmark_group("export_tick_720p");
    let cases: &[(&str, Option<HoldPicture>, bool)] = &[
        ("run", None, false),
        ("freeze", Some(HoldPicture::Freeze), false),
        // SCENE BEGIN
        ("scene", Some(HoldPicture::Scene), true),
        // SCENE END
    ];
    for &(name, picture, scene) in cases {
        let mut r = rig(&ctx, picture, scene);
        group.bench_function(name, |b| b.iter(|| r.tick()));
    }
    group.finish();
}

criterion_group!(benches, ticks);
criterion_main!(benches);
