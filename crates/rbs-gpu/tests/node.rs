//! The compositor node on the graph runtime (spec `gpu-compositor.md`, "Graph runtime and
//! compositor node"): two source nodes feed a compositor, a probe keeps its output.
//!
//! Its own test binary with one shared context: the runtime needs an `Arc<GpuContext>`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use common::{assert_close, at};

#[path = "node/hold.rs"]
mod hold;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame, next_generation,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{
    CompositorNode, CompositorStats, Fade, GpuRuntime, GroupSpan, LayerList, LayerSender,
    NodeLayer, PixelCrop, RenderLocal,
};
use rbs_gpu::{
    CanvasDesc, GpuContext, GpuError, GpuRequest, LayerScroll, ScrollBinding, ScrollDirection,
    ScrollRepeat, ScrollRun, ScrollSender, ScrollStart, TextureDesc, readback,
};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, WarmCtx, channel,
};
use std::time::Duration;

const W: u32 = 64;
const H: u32 = 32;

/// A source publishing one 2x2 pool texture of a solid colour (sRGB bytes): a new frame
/// each tick (a live source), or, as a still, once and again only when `repaint` is set.
struct Solid {
    desc: NodeDescriptor,
    rgba: [u8; 4],
    id: Option<rbs_core::media::TextureId>,
    still: Option<Arc<AtomicBool>>,
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
            still: None,
        }
    }

    /// A still that publishes on its first tick and whenever `repaint` is set.
    fn still(rgba: [u8; 4], repaint: Arc<AtomicBool>) -> Self {
        Self {
            still: Some(repaint),
            ..Self::new(rgba)
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
        if let Some(repaint) = &self.still {
            // The slot keeps the frame; a repaint is a new generation (ADR-0117).
            let first = outputs.video(0).is_none();
            if !first && !repaint.swap(false, Ordering::Relaxed) {
                return Ok(());
            }
        }
        let frame = local.pool.frame(id, format, time).unwrap();
        drop(outputs.set_video(0, Some(frame.with_sequence(next_generation())))?);
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
        let red = Solid::new([255, 0, 0, 255]);
        let blue = Solid::new([0, 0, 255, 255]);
        self.scene_of(red, blue, capacity, false, false, layers)
    }

    /// [`Self::scene`] with still sources: `repaint` makes red publish once more.
    fn still_scene(
        &mut self,
        every_tick: bool,
        layers: impl FnOnce(NodeId, NodeId) -> Vec<NodeLayer>,
    ) -> (Scene, Arc<AtomicBool>) {
        let repaint = Arc::new(AtomicBool::new(false));
        let red = Solid::still([255, 0, 0, 255], Arc::clone(&repaint));
        let blue = Solid::still([0, 0, 255, 255], Arc::new(AtomicBool::new(false)));
        (
            self.scene_of(red, blue, 4, every_tick, false, layers),
            repaint,
        )
    }

    /// [`Self::still_scene`] whose compositor has scroll records (ADR-0150).
    fn scroll_scene(&mut self, layers: impl FnOnce(NodeId, NodeId) -> Vec<NodeLayer>) -> Scene {
        let red = Solid::still([255, 0, 0, 255], Arc::new(AtomicBool::new(false)));
        let blue = Solid::still([0, 0, 255, 255], Arc::new(AtomicBool::new(false)));
        self.scene_of(red, blue, 4, false, true, layers)
    }

    fn scene_of(
        &mut self,
        red: Solid,
        blue: Solid,
        capacity: u32,
        every_tick: bool,
        scroll: bool,
        layers: impl FnOnce(NodeId, NodeId) -> Vec<NodeLayer>,
    ) -> Scene {
        let red = self.add(Box::new(red));
        let blue = self.add(Box::new(blue));
        let (node, sender) = CompositorNode::new(
            CanvasDesc {
                width: W,
                height: H,
            },
            capacity,
            layers(red, blue),
        );
        assert_eq!(node.capacity(), capacity);
        let stats = node.stats();
        let (node, scroll) = if scroll {
            let (node, mut sender) = node.with_scroll();
            sender.send(Some(ScrollBinding::new(&self.ctx)));
            (node, Some(sender))
        } else {
            (node, None)
        };
        let node: Box<dyn Node<GpuRuntime>> = if every_tick {
            Box::new(node.every_tick())
        } else {
            Box::new(node)
        };
        let comp = self.add(node);
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
            _scroll: scroll,
            last,
            stats,
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
        at(&self.canvas(scene), W, x, y)
    }

    /// Every pixel of the last frame the probe saw, linear RGBA.
    fn canvas(&mut self, scene: &Scene) -> Vec<[f32; 4]> {
        let guard = scene.last.lock().unwrap();
        let frame = guard.as_ref().expect("the probe saw a canvas");
        assert_eq!(frame.format().pixel_format, PixelFormat::Rgba16Float);
        assert_eq!(frame.format().color, ColorSpace::LINEAR_BT709);
        let FramePayload::Texture(canvas) = *frame.payload() else {
            panic!("canvas is a pool texture");
        };
        readback::read_rgba16f(&self.ctx, &self.exec.local().pool, canvas).unwrap()
    }
}

