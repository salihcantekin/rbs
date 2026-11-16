//! Text source node on the render runtime (ADR-0129 decision 5, spec `io-nodes.md`,
//! "Text source node"). Runs on any GPU adapter, software included; skips when there
//! is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{TextFrame, TextNodeStats, TextSourceNode};
use rbs_text::TextPicture;

/// Time, generation, width and height of each new frame a probe saw.
type Seen = Arc<Mutex<Vec<(Timestamp, u64, u32, u32)>>>;

/// What `Rig::text` gives back.
type TextRig = (
    NodeId,
    ParamSender<Option<TextFrame>>,
    Arc<TextNodeStats>,
    Seen,
);

/// Keeps every new frame on its input.
struct Probe {
    desc: NodeDescriptor,
    frames: Seen,
    last: Option<(Timestamp, u64)>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if let Some(frame) = ctx.inputs().video(0, 0) {
            let key = (frame.timestamp(), frame.sequence());
            if self.last != Some(key) {
                self.last = Some(key);
                let size = frame.format().size;
                self.frames
                    .lock()
                    .unwrap()
                    .push((key.0, key.1, size.width, size.height));
            }
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

fn gpu() -> Option<Arc<GpuContext>> {
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
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

fn frame(request: u64, w: u32, h: u32) -> TextFrame {
    let picture = TextPicture::new(w, h, vec![255; (w * h * 4) as usize]).unwrap();
    TextFrame::new(request, 1, Arc::new(picture)).unwrap()
}

struct Rig {
    ctx: Arc<GpuContext>,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    graph: GraphDescription,
    cadence: Cadence,
    tick: u64,
}

impl Rig {
    fn new(ctx: Arc<GpuContext>) -> Self {
        let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        Self {
            ctx,
            ctl,
            exec,
            graph: GraphDescription::new(),
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30),
            tick: 0,
        }
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

    fn text(&mut self, initial: Option<TextFrame>) -> TextRig {
        let (node, tx) = TextSourceNode::new(initial);
        let stats = node.stats();
        let id = self.add(Box::new(node));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let probe = self.add(Box::new(Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            frames: Arc::clone(&frames),
            last: None,
        }));
        self.graph.connect(id, "video", probe, "in").unwrap();
        (id, tx, stats, frames)
    }

    fn apply(&mut self) {
        let report = self.ctl.apply(&self.graph).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        self.exec.swap_pending();
        let _ = self.ctl.collect();
    }

    fn ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
            self.ctx.wait_idle().unwrap();
            self.ctx.poll();
            let _ = self.ctl.collect();
        }
    }
}

#[test]
fn a_new_node_with_no_picture_publishes_nothing() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, _tx, stats, frames) = rig.text(None);
    rig.apply();
    rig.ticks(4);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 0);
    assert!(frames.lock().unwrap().is_empty());
}

#[test]
fn pictures_are_uploaded_in_place_once_each() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, mut tx, stats, frames) = rig.text(Some(frame(1, 64, 32)));
    rig.apply();
    rig.ticks(4);
    // `warm` uploaded the initial picture; the first tick published it.
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
    assert_eq!(stats.uploaded_request.load(Ordering::Relaxed), 1);
    assert_eq!(stats.picture_size(), Some((64, 32)));

    // Two edits: each uploads once, into whichever texture is free.
    tx.send(Some(frame(2, 64, 32)));
    rig.ticks(3);
    tx.send(Some(frame(3, 64, 32)));
    rig.ticks(3);
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 3);
    assert_eq!(stats.uploaded_request.load(Ordering::Relaxed), 3);
    assert_eq!(stats.upload_deferred.load(Ordering::Relaxed), 0);

    // A box size change re-reserves once.
    tx.send(Some(frame(4, 128, 40)));
    rig.ticks(2);
    assert_eq!(stats.picture_size(), Some((128, 40)));
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 4, "{frames:?}");
    let generations: std::collections::HashSet<u64> = frames.iter().map(|f| f.1).collect();
    assert_eq!(generations.len(), 4, "every picture has a new generation");
    assert_eq!((frames[3].2, frames[3].3), (128, 40));
}

#[test]
fn a_picture_over_the_caps_is_held_without_upload() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, mut tx, stats, _) = rig.text(Some(frame(1, 8, 8)));
    rig.apply();
    rig.ticks(2);
    tx.send(Some(frame(2, rbs_text::MAX_SIDE + 1, 1)));
    rig.ticks(2);
    assert_eq!(stats.over_cap.load(Ordering::Relaxed), 1);
    assert_eq!(stats.uploaded_request.load(Ordering::Relaxed), 1);
    assert_eq!(stats.picture_size(), Some((8, 8)));
}

#[test]
fn the_upload_budget_spreads_large_pictures_over_ticks() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    // Three 8 MiB boxes: two fit the 16 MiB budget of one tick, the third waits.
    let mut nodes: Vec<_> = (0..3).map(|_| rig.text(None)).collect();
    rig.apply();
    rig.ticks(1);
    for (_, tx, _, _) in &mut nodes {
        tx.send(Some(frame(7, 2048, 1024)));
    }
    rig.ticks(1);
    let done = |nodes: &[(NodeId, _, Arc<TextNodeStats>, _)]| {
        nodes
            .iter()
            .filter(|n| n.2.uploaded_request.load(Ordering::Relaxed) == 7)
            .count()
    };
    assert_eq!(done(&nodes), 2);
    rig.ticks(1);
    assert_eq!(done(&nodes), 3);
    let waited: u64 = nodes
        .iter()
        .map(|n| n.2.budget_deferred.load(Ordering::Relaxed))
        .sum();
    assert_eq!(waited, 1);
}

#[test]
fn standby_gives_the_textures_back_and_warm_uploads_again() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (id, _tx, stats, _) = rig.text(Some(frame(1, 16, 16)));
    rig.apply();
    rig.ticks(2);
    rig.graph.set_state(id, Lifecycle::Standby).unwrap();
    rig.apply();
    rig.ticks(2);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
    rig.graph.set_state(id, Lifecycle::Active).unwrap();
    rig.apply();
    rig.ticks(2);
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 2);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 2);
}

/// Allocations of 100 steady ticks of a graph of `node` and a probe.
fn steady_allocations(ctx: Arc<GpuContext>, node: Box<dyn Node<GpuRuntime>>) -> u64 {
    let mut rig = Rig::new(ctx);
    let id = rig.add(node);
    let probe = rig.add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        frames: Arc::new(Mutex::new(Vec::new())),
        last: None,
    }));
    rig.graph.connect(id, "video", probe, "in").unwrap();
    rig.apply();
    rig.ticks(3);
    let start = rig.tick;
    allocation_counter::measure(|| {
        for i in start..start + 100 {
            rig.exec.run_tick(Tick::of(&rig.cadence, i));
        }
    })
    .count_total
}

/// A text node with nothing new adds no allocation to a tick: the GPU executor's own
/// per-tick work is measured with a still source in its place.
#[test]
fn steady_state_text_ticks_do_not_allocate() {
    let Some(ctx) = gpu() else { return };
    let still = rbs_nodes::StillSourceNode::new(32, 32, vec![255; 32 * 32 * 4]).unwrap();
    let baseline = steady_allocations(Arc::clone(&ctx), Box::new(still));
    let (text, _tx) = TextSourceNode::new(Some(frame(1, 32, 32)));
    let stats = text.stats();
    let with_text = steady_allocations(ctx, Box::new(text));
    assert_eq!(with_text, baseline, "the text node allocated");
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
}
