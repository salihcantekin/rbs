//! Capture source -> compositor -> video export in native mode -> an encoder-kind probe
//! (ADR-0147; spec `io-nodes.md`, "Video export node", native mode). The export hands out
//! exported native frames, never system memory: IOSurfaces on Metal (macOS), shared D3D12
//! buffers with a timeline fence on D3D12 (Windows). Its ring keeps up while the consumer
//! holds frames like an encoder does. Elsewhere native mode has no exporter.

#![cfg(any(target_vendor = "apple", windows))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{FramePayload, MemoryKind, NativeSurface, PixelFormat, VideoFrame};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest, YuvFormat};
use rbs_graph::description::{GraphDescription, GraphError, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{
    CaptureSourceNode, ExportEnd, LeaseOwners, NativeExport, VideoExportNode, export_preference,
};

const W: u32 = 128;
const H: u32 = 72;
/// Frames the probe keeps, like an encoder holding frames after `submit`.
const HELD: usize = 3;

#[cfg(target_vendor = "apple")]
const EXPECTED: MemoryKind = MemoryKind::Metal;
#[cfg(windows)]
const EXPECTED: MemoryKind = MemoryKind::D3d12Buffer;

/// What is wrong with the frame's surface for this platform's exporter, if anything.
#[cfg(target_vendor = "apple")]
fn wrong_surface(frame: &VideoFrame) -> Option<String> {
    (!matches!(
        frame.payload(),
        FramePayload::Native {
            surface: NativeSurface::IoSurface { .. },
            ..
        }
    ))
    .then(|| "not an IOSurface".into())
}

/// What is wrong with the frame's surface for this platform's exporter, if anything:
/// a shared buffer with the chroma rows right after the luma rows (what NVENC reads)
/// and a timeline fence.
#[cfg(windows)]
fn wrong_surface(frame: &VideoFrame) -> Option<String> {
    use rbs_core::media::SurfaceSync;
    let FramePayload::Native {
        surface: NativeSurface::D3dSharedBuffer { handle, planes, .. },
        sync,
        ..
    } = frame.payload()
    else {
        return Some("not a shared D3D12 buffer".into());
    };
    let h = frame.format().size.height as usize;
    if *handle == 0 || planes[0].offset != 0 || planes[1].offset != planes[0].stride * h {
        return Some(format!("layout {planes:?}"));
    }
    (!matches!(sync, SurfaceSync::TimelineFence { .. })).then(|| format!("sync {sync:?}"))
}

#[derive(Default)]
struct Seen {
    frames: u64,
    times: Vec<Timestamp>,
    wrong: Vec<String>,
}

/// An encoder-kind node that keeps the last [`HELD`] frames (shared copies) and checks
/// each new one.
struct Probe {
    desc: NodeDescriptor,
    held: VecDeque<VideoFrame>,
    seen: Arc<Mutex<Seen>>,
    last: Option<(Timestamp, u64)>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        self.held.reserve(HELD + 1);
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let Some(frame) = ctx.inputs().video(0, 0) else {
            return Ok(());
        };
        let key = (frame.timestamp(), frame.sequence());
        if self.last == Some(key) {
            return Ok(());
        }
        self.last = Some(key);
        let mut seen = self.seen.lock().unwrap();
        seen.frames += 1;
        seen.times.push(frame.timestamp());
        if !frame.is_exported() {
            seen.wrong
                .push(format!("not exported: {:?}", frame.payload()));
        }
        if frame.memory_kind() != EXPECTED {
            seen.wrong.push(format!("memory {:?}", frame.memory_kind()));
        }
        if let Some(what) = wrong_surface(frame) {
            seen.wrong.push(what);
        }
        let size = frame.format().size;
        if (size.width, size.height) != (W, H) {
            seen.wrong.push(format!("size {size:?}"));
        }
        match frame.share() {
            Some(copy) => {
                self.held.push_back(copy);
                if self.held.len() > HELD {
                    self.held.pop_front();
                }
            }
            None => seen.wrong.push("exported frame not shareable".into()),
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {
        self.held.clear();
    }
}

/// Takes the probe's packets, so the chain ends in an output and is scheduled.
struct Sink(NodeDescriptor);

impl Node<GpuRuntime> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, _: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

fn gpu() -> Option<Arc<GpuContext>> {
    match GpuContext::new(&GpuRequest::default()) {
        Ok(ctx) => Some(Arc::new(ctx)),
        Err(GpuError::NoAdapter) => {
            assert!(
                std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                "RBS_REQUIRE_GPU is set but no adapter was found"
            );
            eprintln!("SKIPPED: no GPU adapter");
            None
        }
        Err(e) => panic!("{e}"),
    }
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

    /// One 30 fps render tick, then the GPU is waited on so conversions complete by the
    /// next tick.
    fn tick(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let tick = Tick::of(&cadence, self.tick);
        self.clock.set(tick.time);
        self.exec.run_tick(tick);
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        self.tick += 1;
        let _ = self.ctl.collect();
    }

    /// The time of the last tick run.
    fn last_time(&self) -> Timestamp {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        Tick::of(&cadence, self.tick - 1).time
    }
}

struct Built {
    rig: Rig,
    seen: Arc<Mutex<Seen>>,
    end: Arc<ExportEnd>,
    owners: Arc<LeaseOwners>,
    export: NodeId,
}

fn probe_descriptor(kind: NodeKind) -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static("org.rbs.test.native-probe"),
        kind,
        vec![Pin::one("video", LinkKind::Video)],
        match kind {
            // An encoder has one packet output (graph shape rule).
            NodeKind::Encoder => vec![Pin::one("packets", LinkKind::Packets)],
            _ => vec![],
        },
    )
}

