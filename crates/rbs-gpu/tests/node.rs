//! The compositor node on the graph runtime (spec `gpu-compositor.md`, "Graph runtime and
//! compositor node"): two source nodes feed a compositor, a probe keeps its output.
//!
//! Its own test binary with one shared context: the runtime needs an `Arc<GpuContext>`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::{Arc, Mutex, OnceLock};

use common::{assert_close, at};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{
    CompositorNode, Fade, GpuRuntime, LayerSender, NodeLayer, PixelCrop, RenderLocal,
};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest, TextureDesc, readback};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, WarmCtx, channel,
};
use std::time::Duration;

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
        // One device per test binary: software Vulkan drivers crash with several.
        static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
        let ctx = CTX.get_or_init(|| {
            match GpuContext::new(&GpuRequest {
                allow_software: true,
                ..GpuRequest::default()
            }) {
                Ok(ctx) => Some(Arc::new(ctx)),
                Err(GpuError::NoAdapter) => None,
                Err(e) => panic!("{e}"),
            }
        });
        let Some(ctx) = ctx.clone() else {
            assert!(
                std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                "RBS_REQUIRE_GPU is set but no adapter was found"
            );
            eprintln!("SKIPPED: no GPU adapter");
            return None;
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

impl Rig {
    /// Builds red and blue sources, a compositor with `layers(red, blue)` and `capacity`,
    /// and a probe; applies the graph.
    fn scene(
        &mut self,
        capacity: u32,
        layers: impl FnOnce(NodeId, NodeId) -> Vec<NodeLayer>,
    ) -> Scene {
        let red = self.add(Box::new(Solid::new([255, 0, 0, 255])));
        let blue = self.add(Box::new(Solid::new([0, 0, 255, 255])));
        let (node, sender) = CompositorNode::new(
            CanvasDesc {
                width: W,
                height: H,
            },
            capacity,
            layers(red, blue),
        );
        assert_eq!(node.capacity(), capacity);
        let comp = self.add(Box::new(node));
        let last = Arc::new(Mutex::new(None));
        let probe = self.add(Box::new(Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            last: Arc::clone(&last),
        }));
        self.graph.connect(red, "out", comp, "layers").unwrap();
        self.graph.connect(blue, "out", comp, "layers").unwrap();
        self.graph.connect(comp, "canvas", probe, "in").unwrap();
        let report = self.ctl.apply(&self.graph).unwrap();
        assert!(
            report.plan.skipped.is_empty() && report.failed.is_empty(),
            "{report:?}"
        );
        Scene {
            red,
            blue,
            comp,
            probe,
            sender,
            last,
        }
    }

    /// Runs `n` ticks and checks that no node failed.
    fn run(&mut self, scene: &Scene, n: u64) {
        for _ in 0..n {
            self.tick();
        }
        self.ctl.collect().unwrap();
        for id in [scene.red, scene.blue, scene.comp, scene.probe] {
            let health = self.exec.health(id).unwrap();
            assert!(!health.bypassed && health.errors == 0, "{id:?}: {health:?}");
        }
    }

    /// Linear RGBA of canvas pixel (x, y) in the last frame the probe saw.
    fn pixel(&mut self, scene: &Scene, x: u32, y: u32) -> [f32; 4] {
        let guard = scene.last.lock().unwrap();
        let frame = guard.as_ref().expect("the probe saw a canvas");
        assert_eq!(frame.format().pixel_format, PixelFormat::Rgba16Float);
        assert_eq!(frame.format().color, ColorSpace::LINEAR_BT709);
        let FramePayload::Texture(canvas) = *frame.payload() else {
            panic!("canvas is a pool texture");
        };
        let px = readback::read_rgba16f(&self.ctx, &self.exec.local().pool, canvas).unwrap();
        at(&px, W, x, y)
    }
}

struct Scene {
    red: NodeId,
    blue: NodeId,
    comp: NodeId,
    probe: NodeId,
    sender: LayerSender,
    last: Arc<Mutex<Option<VideoFrame>>>,
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

fn full(producer: NodeId, link: usize) -> NodeLayer {
    NodeLayer::new(producer, link, 0.0, 0.0, W as f32, H as f32)
}

fn right_half(producer: NodeId, link: usize) -> NodeLayer {
    NodeLayer::new(
        producer,
        link,
        W as f32 / 2.0,
        0.0,
        W as f32 / 2.0,
        H as f32,
    )
}

#[test]
fn compositor_node_draws_its_links_in_layer_order() {
    let Some(mut rig) = Rig::new() else { return };
    // Link 0 = red, link 1 = blue (connection order). Red fills the canvas; blue covers
    // the right half, with the left half of its source cropped away (no visible change for
    // a solid source, but the crop path runs).
    let scene = rig.scene(4, |red, blue| {
        let mut blue_layer = right_half(blue, 1);
        blue_layer.crop = PixelCrop {
            left: 1,
            ..PixelCrop::default()
        };
        vec![full(red, 0), blue_layer]
    });
    rig.run(&scene, 4);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);
    assert!(
        rig.exec.local().pool.allocated() <= 2 + 3,
        "two sources and the warmed ring; steady ticks reuse it"
    );
}

#[test]
fn a_sent_layer_list_replaces_the_running_one() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scene(4, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);

    // Swap the order: blue fills the canvas, red covers the right half.
    let (red, blue) = (scene.red, scene.blue);
    assert!(scene.sender.recycle().is_none(), "nothing replaced yet");
    assert!(
        scene
            .sender
            .send(vec![full(blue, 1), right_half(red, 0)])
            .is_none()
    );
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), BLUE, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), RED, 0.01);
    // The node handed the replaced list back for reuse.
    let old = scene
        .sender
        .recycle()
        .expect("the replaced list comes back");
    assert_eq!(old.len(), 2);
    assert_eq!(old[0].producer, red);
}

#[test]
fn a_stale_link_hint_finds_the_producer() {
    let Some(mut rig) = Rig::new() else { return };
    // Both hints point at the other link; the node looks the producers up.
    let scene = rig.scene(4, |red, blue| vec![full(red, 1), right_half(blue, 0)]);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);
}

#[test]
fn layers_past_capacity_are_not_drawn() {
    let Some(mut rig) = Rig::new() else { return };
    let scene = rig.scene(1, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 56, 16), RED, 0.01);
}

#[test]
fn a_fade_is_evaluated_at_tick_time() {
    let Some(mut rig) = Rig::new() else { return };
    // Ticks are at 60 fps from zero: tick 3 (the last of four) is at 50 ms. A fade from 0
    // to 1 over 100 ms starting at zero is half way there.
    let scene = rig.scene(4, |red, _| {
        let mut layer = full(red, 0);
        layer.fade = Some(Fade {
            from: 0.0,
            start: Timestamp::ZERO,
            duration: Duration::from_millis(100),
        });
        vec![layer]
    });
    rig.run(&scene, 4);
    // Premultiplied red at opacity 0.5 over a black canvas.
    assert_close(rig.pixel(&scene, 8, 16), [0.5, 0.0, 0.0, 0.5], 0.01);
    rig.run(&scene, 4); // 116 ms: done.
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
}
