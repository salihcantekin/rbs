//! Image track node (ADR-0117 decision 5; spec `io-nodes.md`, "Image track node"):
//! one readback per picture change, at most one per `MIN_IMAGE_INTERVAL`, coalesced
//! changes counted, nothing done for an unchanged picture. Runs on any GPU adapter,
//! software included; skips when there is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest, TextureDesc, TextureId};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::ImageTrackNode;

const W: u32 = 4;
const H: u32 = 2;

/// Publishes one straight sRGB picture; its key changes on the ticks `change` says.
struct Feed {
    desc: NodeDescriptor,
    texture: Option<TextureId>,
    /// The generation published now; the test bumps it to make a change.
    generation: Arc<AtomicU64>,
}

impl Node<GpuRuntime> for Feed {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        let id = match self.texture {
            Some(id) => id,
            None => {
                let mut desc = TextureDesc::source(W, H, wgpu::TextureFormat::Rgba8UnormSrgb);
                desc.usage |= wgpu::TextureUsages::COPY_SRC;
                let id = local.pool.acquire(gpu, &desc).unwrap();
                rbs_gpu::readback::upload(gpu, &local.pool, id, &[200; (W * H * 4) as usize])
                    .unwrap();
                self.texture = Some(id);
                id
            }
        };
        let format = VideoFormat::new(Size::new(W, H), PixelFormat::Rgba8, ColorSpace::SRGB);
        let frame = local
            .pool
            .frame(id, format, Timestamp::ZERO)
            .unwrap()
            .with_sequence(self.generation.load(Ordering::Relaxed));
        drop(outputs.set_video(0, Some(frame))?);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

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

#[test]
fn changes_are_taken_once_each_and_at_most_once_per_interval() {
    let Some(ctx) = gpu() else { return };
    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
    let mut ctl: Controller<GpuRuntime> = ctl;
    let mut exec: Executor<GpuRuntime> = exec;
    let mut graph = GraphDescription::new();
    let mut add = |node: Box<dyn Node<GpuRuntime>>| {
        let id = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        ctl.adopt(id, node).unwrap();
        id
    };
    let generation = Arc::new(AtomicU64::new(1));
    let feed = add(Box::new(Feed {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.feed"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        ),
        texture: None,
        generation: Arc::clone(&generation),
    }));
    let node = ImageTrackNode::new(W, H);
    let stats = node.stats();
    let track = add(Box::new(node));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        frames: Arc::clone(&frames),
        last: None,
    }));
    graph.connect(feed, "video", track, "picture").unwrap();
    graph.connect(track, "video", probe, "in").unwrap();
    let report = ctl.apply(&graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    exec.swap_pending();
    let _ = ctl.collect();

    // 60 fps: the 100 ms interval is six ticks.
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let mut next = 0;
    let mut run = |ticks: u64, change_each_tick: bool| {
        for _ in 0..ticks {
            if change_each_tick {
                generation.fetch_add(1, Ordering::Relaxed);
            }
            exec.run_tick(Tick::of(&cadence, next));
            next += 1;
            ctx.wait_idle().unwrap();
            ctx.poll();
            let _ = ctl.collect();
        }
    };
    // An unchanged picture: one image, then nothing more however long it runs.
    run(30, false);
    assert_eq!(stats.taken.load(Ordering::Relaxed), 1);
    assert_eq!(stats.coalesced.load(Ordering::Relaxed), 0);
    // A change on every tick for 60 ticks (one second): one image per interval, the
    // changes in between counted once per image they wait for.
    run(60, true);
    let taken = stats.taken.load(Ordering::Relaxed) - 1;
    assert!((9..=11).contains(&taken), "{taken}");
    assert!(stats.coalesced.load(Ordering::Relaxed) >= taken - 1);
    // Let the last readback through.
    run(2, false);
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len() as u64, stats.taken.load(Ordering::Relaxed));
    for pair in frames.windows(2) {
        // Stamped with the tick: at least the interval apart.
        let gap = pair[1].timestamp().as_nanos() - pair[0].timestamp().as_nanos();
        assert!(gap >= 100_000_000, "{gap}");
    }
    let FramePayload::System(sys) = frames[0].payload() else {
        panic!("not a system frame");
    };
    assert_eq!(sys.plane(0).unwrap().0, [200; (W * H * 4) as usize]);
    assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
}