struct Scene {
    red: NodeId,
    blue: NodeId,
    comp: NodeId,
    probe: NodeId,
    sender: LayerSender,
    _scroll: Option<ScrollSender>,
    last: Arc<Mutex<Option<VideoFrame>>>,
    stats: Arc<CompositorStats>,
}

impl Scene {
    /// `(timestamp, sequence)` of the canvas the probe saw last.
    fn key(&self) -> (Timestamp, u64) {
        let guard = self.last.lock().unwrap();
        let frame = guard.as_ref().expect("the probe saw a canvas");
        (frame.timestamp(), frame.sequence())
    }

    /// Draws and unchanged ticks so far.
    fn counts(&self) -> (u64, u64) {
        (
            self.stats.drawn.load(Ordering::Relaxed),
            self.stats.unchanged.load(Ordering::Relaxed),
        )
    }
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
            easing: rbs_gpu::node::Easing::Linear,
        });
        vec![layer]
    });
    rig.run(&scene, 4);
    // Premultiplied red at opacity 0.5 over a black canvas.
    assert_close(rig.pixel(&scene, 8, 16), [0.5, 0.0, 0.0, 0.5], 0.01);
    rig.run(&scene, 4); // 116 ms: done.
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
}

#[test]
fn unchanged_inputs_keep_the_canvas_frame() {
    let Some(mut rig) = Rig::new() else { return };
    let (scene, _) = rig.still_scene(false, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    // The first tick warms the nodes; the second draws.
    rig.run(&scene, 2);
    assert_eq!(scene.counts(), (1, 0));
    let first = scene.key();
    rig.run(&scene, 5);
    // No more draws; the canvas frame stays with its timestamp and sequence (R-03).
    assert_eq!(scene.counts(), (1, 5));
    assert_eq!(scene.key(), first);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);
}

#[test]
fn a_new_picture_draws_again_with_a_new_key() {
    let Some(mut rig) = Rig::new() else { return };
    let (scene, repaint) =
        rig.still_scene(false, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 3);
    assert_eq!(scene.counts(), (1, 1));
    let before = scene.key();
    repaint.store(true, Ordering::Relaxed);
    rig.run(&scene, 1);
    assert_eq!(scene.counts(), (2, 1));
    assert_ne!(scene.key().1, before.1, "a new draw is a new generation");
    rig.run(&scene, 2);
    assert_eq!(scene.counts(), (2, 3));
}