fn pipeline() -> Option<Built> {
    let ctx = gpu()?;
    let Some((kind, _device)) = export_preference(&ctx) else {
        eprintln!("SKIPPED: this device has no native export");
        return None;
    };
    if kind != EXPECTED {
        eprintln!("SKIPPED: this device exports {kind}, not {EXPECTED}");
        return None;
    }
    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
    let clock = TestClock::new();
    let mut rig = Rig {
        graph: GraphDescription::new(),
        ctl,
        exec,
        ctx,
        clock: clock.clone(),
        tick: 0,
    };
    let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock))
        .with_frame_rate(FrameRate::FPS_30)
        .with_pixel_formats(&[PixelFormat::Bgra8]);
    let capture = rig.add(Box::new(
        CaptureSourceNode::new(Arc::new(TestCaptureFactory::new()), request).open_inline(true),
    ));
    let (comp, _layers) = CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        4,
        vec![NodeLayer::new(capture, 0, 0.0, 0.0, W as f32, H as f32)],
    );
    let comp = rig.add(Box::new(comp));
    let owners = Arc::new(LeaseOwners::default());
    let export = VideoExportNode::new(YuvFormat::Nv12, W, H, W, H).native(NativeExport {
        encoder_frames: HELD,
        owners: Arc::clone(&owners),
    });
    assert!(export.is_native());
    let end = export.end();
    let export = rig.add(Box::new(export));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let probe = rig.add(Box::new(Probe {
        desc: probe_descriptor(NodeKind::Encoder),
        held: VecDeque::new(),
        seen: Arc::clone(&seen),
        last: None,
    }));
    let sink = rig.add(Box::new(Sink(NodeDescriptor::new(
        NodeTypeId::new_static("org.rbs.test.sink"),
        NodeKind::Output,
        vec![Pin::one("packets", LinkKind::Packets)],
        vec![],
    ))));
    rig.graph
        .connect(probe, "packets", sink, "packets")
        .unwrap();
    rig.graph.connect(capture, "video", comp, "layers").unwrap();
    rig.graph.connect(comp, "canvas", export, "canvas").unwrap();
    rig.graph.connect(export, "video", probe, "video").unwrap();
    let report = rig.ctl.apply(&rig.graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    rig.exec.swap_pending();
    let _ = rig.ctl.collect();
    Some(Built {
        rig,
        seen,
        end,
        owners,
        export,
    })
}

#[test]
fn a_native_export_hands_exported_surfaces_to_its_encoder_and_keeps_up() {
    let Some(mut b) = pipeline() else {
        return;
    };
    assert_eq!(
        b.owners.kept(),
        1,
        "prepare keeps its exporter's lease owner"
    );
    for _ in 0..60 {
        b.rig.tick();
    }
    let seen = b.seen.lock().unwrap();
    assert!(seen.wrong.is_empty(), "{:?}", seen.wrong);
    // The first tick only converts; every later tick delivers one frame although the
    // probe keeps three of them, so the ring is never starved.
    assert!(seen.frames >= 58, "{} frames", seen.frames);
    assert!(
        seen.times.windows(2).all(|w| w[0] < w[1]),
        "times go forward"
    );
}

#[test]
fn an_ended_native_export_passes_on_what_it_took_and_ends() {
    let Some(mut b) = pipeline() else {
        return;
    };
    for _ in 0..10 {
        b.rig.tick();
    }
    let at = b.rig.last_time();
    let before = b.seen.lock().unwrap().frames;
    b.end.end_at(at);
    for _ in 0..5 {
        b.rig.tick();
    }
    assert!(b.end.ended());
    let seen = b.seen.lock().unwrap();
    assert!(seen.wrong.is_empty(), "{:?}", seen.wrong);
    assert!(
        seen.frames > before,
        "the conversion in flight still goes out"
    );
    assert!(seen.times.iter().all(|t| *t <= at), "nothing after the end");
}

#[test]
fn a_native_export_links_only_to_one_encoder() {
    let Some(mut b) = pipeline() else {
        return;
    };
    let other = b
        .rig
        .graph
        .add_node(Arc::new(probe_descriptor(NodeKind::Output)))
        .unwrap();
    assert!(matches!(
        b.rig.graph.connect(b.export, "video", other, "video"),
        Err(GraphError::ExclusiveOutput(_))
    ));
    let second = b
        .rig
        .graph
        .add_node(Arc::new(probe_descriptor(NodeKind::Encoder)))
        .unwrap();
    assert!(matches!(
        b.rig.graph.connect(b.export, "video", second, "video"),
        Err(GraphError::ExclusiveOutput(_))
    ));
}
