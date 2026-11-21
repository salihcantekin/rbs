//! Render on change (R-03 of the November 2026 performance audit, hot path, P15): one
//! render tick of sources -> compositor -> video export -> sink at 1080p, GPU waited on
//! each tick, with the compositor drawing every tick (`every_tick`, the behaviour before
//! R-03) or only on change (`on_change`).
//!
//! - `static`: three stills, nothing changes after the first draw.
//! - `half_live`: the stills and a 30 fps capture on a 60 fps canvas.
//! - `live`: a 60 fps capture on a 60 fps canvas, a new picture every tick (the cost
//!   of the check itself; `on_change` must stay within noise of `every_tick`).
//!
//! `on_change_scroll_channel` is `on_change` with the compositor's scroll channel and
//! no binding, as every studio compositor has it (ADR-0150 decision 7): the per-tick
//! cost a canvas pays while nothing scrolls.
//!
//! Runs on any adapter, software included; the numbers are the whole tick, so they
//! compare variants on one machine, not machines.
//!
//! Run: `cargo bench -p rbs-nodes --bench render_on_change`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::PixelFormat;
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuRequest, YuvFormat};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{CaptureSourceNode, StillSourceNode, VideoExportNode};

const W: u32 = 1920;
const H: u32 = 1080;

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
    /// Kept so the scroll channel stays open.
    _scroll: Option<rbs_gpu::ScrollSender>,
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

/// The pipeline: three stills, plus a capture at `capture` fps when given.
fn rig(
    ctx: &Arc<GpuContext>,
    capture: Option<FrameRate>,
    every_tick: bool,
    scroll_channel: bool,
) -> Rig {
    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(ctx), RenderLocal::default());
    let clock = TestClock::new();
    let mut rig = Rig {
        graph: GraphDescription::new(),
        ctl,
        exec,
        ctx: Arc::clone(ctx),
        clock: clock.clone(),
        tick: 0,
        _scroll: None,
    };
    let mut producers = Vec::new();
    for grey in [40u8, 120, 200] {
        let still = StillSourceNode::new(64, 64, vec![grey; 64 * 64 * 4]).unwrap();
        producers.push(rig.add(Box::new(still)));
    }
    if let Some(rate) = capture {
        let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock))
            .with_frame_rate(rate)
            .with_pixel_formats(&[PixelFormat::Bgra8]);
        producers.push(rig.add(Box::new(
            CaptureSourceNode::new(Arc::new(TestCaptureFactory::new()), request).open_inline(true),
        )));
    }
    let (w, h) = (W as f32, H as f32);
    let layers = producers
        .iter()
        .enumerate()
        .map(|(i, &p)| match i {
            0 => NodeLayer::new(p, i, 0.0, 0.0, w, h),
            3 => NodeLayer::new(p, i, w * 0.25, h * 0.25, w * 0.5, h * 0.5),
            _ => NodeLayer::new(p, i, w * 0.7, h * 0.1 * i as f32, w * 0.25, h * 0.1),
        })
        .collect();
    let (comp, _) = CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        8,
        layers,
    );
    let comp: Box<dyn Node<GpuRuntime>> = if every_tick {
        Box::new(comp.every_tick())
    } else if scroll_channel {
        let (comp, sender) = comp.with_scroll();
        rig._scroll = Some(sender);
        Box::new(comp)
    } else {
        Box::new(comp)
    };
    let comp = rig.add(comp);
    let export = rig.add(Box::new(VideoExportNode::new(YuvFormat::Nv12, W, H, W, H)));
    let sink = rig.add(Box::new(Sink {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.bench.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
    }));
    for &p in &producers {
        rig.graph.connect(p, "video", comp, "layers").unwrap();
    }
    rig.graph.connect(comp, "canvas", export, "canvas").unwrap();
    rig.graph.connect(export, "video", sink, "in").unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    rig.exec.swap_pending();
    let _ = rig.ctl.collect();
    for _ in 0..8 {
        rig.tick();
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
    eprintln!("adapter: {}", ctx.adapter_name());
    let ctx = Arc::new(ctx);
    let mut group = c.benchmark_group("render_tick_1080p");
    let scenes: &[(&str, Option<FrameRate>)] = &[
        ("static", None),
        ("half_live", Some(FrameRate::FPS_30)),
        ("live", Some(FrameRate::FPS_60)),
    ];
    for &(scene, capture) in scenes {
        for (mode, every_tick, scroll_channel) in [
            ("every_tick", true, false),
            ("on_change", false, false),
            ("on_change_scroll_channel", false, true),
        ] {
            let mut r = rig(&ctx, capture, every_tick, scroll_channel);
            group.bench_function(format!("{scene}/{mode}"), |b| b.iter(|| r.tick()));
        }
    }
    group.finish();
}

criterion_group!(benches, ticks);
criterion_main!(benches);