#[test]
fn a_new_layer_list_draws_again() {
    let Some(mut rig) = Rig::new() else { return };
    let (mut scene, _) =
        rig.still_scene(false, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 3);
    assert_eq!(scene.counts(), (1, 1));
    let (red, blue) = (scene.red, scene.blue);
    assert!(
        scene
            .sender
            .send(vec![full(blue, 1), right_half(red, 0)])
            .is_none()
    );
    rig.run(&scene, 2);
    assert_eq!(scene.counts(), (2, 2));
    assert_close(rig.pixel(&scene, 8, 16), BLUE, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), RED, 0.01);
}

#[test]
fn a_fade_draws_every_tick_until_it_has_ended() {
    let Some(mut rig) = Rig::new() else { return };
    // Ticks at 60 fps from zero; the fade ends at 100 ms (tick 6).
    let (scene, _) = rig.still_scene(false, |red, _| {
        let mut layer = full(red, 0);
        layer.fade = Some(Fade {
            from: 0.0,
            start: Timestamp::ZERO,
            duration: Duration::from_millis(100),
            easing: rbs_gpu::node::Easing::Linear,
        });
        vec![layer]
    });
    // Ticks 0 to 3: the last is at 50 ms, half way.
    rig.run(&scene, 4);
    assert_eq!(scene.counts(), (3, 0));
    assert_close(rig.pixel(&scene, 8, 16), [0.5, 0.0, 0.0, 0.5], 0.01);
    // Ticks 4 to 6 still move it; tick 6 draws the end value at 100 ms.
    rig.run(&scene, 3);
    assert_eq!(scene.counts(), (6, 0));
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    // Tick 7 draws once more (the last draw was at the end), then it stays.
    rig.run(&scene, 5);
    assert_eq!(scene.counts(), (7, 4));
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
}

#[test]
fn every_tick_draws_each_tick() {
    let Some(mut rig) = Rig::new() else { return };
    let (scene, _) = rig.still_scene(true, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 5);
    assert_eq!(scene.counts(), (4, 0));
}

#[test]
fn a_live_source_draws_each_tick() {
    let Some(mut rig) = Rig::new() else { return };
    let scene = rig.scene(4, |red, blue| vec![full(red, 0), right_half(blue, 1)]);
    rig.run(&scene, 5);
    assert_eq!(scene.counts(), (4, 0));
}

/// A full-canvas red layer scrolling left as `run` says, `repeat` copies.
fn scrolling(red: NodeId, repeat: ScrollRepeat, run: ScrollRun) -> NodeLayer {
    let mut layer = full(red, 0);
    layer.scroll = Some(LayerScroll {
        direction: ScrollDirection::Left,
        repeat,
        start: ScrollStart::Inside,
        gap: 32.0,
        fade: 0.0,
        run,
    });
    layer
}

#[test]
fn a_moving_scroll_draws_every_tick() {
    let Some(mut rig) = Rig::new() else { return };
    let scene = rig.scroll_scene(|red, _| {
        vec![scrolling(
            red,
            ScrollRepeat::Loop,
            ScrollRun::start(Timestamp::ZERO, 60.0),
        )]
    });
    rig.run(&scene, 4);
    assert_eq!(scene.counts(), (3, 0));
    rig.run(&scene, 5);
    assert_eq!(scene.counts(), (8, 0));
}

#[test]
fn a_scroll_draws_its_picture_and_gap() {
    let Some(mut rig) = Rig::new() else { return };
    // The 2 x 2 picture is a fill as long as the 64 wide box (ADR-0177), then a 32
    // gap; held 48 along: 16 of the picture, the gap, then the next copy.
    let scene = rig.scroll_scene(|red, _| {
        vec![scrolling(
            red,
            ScrollRepeat::Loop,
            ScrollRun {
                position: 48.0,
                ..ScrollRun::start(Timestamp::ZERO, 0.0)
            },
        )]
    });
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 32, 16), [0.0; 4], 0.01);
    assert_close(rig.pixel(&scene, 56, 16), RED, 0.01);
}

