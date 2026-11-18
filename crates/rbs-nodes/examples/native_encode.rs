//! Measures the encode path of one 1080p60 H.264 chain on macOS with system memory (GPU
//! readback, CPU copy into VideoToolbox's pool) and with native input (exported
//! IOSurfaces, no copy; ADR-0147): synthetic capture -> compositor -> video export ->
//! VideoToolbox encoder node -> sink, ticked at 60 fps in real time.
//!
//! Prints per mode: render-thread time per tick (mean, p95), process CPU time (user +
//! system, which includes VideoToolbox's own threads), frames and drops. A hardware
//! measurement (P15, ADR-0147), not a test.
//!
//! ```text
//! cargo run -p rbs-nodes --example native_encode --release -- [seconds per mode, default 5]
//! ```
//!
//! The adapter is a dev-dependency of this example only; the library never depends on
//! one (P1).

#![allow(clippy::unwrap_used, clippy::expect_used)] // A hardware check fails by panicking.

#[cfg(not(target_vendor = "apple"))]
fn main() {
    eprintln!("native_encode: this example measures VideoToolbox; run it on macOS.");
}

#[cfg(target_vendor = "apple")]
fn main() {
    mac::run();
}

#[cfg(target_vendor = "apple")]
mod mac {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use rbs_capture::{CaptureRequest, TargetId};
    use rbs_capture_test::TestCaptureFactory;
    use rbs_codec::{RateControl, VideoEncoderConfig};
    use rbs_codec_videotoolbox::VideoToolboxFactory;
    use rbs_core::lifecycle::Lifecycle;
    use rbs_core::media::{CodecId, MemoryKind, PixelFormat};
    use rbs_core::time::{TestClock, Timestamp};
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
    use rbs_gpu::{CanvasDesc, GpuContext, GpuRequest, YuvFormat};
    use rbs_graph::description::{GraphDescription, NodeId};
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
    use rbs_graph::runtime::{Controller, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};
    use rbs_nodes::{
        CaptureSourceNode, LeaseOwners, NativeExport, VideoEncoderNode, VideoExportNode,
        export_preference,
    };

    const W: u32 = 1920;
    const H: u32 = 1080;

    /// Takes the encoder's packets and lets them go.
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

