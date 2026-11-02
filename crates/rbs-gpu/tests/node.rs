//! The compositor node on the graph runtime (spec `gpu-compositor.md`, "Graph runtime and
//! compositor node"): two source nodes feed a compositor, a probe keeps its output.
//!
//! Its own test binary with its own context: the runtime needs an `Arc<GpuContext>`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::{Arc, Mutex};

use common::{assert_close, at};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, PixelCrop, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest, TextureDesc, readback};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, WarmCtx, channel,
};

const W: u32 = 64;
const H: u32 = 32;

/// A source publishing one 2x2 pool texture of a solid colour (sRGB bytes).
struct Solid {
    desc: NodeDescriptor,
    rgba: [u8; 4],
    id: Option<rbs_core::media::TextureId>,
}

impl Solid {
    fn new(rgba: [u8; 4]) -> Self {
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.solid"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            rgba,
            id: None,
        }
    }
}

impl Node<GpuRuntime> for Solid {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let desc = TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8Unorm);
        let id = ctx.local.pool.acquire(ctx.shared, &desc).unwrap();
        readback::upload(ctx.shared, &ctx.local.pool, id, &self.rgba.repeat(4)).unwrap();
        self.id = Some(id);
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let id = self.id.ok_or(NodeError::NotReady)?;
        let time = ctx.tick().time;
        let format = VideoFormat::new(Size::new(2, 2), PixelFormat::Rgba8, ColorSpace::SRGB);
        let (_, outputs, local) = ctx.parts();
        let frame = local.pool.frame(id, format, time).unwrap();
        drop(outputs.set_video(0, Some(frame))?);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// Keeps a shared copy of the last frame it received.
struct Probe {
    desc: NodeDescriptor,
    last: Arc<Mutex<Option<VideoFrame>>>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let frame = ctx.inputs().video(0, 0).and_then(VideoFrame::share);
        *self.last.lock().unwrap() = frame;
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

struct Rig {
    graph: GraphDescription,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    ctx: Arc<GpuContext>,
    tick: u64,
}

impl Rig {
    fn new() -> Option<Self> {
        let ctx = match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Arc::new(ctx),
            Err(GpuError::NoAdapter) => {
                assert!(
                    std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                    "RBS_REQUIRE_GPU is set but no adapter was found"
                );
                eprintln!("SKIPPED: no GPU adapter");
                return None;
            }
            Err(e) => panic!("{e}"),
        };
        let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        Some(Self {
            graph: GraphDescription::new(),
            ctl,
            exec,
            ctx,
            tick: 0,
        })
    }

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
        self.exec.run_tick(Tick::of(&cadence, self.tick));
        self.tick += 1;
    }
}

#[test]
fn compositor_node_draws_its_links_in_layer_order() {
    let Some(mut rig) = Rig::new() else { return };
    let red = rig.add(Box::new(Solid::new([255, 0, 0, 255])));
    let blue = rig.add(Box::new(Solid::new([0, 0, 255, 255])));
    // Link 0 = red, link 1 = blue (connection order). Red fills the canvas; blue covers
    // the right half, with the left half of its source cropped away (no visible change for
    // a solid source, but the crop path runs).
    let mut blue_layer = NodeLayer::new(1, W as f32 / 2.0, 0.0, W as f32 / 2.0, H as f32);
    blue_layer.crop = PixelCrop {
        left: 1,
        ..PixelCrop::default()
    };
    let comp = rig.add(Box::new(CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        vec![NodeLayer::new(0, 0.0, 0.0, W as f32, H as f32), blue_layer],
    )));
    let last = Arc::new(Mutex::new(None));
    let probe = rig.add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        last: Arc::clone(&last),
    }));
    rig.graph.connect(red, "out", comp, "layers").unwrap();
    rig.graph.connect(blue, "out", comp, "layers").unwrap();
    rig.graph.connect(comp, "canvas", probe, "in").unwrap();

    let report = rig.ctl.apply(&rig.graph).unwrap();
    assert!(
        report.plan.skipped.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    for _ in 0..4 {
        rig.tick();
    }
    rig.ctl.collect().unwrap();
    for id in [red, blue, comp, probe] {
        let health = rig.exec.health(id).unwrap();
        assert!(!health.bypassed && health.errors == 0, "{id:?}: {health:?}");
    }

    let frame = last.lock().unwrap().take().expect("the probe saw a canvas");
    assert_eq!(frame.format().pixel_format, PixelFormat::Rgba16Float);
    assert_eq!(frame.format().color, ColorSpace::LINEAR_BT709);
    let FramePayload::Texture(canvas) = *frame.payload() else {
        panic!("canvas is a pool texture");
    };
    let pool = &rig.exec.local().pool;
    let px = readback::read_rgba16f(&rig.ctx, pool, canvas).unwrap();
    assert_close(at(&px, W, 8, 16), [1.0, 0.0, 0.0, 1.0], 0.01);
    assert_close(at(&px, W, 56, 16), [0.0, 0.0, 1.0, 1.0], 0.01);
    assert!(
        rig.exec.local().pool.allocated() <= 2 + 3,
        "two sources and the warmed ring; steady ticks reuse it"
    );
}