#[test]
fn a_paused_scroll_lets_render_on_change_skip_again() {
    let Some(mut rig) = Rig::new() else { return };
    // Pauses at 50 ms (tick 3) at once: tick 3 is the last that moves.
    let run = ScrollRun::start(Timestamp::ZERO, 60.0).change_speed(
        Timestamp::from_nanos(50_000_000),
        0.0,
        Duration::ZERO,
    );
    let scene = rig.scroll_scene(|red, _| vec![scrolling(red, ScrollRepeat::Loop, run)]);
    rig.run(&scene, 10);
    let (drawn, unchanged) = scene.counts();
    assert!(drawn <= 5, "drew {drawn}");
    assert!(unchanged >= 4, "skipped {unchanged}");
}

#[test]
fn a_finished_once_scroll_draws_nothing_and_stops_drawing() {
    let Some(mut rig) = Rig::new() else { return };
    // A fill 64 long (the box, ADR-0177) at 1920 per second: gone at 33.3 ms, between
    // ticks 2 and 3.
    let scene = rig.scroll_scene(|red, _| {
        vec![scrolling(
            red,
            ScrollRepeat::Once,
            ScrollRun::start(Timestamp::ZERO, 1920.0),
        )]
    });
    rig.run(&scene, 10);
    let (drawn, unchanged) = scene.counts();
    assert!(drawn <= 5, "drew {drawn}");
    assert!(unchanged >= 4, "skipped {unchanged}");
    assert_close(rig.pixel(&scene, 8, 16), [0.0; 4], 0.01);
}

/// A group Scroll held at zero (ADR-0193): its picture is exactly the window.
fn still_group() -> LayerScroll {
    LayerScroll {
        direction: ScrollDirection::Left,
        repeat: ScrollRepeat::Loop,
        start: ScrollStart::Inside,
        gap: 0.0,
        fade: 0.0,
        run: ScrollRun::start(Timestamp::ZERO, 0.0),
    }
}

fn span(first: u32, count: u32, window: [f32; 4], scroll: LayerScroll) -> GroupSpan {
    GroupSpan {
        first,
        count,
        window,
        scroll,
        offset: [0.0; 2],
        slide: None,
        tag: 0,
    }
}

impl Scene {
    fn send_spans(&mut self, layers: Vec<NodeLayer>, spans: Vec<GroupSpan>) {
        let _ = self.sender.send_list(LayerList { layers, spans });
    }

    /// Span passes, acquires and failures so far.
    fn span_counts(&self) -> (u64, u64, u64) {
        let s = &self.stats;
        (
            s.span_passes.load(Ordering::Relaxed),
            s.span_acquires.load(Ordering::Relaxed),
            s.span_failures.load(Ordering::Relaxed),
        )
    }
}

/// Translucent, styled, overlapping members at fractional places; their union is
/// `[6.5, 4.25, 39.5, 20.25]`.
fn members(red: NodeId, blue: NodeId) -> Vec<NodeLayer> {
    let mut a = NodeLayer::new(red, 0, 6.5, 4.25, 20.0, 12.0);
    a.opacity = 0.6;
    let mut b = NodeLayer::new(blue, 1, 16.0, 8.5, 30.0, 16.0);
    b.opacity = 0.8;
    b.style = rbs_gpu::LayerStyle::new(
        rbs_gpu::LayerShape::Rounded { radius: 5.0 },
        2.0,
        [255, 255, 255, 255],
        [30.0, 16.0],
    );
    vec![a, b]
}

