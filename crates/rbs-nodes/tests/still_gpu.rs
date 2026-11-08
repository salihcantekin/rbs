//! Still source -> compositor -> video export on the render runtime (ADR-0074, spec
//! `io-nodes.md`, "Still source node"). Runs on any GPU adapter, software included;
//! skips when there is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{FramePayload, PixelFormat, Size, VideoFrame};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest, YuvFormat};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{StillError, StillSourceNode, VideoExportNode};

const W: u32 = 128;
const H: u32 = 72;

/// Keeps every new frame on its input.
struct Probe {
    desc: NodeDescriptor,
    frames: Arc<Mutex<Vec<VideoFrame>>>,
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
                self.frames.lock().unwrap().push(frame.share().unwrap());
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

/// The luma byte at the centre of an NV12 system frame.
fn centre_luma(frame: &VideoFrame) -> u8 {
    let FramePayload::System(sys) = frame.payload() else {
        panic!("expected a system frame, got {:?}", frame.payload());
    };
    let (luma, stride) = sys.plane(0).unwrap();
    luma[(H as usize / 2) * stride + W as usize / 2]
}

#[test]
fn a_picture_with_the_wrong_size_is_refused() {
    assert_eq!(
        StillSourceNode::new(0, 4, Vec::new()).err(),
        Some(StillError::Empty)
    );
    assert_eq!(
        StillSourceNode::new(2, 2, vec![0; 15]).err(),
        Some(StillError::WrongLength)
    );
    let node = StillSourceNode::new(2, 1, vec![0; 8]).unwrap();
    assert_eq!(node.size(), (2, 1));
    assert_eq!(node.stats().picture_size(), Some((2, 1)));
}

#[test]
fn a_still_is_uploaded_once_and_stays_on_the_canvas() {
    let Some(ctx) = gpu() else { return };
    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
    let mut ctl: Controller<GpuRuntime> = ctl;
    let mut exec: Executor<GpuRuntime> = exec;
    let mut graph = GraphDescription::new();
    let add = |graph: &mut GraphDescription,
               ctl: &mut Controller<GpuRuntime>,
               node: Box<dyn Node<GpuRuntime>>| {
        let id = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        ctl.adopt(id, node).unwrap();
        id
    };
    // A white picture of its own size, scaled to the whole canvas by its layer.
    let still = StillSourceNode::new(4, 4, vec![255; 4 * 4 * 4]).unwrap();
    let stats = still.stats();
    let still = add(&mut graph, &mut ctl, Box::new(still));
    let (comp, _layers) = CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        4,
        vec![NodeLayer::new(still, 0, 0.0, 0.0, W as f32, H as f32)],
    );
    let comp = add(&mut graph, &mut ctl, Box::new(comp));
    let export = add(
        &mut graph,
        &mut ctl,
        Box::new(VideoExportNode::new(YuvFormat::Nv12, W, H)),
    );
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = add(
        &mut graph,
        &mut ctl,
        Box::new(Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            frames: Arc::clone(&frames),
            last: None,
        }),
    );
    graph.connect(still, "video", comp, "layers").unwrap();
    graph.connect(comp, "canvas", export, "canvas").unwrap();
    graph.connect(export, "video", probe, "in").unwrap();
    let report = ctl.apply(&graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    exec.swap_pending();
    let _ = ctl.collect();

    let clock = TestClock::new();
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    for i in 0..8 {
        let tick = Tick::of(&cadence, i);
        clock.set(tick.time);
        exec.run_tick(tick);
        ctx.wait_idle().unwrap();
        ctx.poll();
        let _ = ctl.collect();
    }
    // One upload and one publication, however many ticks ran (P13).
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
    let frames = frames.lock().unwrap();
    assert!(frames.len() >= 4, "only {} frames", frames.len());
    for frame in frames.iter() {
        assert_eq!(frame.format().pixel_format, PixelFormat::Nv12);
        assert_eq!(frame.format().size, Size::new(W, H));
        // White: limited-range luma 235.
        assert!(centre_luma(frame).abs_diff(235) <= 2);
    }
    assert_eq!(exec.health(export).unwrap().errors, 0);

    // Standby and back (the P13 contract): the texture goes back to the pool, and the
    // next activation uploads and publishes once more from the pixels the node kept.
    let mut run = |graph: &GraphDescription, ticks: std::ops::Range<u64>| {
        let report = ctl.apply(graph).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        exec.swap_pending();
        let _ = ctl.collect();
        for i in ticks {
            let tick = Tick::of(&cadence, i);
            clock.set(tick.time);
            exec.run_tick(tick);
            ctx.wait_idle().unwrap();
            ctx.poll();
            let _ = ctl.collect();
        }
    };
    graph.set_state(still, Lifecycle::Standby).unwrap();
    run(&graph, 8..10);
    assert_eq!(
        stats.frames.load(Ordering::Relaxed),
        1,
        "nothing while in Standby"
    );
    graph.set_state(still, Lifecycle::Active).unwrap();
    run(&graph, 10..14);
    assert_eq!(stats.uploaded.load(Ordering::Relaxed), 2);
    assert_eq!(stats.frames.load(Ordering::Relaxed), 2);
}