    fn cpu_time() -> Duration {
        // SAFETY: `getrusage` writes into the zeroed struct we pass and reads nothing else.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            usage
        };
        let tv = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        tv(usage.ru_utime) + tv(usage.ru_stime)
    }

    fn add(
        graph: &mut GraphDescription,
        ctl: &mut Controller<GpuRuntime>,
        node: Box<dyn Node<GpuRuntime>>,
    ) -> NodeId {
        let id = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        ctl.adopt(id, node).unwrap();
        id
    }

    fn measure(ctx: &Arc<GpuContext>, native: bool, seconds: u64) {
        let (mut ctl, mut exec) = channel::<GpuRuntime>(Arc::clone(ctx), RenderLocal::default());
        let mut graph = GraphDescription::new();
        let clock = TestClock::new();
        let fps = FrameRate::FPS_60;
        let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock.clone()))
            .with_frame_rate(fps)
            .with_pixel_formats(&[PixelFormat::Bgra8]);
        let capture = add(
            &mut graph,
            &mut ctl,
            Box::new(CaptureSourceNode::new(
                Arc::new(TestCaptureFactory::new()),
                request,
            )),
        );
        let (comp, _layers) = CompositorNode::new(
            CanvasDesc {
                width: W,
                height: H,
            },
            4,
            vec![NodeLayer::new(capture, 0, 0.0, 0.0, W as f32, H as f32)],
        );
        let comp = add(&mut graph, &mut ctl, Box::new(comp));

        let export = VideoExportNode::new(YuvFormat::Nv12, W, H, W, H);
        let format = export.frame_format();
        let mut config = VideoEncoderConfig::new(
            CodecId::H264,
            format,
            fps,
            MemoryKind::System,
            RateControl::Cbr {
                bitrate_bps: 8_000_000,
            },
        );
        let owners = Arc::new(LeaseOwners::default());
        if native {
            let (kind, device) = export_preference(ctx).expect("no native export on this GPU");
            config = config.with_device(device);
            config.memory = kind;
        }
        let factory = Arc::new(VideoToolboxFactory::new());
        let encoder = VideoEncoderNode::open(factory, config).expect("VideoToolbox refused");
        let export = if native {
            export.native(NativeExport {
                encoder_frames: encoder.frames_held(),
                owners: Arc::clone(&owners),
            })
        } else {
            export
        };
        let export_stats = export.stats();
        let encoder_stats = encoder.stats();
        let export = add(&mut graph, &mut ctl, Box::new(export));
        let encoder = add(&mut graph, &mut ctl, Box::new(encoder));
        let sink = add(
            &mut graph,
            &mut ctl,
            Box::new(Sink(NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.example.sink"),
                NodeKind::Output,
                vec![Pin::one("packets", LinkKind::Packets)],
                vec![],
            ))),
        );
        graph.connect(capture, "video", comp, "layers").unwrap();
        graph.connect(comp, "canvas", export, "canvas").unwrap();
        graph.connect(export, "video", encoder, "video").unwrap();
        graph.connect(encoder, "packets", sink, "packets").unwrap();
        let report = ctl.apply(&graph).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        exec.swap_pending();
        let _ = ctl.collect();

        let cadence = Cadence::frames(Timestamp::ZERO, fps);
        let ticks = seconds * 60;
        let warmup = 60;
        let mut times = Vec::with_capacity(ticks as usize);
        let start = Instant::now();
        let mut cpu_start = cpu_time();
        for i in 0..warmup + ticks {
            if i == warmup {
                cpu_start = cpu_time();
            }
            let tick = Tick::of(&cadence, i);
            let due = start + Duration::from_nanos(tick.time.as_nanos());
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            clock.set(tick.time);
            let t0 = Instant::now();
            exec.run_tick(tick);
            let took = t0.elapsed();
            ctx.poll();
            let _ = ctl.collect();
            if i >= warmup {
                times.push(took);
            }
        }
        let cpu = cpu_time() - cpu_start;
        times.sort_unstable();
        let mean = times.iter().sum::<Duration>() / times.len() as u32;
        let p95 = times[times.len() * 95 / 100];
        let wall = Duration::from_secs(seconds);
        println!(
            "{:7} tick mean {:>7.3} ms  p95 {:>7.3} ms  cpu {:>6.1} % of one core  \
             export frames {} busy {} lost {}  encoder submitted {} packets {} busy_drops {} refused {}",
            if native { "native" } else { "system" },
            mean.as_secs_f64() * 1e3,
            p95.as_secs_f64() * 1e3,
            cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
            export_stats.frames.load(Ordering::Relaxed),
            export_stats.busy.load(Ordering::Relaxed),
            export_stats.lost.load(Ordering::Relaxed),
            encoder_stats.submitted.load(Ordering::Relaxed),
            encoder_stats.packets.load(Ordering::Relaxed),
            encoder_stats.busy_drops.load(Ordering::Relaxed),
            encoder_stats.refused.load(Ordering::Relaxed),
        );
        // Nodes go first; the exporters' lease owners are dropped after them (ADR-0147
        // decision 8).
        drop(exec);
        drop(ctl);
        drop(owners.take_all());
    }

    pub fn run() {
        let seconds = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let ctx = Arc::new(GpuContext::new(&GpuRequest::default()).expect("no GPU"));
        println!(
            "1080p60 H.264 CBR 8 Mbit/s, synthetic window, {seconds} s per mode after 1 s warm-up"
        );
        measure(&ctx, false, seconds);
        measure(&ctx, true, seconds);
    }
}