#[test]
fn a_span_at_zero_scroll_draws_its_members_as_drawn_directly() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scroll_scene(members);
    rig.run(&scene, 2);
    let direct = rig.canvas(&scene);
    assert_eq!(scene.span_counts(), (0, 0, 0), "no span, no intermediate");
    let layers = members(scene.red, scene.blue);
    scene.send_spans(
        layers,
        vec![span(0, 2, [6.5, 4.25, 39.5, 20.25], still_group())],
    );
    rig.run(&scene, 2);
    assert_eq!(scene.span_counts(), (1, 1, 0));
    let grouped = rig.canvas(&scene);
    // Parity inside the window; a styled member's soft edge past it is clipped.
    let inside = |i: usize| {
        let (x, y) = ((i as u32 % W) as f32 + 0.5, (i as u32 / W) as f32 + 0.5);
        (6.5..46.0).contains(&x) && (4.25..24.5).contains(&y)
    };
    for (i, (g, d)) in grouped.iter().zip(&direct).enumerate() {
        let d = if inside(i) { *d } else { [0.0; 4] };
        for c in 0..4 {
            assert!(
                (g[c] - d[c]).abs() <= 1.0 / 1024.0,
                "pixel {i}: grouped {g:?}, direct {d:?}"
            );
        }
    }
}

#[test]
fn a_moving_group_scroll_redraws_the_canvas_but_reuses_its_intermediate() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scroll_scene(|red, _| vec![full(red, 0)]);
    rig.run(&scene, 3);
    let before = rig.exec.local().pool.allocated();
    let mut moving = still_group();
    moving.run = ScrollRun::start(Timestamp::ZERO, 60.0);
    let (red, blue) = (scene.red, scene.blue);
    let window = [8.0, 4.0, 32.0, 16.0];
    let list = || vec![full(red, 0), NodeLayer::new(blue, 1, 8.0, 4.0, 32.0, 16.0)];
    scene.send_spans(list(), vec![span(1, 1, window, moving)]);
    let (drawn, _) = scene.counts();
    rig.run(&scene, 6);
    // Still members: one acquire, one span pass; the moving Scroll draws every tick.
    assert_eq!(scene.span_counts(), (1, 1, 0));
    assert_eq!(scene.counts().0, drawn + 6);
    let shown = rig.exec.local().pool.allocated();
    assert_eq!(shown, before + 1, "one intermediate");

    // A size change gives the old one back and takes one new one.
    scene.send_spans(list(), vec![span(1, 1, [8.0, 4.0, 16.0, 16.0], moving)]);
    rig.run(&scene, 2);
    assert_eq!(scene.span_counts(), (2, 2, 0));
    assert_eq!(rig.exec.local().pool.allocated(), shown);

    // Leaving the list gives it back at once.
    let _ = scene.sender.send(list());
    rig.run(&scene, 2);
    assert_eq!(rig.exec.local().pool.allocated(), shown - 1);

    // `cool` gives it back when the compositor leaves the schedule.
    scene.send_spans(list(), vec![span(1, 1, window, moving)]);
    rig.run(&scene, 2);
    assert_eq!(scene.span_counts(), (3, 3, 0));
    assert_eq!(rig.exec.local().pool.allocated(), shown);
    rig.graph.set_state(scene.comp, Lifecycle::Standby).unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    rig.tick();
    rig.tick();
    assert_eq!(rig.exec.local().pool.allocated(), shown - 1);
}

#[test]
fn a_failed_or_oversized_group_draws_transparent_and_the_rest_draws() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scroll_scene(|red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    let (red, blue) = (scene.red, scene.blue);
    let list = || {
        vec![
            NodeLayer::new(red, 0, 0.0, 0.0, 32.0, H as f32),
            right_half(blue, 1),
        ]
    };
    // A side past the device's limit: the acquire fails with a typed error.
    let max = rig.ctx.device().limits().max_texture_dimension_2d as f32;
    scene.send_spans(
        list(),
        vec![span(1, 1, [32.0, 0.0, 1.0, max + 1.0], still_group())],
    );
    rig.run(&scene, 3);
    let (passes, acquires, failures) = scene.span_counts();
    assert_eq!(passes, 0);
    assert!(acquires >= 1 && failures >= 1, "{:?}", scene.span_counts());
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), [0.0; 4], 0.01);

    // Over 8,388,608 pixels: no acquire at all.
    scene.send_spans(
        list(),
        vec![span(1, 1, [0.0, 0.0, 4096.0, 4096.0], still_group())],
    );
    rig.run(&scene, 2);
    let (_, after, more) = scene.span_counts();
    assert!(
        after - acquires <= 1,
        "at most the retry before the new list"
    );
    assert!(more > failures);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);
    assert_close(rig.pixel(&scene, 56, 16), [0.0; 4], 0.01);
}

