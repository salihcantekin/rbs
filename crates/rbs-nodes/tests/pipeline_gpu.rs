//! Capture source -> compositor -> video export on the render runtime, with the
//! synthetic capture adapter (spec `io-nodes.md`, "Capture source node" and "Video export
//! node"). Runs on any GPU adapter, software included (lavapipe, WARP on CI).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::{Arc, Mutex, OnceLock};

use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{FramePayload, PixelFormat, Size, VideoFrame};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest, YuvFormat};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::capture::CaptureNodeStats;
use rbs_nodes::{CaptureSourceNode, ExportEnd, VideoExportNode};

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

type Frames = Arc<Mutex<Vec<VideoFrame>>>;

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

    /// One render tick at 30 fps without waiting for the GPU: its readback may still
    /// be in flight on later ticks.
    fn tick_without_waiting(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let tick = Tick::of(&cadence, self.tick);
        self.clock.set(tick.time);
        self.exec.run_tick(tick);
        self.tick += 1;
        let _ = self.ctl.collect();
    }

    /// One render tick at 30 fps with the capture clock at the tick's time; the GPU is
    /// waited on afterwards so readbacks complete deterministically by the next tick.
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
}

/// target -> capture -> compositor (one layer filling the canvas) -> export -> probe.
fn pipeline(
    target: &str,
    size: Option<Size>,
    formats: &[PixelFormat],
) -> Option<(Rig, Frames, NodeId, Arc<ExportEnd>)> {
    pipeline_from(Arc::new(TestCaptureFactory::new()), target, size, formats)
        .map(|(rig, frames, export, end, _)| (rig, frames, export, end))
}

/// A pipeline, its frames, the export node, its end and the capture node's counters.
type Built = (Rig, Frames, NodeId, Arc<ExportEnd>, Arc<CaptureNodeStats>);

/// [`pipeline`] capturing from `factory`; also returns the capture node's counters.
fn pipeline_from(
    factory: Arc<TestCaptureFactory>,
    target: &str,
    size: Option<Size>,
    formats: &[PixelFormat],
) -> Option<Built> {
    let ctx = gpu()?;
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
    let mut request = CaptureRequest::new(TargetId::new(target), Arc::new(clock))
        .with_frame_rate(FrameRate::FPS_30);
    if let Some(size) = size {
        request = request.with_size(size);
    }
    if !formats.is_empty() {
        request = request.with_pixel_formats(formats);
    }
    let capture = CaptureSourceNode::new(factory, request);
    let stats = capture.stats();
    let capture = rig.add(Box::new(capture));
    let (comp, _layers) = CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        4,
        vec![NodeLayer::new(capture, 0, 0.0, 0.0, W as f32, H as f32)],
    );
    let comp = rig.add(Box::new(comp));
    let export = VideoExportNode::new(YuvFormat::Nv12, W, H);
    let end = export.end();
    let export = rig.add(Box::new(export));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let probe = rig.add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        frames: Arc::clone(&frames),
        last: None,
    }));
    rig.graph.connect(capture, "video", comp, "layers").unwrap();
    rig.graph.connect(comp, "canvas", export, "canvas").unwrap();
    rig.graph.connect(export, "video", probe, "in").unwrap();
    let report = rig.ctl.apply(&rig.graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    rig.exec.swap_pending();
    let _ = rig.ctl.collect();
    Some((rig, frames, export, end, stats))
}

/// The luma and chroma bytes at the centre of an NV12 system frame.
fn centre(frame: &VideoFrame) -> (u8, u8, u8) {
    let FramePayload::System(sys) = frame.payload() else {
        panic!("expected a system frame, got {:?}", frame.payload());
    };
    let (luma, ls) = sys.plane(0).unwrap();
    let (chroma, cs) = sys.plane(1).unwrap();
    let (x, y) = (W as usize / 2, H as usize / 2);
    let c = (y / 2) * cs + (x / 2) * 2;
    (luma[y * ls + x], chroma[c], chroma[c + 1])
}

/// Limited-range BT.709 luma of an sRGB grey level: sRGB decode to linear light, then
/// the BT.709 OETF (what the converter writes, gpu-compositor spec).
fn expected_luma(grey: u8) -> u8 {
    let c = f64::from(grey) / 255.0;
    let linear = if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    };
    let v = if linear < 0.018 {
        4.5 * linear
    } else {
        1.099 * linear.powf(0.45) - 0.099
    };
    (16.0 + 219.0 * v).round() as u8
}

