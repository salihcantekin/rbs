//! Compositor -> virtual camera node on the render runtime, with the in-process test
//! camera (spec `io-nodes.md`, "Virtual camera node"; ADR-0066 decision 4). Runs on any
//! GPU adapter, software included (lavapipe, WARP on CI).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use rbs_capture::{CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{MemoryKind, PixelFormat};
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, GpuError, GpuRequest};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::runtime::{Controller, Executor, Node, Tick, channel};
use rbs_nodes::{CaptureSourceNode, VirtualCameraNode, VirtualCameraNodeStats};
use rbs_vcam::conformance::Probe as _;
use rbs_vcam::{CameraFormat, VirtualCameraConfig, VirtualCameraFactory};
use rbs_vcam_test::{TestConsumer, TestVcamFactory};

const W: u32 = 128;
const H: u32 = 72;
/// The camera is smaller than the canvas, so the scaled conversion runs.
const CAM_W: u32 = 64;
const CAM_H: u32 = 36;

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

fn config(width: u32, height: u32, rate: FrameRate) -> VirtualCameraConfig {
    VirtualCameraConfig::new(
        CameraFormat::new(width, height, rate, PixelFormat::Nv12),
        MemoryKind::System,
    )
}

struct Rig {
    graph: GraphDescription,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    ctx: Arc<GpuContext>,
    clock: TestClock,
    cadence: Cadence,
    tick: u64,
    camera: Option<NodeId>,
    stats: Arc<VirtualCameraNodeStats>,
    consumer: Arc<TestConsumer>,
    _factory: TestVcamFactory,
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

    /// One render tick; the GPU is waited on afterwards so readbacks complete
    /// deterministically by the next tick.
    fn tick(&mut self) {
        let tick = Tick::of(&self.cadence, self.tick);
        self.clock.set(tick.time);
        self.exec.run_tick(tick);
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        self.tick += 1;
        let _ = self.ctl.collect();
    }

    fn stat(&self, f: impl Fn(&VirtualCameraNodeStats) -> &std::sync::atomic::AtomicU64) -> u64 {
        f(&self.stats).load(Ordering::Relaxed)
    }
}

/// capture -> compositor (canvas `W x H` at `canvas_rate`) -> virtual camera. The session
/// is opened with `session`, the node with `node` (they differ only to provoke refusals).
fn rig(
    canvas_rate: FrameRate,
    session: VirtualCameraConfig,
    node: VirtualCameraConfig,
    stalled: bool,
) -> Option<Rig> {
    let ctx = gpu()?;
    let factory = TestVcamFactory::new();
    factory.install().unwrap();
    let consumer = Arc::clone(factory.consumer());
    if stalled {
        consumer.stall();
    }
    let s = factory.open(&session).unwrap();
    let camera = VirtualCameraNode::new(s, node, W, H);
    let stats = camera.stats();

    let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
    let clock = TestClock::new();
    let mut rig = Rig {
        graph: GraphDescription::new(),
        ctl,
        exec,
        ctx,
        clock: clock.clone(),
        cadence: Cadence::frames(Timestamp::ZERO, canvas_rate),
        tick: 0,
        camera: None,
        stats,
        consumer,
        _factory: factory,
    };
    let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock))
        .with_frame_rate(canvas_rate);
    let capture = rig.add(Box::new(CaptureSourceNode::new(
        Arc::new(TestCaptureFactory::new()),
        request,
    )));
    let (comp, _layers) = CompositorNode::new(
        CanvasDesc {
            width: W,
            height: H,
        },
        4,
        vec![NodeLayer::new(capture, 0, 0.0, 0.0, W as f32, H as f32)],
    );
    let comp = rig.add(Box::new(comp));
    let camera = rig.add(Box::new(camera));
    rig.camera = Some(camera);
    rig.graph.connect(capture, "video", comp, "layers").unwrap();
    rig.graph.connect(comp, "canvas", camera, "canvas").unwrap();
    let report = rig.ctl.apply(&rig.graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    rig.exec.swap_pending();
    let _ = rig.ctl.collect();
    Some(rig)
}

