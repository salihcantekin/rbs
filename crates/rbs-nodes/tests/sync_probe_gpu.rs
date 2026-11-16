//! Sync picture probe (ADR-0126 decision 3; spec `io-nodes.md`, "Sync picture probe"):
//! one motion value per new picture, stamped with the tick it first reached the probe,
//! nothing for a repeated picture. Runs on any GPU adapter, software included; skips
//! when there is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ColorSpace, PixelFormat, Size, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest, TextureDesc, TextureId};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::SyncPictureProbeNode;

const W: u32 = 64;
const H: u32 = 36;

/// Publishes a dark or a light picture: even generations dark, odd light.
struct Feed {
    desc: NodeDescriptor,
    textures: Option<[TextureId; 2]>,
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
        let textures = match self.textures {
            Some(t) => t,
            None => {
                let desc = TextureDesc::source(W, H, wgpu::TextureFormat::Rgba8Unorm);
                let mut make = |v: u8| {
                    let id = local.pool.acquire(gpu, &desc).unwrap();
                    rbs_gpu::readback::upload(gpu, &local.pool, id, &[v; (W * H * 4) as usize])
                        .unwrap();
                    id
                };
                let t = [make(0), make(255)];
                self.textures = Some(t);
                t
            }
        };
        let g = self.generation.load(Ordering::Relaxed);
        let format = VideoFormat::new(Size::new(W, H), PixelFormat::Rgba8, ColorSpace::SRGB);
        let frame = local
            .pool
            .frame(textures[(g % 2) as usize], format, Timestamp::ZERO)
            .unwrap()
            .with_sequence(g);
        drop(outputs.set_video(0, Some(frame))?);
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
fn only_new_pictures_are_measured_at_the_tick_they_arrive() {
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
    let generation = Arc::new(AtomicU64::new(0));
    let feed = add(Box::new(Feed {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.feed"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        ),
        textures: None,
        generation: Arc::clone(&generation),
    }));
    let (node, log) = SyncPictureProbeNode::new(Duration::from_secs(1));
    let probe = add(Box::new(node));
    graph.connect(feed, "video", probe, "picture").unwrap();
    let report = ctl.apply(&graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    exec.swap_pending();
    let _ = ctl.collect();

    // A 30 fps camera on a 60 fps canvas: a new picture every other tick.
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    for tick in 0..20u64 {
        if tick % 2 == 0 {
            generation.store(tick / 2, Ordering::Relaxed);
        }
        exec.run_tick(Tick::of(&cadence, tick));
        ctx.wait_idle().unwrap();
        ctx.poll();
        let _ = ctl.collect();
    }
    let mut got = Vec::new();
    log.read_from(0, &mut got);
    // Ten new pictures; the first has nothing to compare with, and the last value may
    // still wait for a later tick's take.
    assert!((8..=9).contains(&got.len()), "{got:?}");
    for (i, (t, m)) in got.iter().enumerate() {
        let tick = 2 * (i as u64 + 1);
        assert_eq!(
            *t,
            Tick::of(&cadence, tick).time.as_nanos() as i64,
            "value {i}"
        );
        // Dark and light alternate: every value is a full change.
        assert!((m - 1.0).abs() < 0.01, "value {i}: {m}");
    }
    assert_eq!((log.busy(), log.failed()), (0, 0));
}