#[test]
fn a_window_is_captured_composited_and_exported_as_nv12() {
    let Some((mut rig, frames, export, _)) = pipeline("test-window-0", None, &[PixelFormat::Bgra8])
    else {
        return;
    };
    for _ in 0..6 {
        rig.tick();
    }
    let frames = frames.lock().unwrap();
    // The readback of tick i is delivered on tick i + 1.
    assert!(frames.len() >= 4, "only {} frames", frames.len());
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    for (i, frame) in frames.iter().enumerate() {
        let format = frame.format();
        assert_eq!(format.pixel_format, PixelFormat::Nv12);
        assert_eq!(format.size, Size::new(W, H));
        assert_eq!(frame.timestamp(), Tick::of(&cadence, i as u64).time);
        assert_eq!(frame.sequence(), i as u64);
        // A grey sRGB window: limited-range luma near the grey level, neutral chroma.
        let (y, cb, cr) = centre(frame);
        let near = [32u8, 96, 160]
            .map(expected_luma)
            .iter()
            .any(|e| e.abs_diff(y) <= 2);
        assert!(near, "luma {y}");
        assert!(
            cb.abs_diff(128) <= 2 && cr.abs_diff(128) <= 2,
            "chroma {cb} {cr}"
        );
    }
    let health = rig.exec.health(export).unwrap();
    assert_eq!(health.errors, 0, "{health:?}");
}

#[test]
fn a_camera_in_system_nv12_is_uploaded_decoded_and_exported() {
    let Some((mut rig, frames, _, _)) = pipeline("test-camera-0", Some(Size::new(640, 480)), &[])
    else {
        return;
    };
    for _ in 0..4 {
        rig.tick();
    }
    let frames = frames.lock().unwrap();
    assert!(frames.len() >= 2, "only {} frames", frames.len());
    for frame in frames.iter() {
        assert_eq!(frame.format().pixel_format, PixelFormat::Nv12);
        let (y, _, _) = centre(frame);
        // The test camera's pattern is a grey level of 32, 96 or 160 in every byte.
        assert!((16..=235).contains(&y), "luma {y}");
    }
}

#[test]
fn an_ended_export_passes_on_every_canvas_up_to_the_end_and_none_after() {
    let Some((mut rig, frames, export, end)) =
        pipeline("test-window-0", None, &[PixelFormat::Bgra8])
    else {
        return;
    };
    for _ in 0..3 {
        rig.tick();
    }
    // End at the time of tick 5 before ticks 3 to 5 run, so the export is ending (and
    // passes frames oldest first) while they do. They run without waiting for the GPU,
    // so readbacks may still be in flight at the end and several may complete in the
    // same poll (on the run path the older ones would be dropped as stale; `take_oldest`
    // is checked on its own in rbs-gpu). Three stay within the ring: none is busy.
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    let last = cadence.time_of(5);
    end.end_at(last);
    assert_eq!(end.requested(), Some(last));
    for _ in 0..3 {
        rig.tick_without_waiting();
    }
    assert!(!end.ended(), "no tick past the end ran yet");
    let mut ticks = 0;
    while !end.ended() {
        assert!(ticks < 8, "the export never ended");
        rig.tick();
        ticks += 1;
    }
    let delivered: Vec<Timestamp> = frames
        .lock()
        .unwrap()
        .iter()
        .map(|f| f.timestamp())
        .collect();
    let expected: Vec<Timestamp> = (0..6).map(|i| cadence.time_of(i)).collect();
    assert_eq!(delivered, expected, "every canvas up to the end, in order");
    // Later canvases are not taken.
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(frames.lock().unwrap().len(), 6);
    assert!(end.ended());
    let health = rig.exec.health(export).unwrap();
    assert_eq!(health.errors, 0, "{health:?}");
}

#[test]
fn the_capture_node_reports_its_picture_size_and_follows_a_resize() {
    let factory = Arc::new(TestCaptureFactory::new());
    let Some((mut rig, _, _, _, stats)) = pipeline_from(
        Arc::clone(&factory),
        "test-window-0",
        None,
        &[PixelFormat::Bgra8],
    ) else {
        return;
    };
    let first = stats.picture_size().expect("known once the session opened");
    rig.tick();
    assert_eq!(stats.picture_size(), Some(first));
    let (w, h) = (first.0 / 2, first.1 / 2);
    assert!(factory.resize_target(&TargetId::new("test-window-0"), w, h));
    // The new size arrives with the first frame captured after the resize.
    for _ in 0..4 {
        rig.tick();
        if stats.picture_size() == Some((w, h)) {
            return;
        }
    }
    panic!("still {:?} after the resize", stats.picture_size());
}