/// Waits for the test camera's sender thread to deliver `n` frames (a hang guard, not a
/// timing assumption).
fn wait_delivered(consumer: &TestConsumer, n: usize) -> Vec<u64> {
    for _ in 0..50_000_000u64 {
        let got = consumer.delivered();
        if got.len() >= n {
            return got;
        }
        std::thread::yield_now();
    }
    panic!("the camera delivered {:?}", consumer.delivered());
}

#[test]
fn a_60_fps_canvas_feeds_a_30_fps_camera_every_other_tick() {
    let cam = config(CAM_W, CAM_H, FrameRate::FPS_30);
    let Some(mut rig) = rig(FrameRate::FPS_60, cam, cam, false) else {
        return;
    };
    for _ in 0..21 {
        rig.tick();
    }
    // Ticks 0, 2, ..., 18 are due (10 camera frames); tick 20's readback is still being
    // taken. Each due frame is sent, or skipped and counted.
    let sent = rig.stat(|s| &s.sent);
    let skipped = rig.stat(|s| &s.consumer_busy) + rig.stat(|s| &s.busy) + rig.stat(|s| &s.lost);
    assert_eq!(sent + skipped, 10, "sent {sent}, skipped {skipped}");
    assert!(sent >= 1);
    assert_eq!(rig.stat(|s| &s.refused), 0);
    assert_eq!(rig.stat(|s| &s.failed), 0);
    let got = wait_delivered(&rig.consumer, 1);
    // Newest wins, in order, and only even canvas sequence numbers were due.
    assert!(got.windows(2).all(|w| w[0] < w[1]), "{got:?}");
    assert!(got.iter().all(|seq| seq % 2 == 0), "{got:?}");
    assert_eq!(rig.exec.health(rig.camera.unwrap()).unwrap().errors, 0);
}

#[test]
fn a_stalled_consumer_skips_camera_frames_before_conversion_and_never_fails_a_tick() {
    let cam = config(CAM_W, CAM_H, FrameRate::FPS_30);
    let Some(mut rig) = rig(FrameRate::FPS_30, cam, cam, true) else {
        return;
    };
    for _ in 0..10 {
        rig.tick();
    }
    // Three slots fill (ticks 0-2, sent on ticks 1-3); from tick 3 on no slot is free,
    // so nothing more is converted.
    assert_eq!(rig.stat(|s| &s.sent), 3);
    assert_eq!(rig.stat(|s| &s.consumer_busy), 7);
    assert_eq!(rig.stat(|s| &s.busy), 0);
    assert_eq!(rig.stat(|s| &s.lost), 0);
    assert_eq!(rig.exec.health(rig.camera.unwrap()).unwrap().errors, 0);
    // Resumed, the sender takes the newest and drops the two older ones itself.
    rig.consumer.resume();
    let got = wait_delivered(&rig.consumer, 1);
    assert_eq!(got[0], 2, "{got:?}");
    rig.tick();
    rig.tick();
    assert!(rig.stat(|s| &s.sent) >= 4);
}

#[test]
fn a_frame_the_session_refuses_is_counted_never_an_error() {
    // The node converts to 32 x 18; the session was opened for 64 x 36.
    let session = config(CAM_W, CAM_H, FrameRate::FPS_30);
    let node = config(CAM_W / 2, CAM_H / 2, FrameRate::FPS_30);
    let Some(mut rig) = rig(FrameRate::FPS_30, session, node, false) else {
        return;
    };
    for _ in 0..6 {
        rig.tick();
    }
    assert_eq!(rig.stat(|s| &s.sent), 0);
    assert_eq!(rig.stat(|s| &s.refused), 5);
    assert_eq!(rig.exec.health(rig.camera.unwrap()).unwrap().errors, 0);
    // Refused frames free their slots: no conversion was skipped for want of one.
    assert_eq!(rig.stat(|s| &s.consumer_busy), 0);
}