#[test]
fn span_failing_bits_name_the_failing_slot_and_clear_when_it_recovers() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scroll_scene(|red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    let (red, blue) = (scene.red, scene.blue);
    let list = || {
        vec![
            NodeLayer::new(red, 0, 0.0, 0.0, 32.0, H as f32),
            right_half(blue, 1),
        ]
    };
    let failing = |scene: &Scene| {
        (
            scene.stats.span_acquire_failing.load(Ordering::Relaxed),
            scene.stats.span_pass_failing.load(Ordering::Relaxed),
        )
    };
    let max = rig.ctx.device().limits().max_texture_dimension_2d as f32;
    let good = span(0, 1, [0.0, 0.0, 32.0, H as f32], still_group());
    // Slot 1's acquire fails; slot 0 draws.
    scene.send_spans(
        list(),
        vec![good, span(1, 1, [32.0, 0.0, 1.0, max + 1.0], still_group())],
    );
    rig.run(&scene, 2);
    assert_eq!(failing(&scene), (0b10, 0));
    // A drawable list clears it.
    scene.send_spans(
        list(),
        vec![good, span(1, 1, [32.0, 0.0, 32.0, H as f32], still_group())],
    );
    rig.run(&scene, 2);
    assert_eq!(failing(&scene), (0, 0));
    scene.send_spans(
        list(),
        vec![span(1, 1, [32.0, 0.0, 1.0, max + 1.0], still_group())],
    );
    rig.run(&scene, 2);
    assert_eq!(failing(&scene), (0b1, 0));
    // So does a list without spans.
    let _ = scene.sender.send(list());
    rig.run(&scene, 2);
    assert_eq!(failing(&scene), (0, 0));
}

#[test]
fn a_moved_span_moves_its_picture_without_a_new_pass() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scroll_scene(|red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    let blue = scene.blue;
    let members = || vec![NodeLayer::new(blue, 1, 0.0, 0.0, 16.0, H as f32)];
    let window = [0.0, 0.0, 16.0, H as f32];
    scene.send_spans(members(), vec![span(0, 1, window, still_group())]);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), BLUE, 0.01);
    let (passes, _, _) = scene.span_counts();
    // Pushed out 32 px over 50 ms from the next tick: the members stay where they were
    // in the intermediate, only the span's layer moves.
    let mut moved = span(0, 1, window, still_group());
    moved.offset = [32.0, 0.0];
    moved.slide = Some(rbs_gpu::node::Slide {
        from: rbs_gpu::Affine2::rect(0.0, 0.0, 16.0, H as f32),
        start: Timestamp::from_nanos(rig.tick * 1_000_000_000 / 60),
        duration: Duration::from_millis(50),
        easing: rbs_gpu::node::Easing::Linear,
        from_crop: None,
    });
    scene.send_spans(members(), vec![moved]);
    let drawn = scene.counts().0;
    rig.run(&scene, 8);
    assert_eq!(
        scene.span_counts().0,
        passes + 1,
        "one pass for the new list"
    );
    assert!(
        scene.counts().0 >= drawn + 3,
        "the canvas redraws while it slides"
    );
    assert_close(rig.pixel(&scene, 8, 16), [0.0; 4], 0.01);
    assert_close(rig.pixel(&scene, 40, 16), BLUE, 0.01);
}
