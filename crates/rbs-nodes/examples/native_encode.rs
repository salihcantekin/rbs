//! Measures the encode path of one 1080p60 H.264 chain with system memory (GPU readback,
//! CPU copy into the encoder's input) and with native input (no copy; ADR-0147):
//! synthetic capture -> compositor -> video export -> encoder node -> sink, ticked at
//! 60 fps in real time. macOS: VideoToolbox reads exported IOSurfaces. Windows: NVENC
//! reads exported D3D12 buffers through CUDA (the render device must be the NVIDIA GPU).
//!
//! Prints per mode: render-thread time per tick (mean, p95), process CPU time (user +
//! system, which includes the encoder's own threads), frames and drops, and a hash of the
//! encoded stream. The synthetic picture depends only on the tick, so with no drops in
//! either mode equal hashes mean the encoder read the same pixels from the exported
//! surface as from the readback. A hardware measurement (P15, ADR-0147), not a test.
//!
//! ```text
//! cargo run -p rbs-nodes --example native_encode --release -- [seconds per mode, default 5]
//! ```
//!
//! The adapter is a dev-dependency of this example only; the library never depends on
//! one (P1).

#![allow(clippy::unwrap_used, clippy::expect_used)] // A hardware check fails by panicking.

#[cfg(not(any(target_vendor = "apple", windows)))]
fn main() {
    eprintln!(
        "native_encode: this example measures VideoToolbox or NVENC; run it on macOS or Windows."
    );
}

#[cfg(any(target_vendor = "apple", windows))]
fn main() {
    hw::run();
}

#[cfg(any(target_vendor = "apple", windows))]
mod hw {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rbs_capture::{CaptureRequest, TargetId};
    use rbs_capture_test::TestCaptureFactory;
    use rbs_codec::{RateControl, VideoEncoderConfig, VideoEncoderFactory};
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

    /// The encoder of this platform.
    #[cfg(target_vendor = "apple")]
    fn factory() -> Arc<dyn VideoEncoderFactory> {
        Arc::new(rbs_codec_videotoolbox::VideoToolboxFactory::new())
    }

    /// The encoder of this platform.
    #[cfg(windows)]
    fn factory() -> Arc<dyn VideoEncoderFactory> {
        Arc::new(rbs_codec_nvenc::NvencFactory::new())
    }

    /// Hashes the encoder's packets (FNV-1a over the bytes) and lets them go. It runs in
    /// the tick, the same work in both modes.
    struct Sink {
        descriptor: NodeDescriptor,
        hash: Arc<Mutex<(u64, u64)>>,
    }

    impl Node<GpuRuntime> for Sink {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.descriptor
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
            Ok(())
        }
        fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
            let mut state = self.hash.lock().unwrap();
            for p in ctx.inputs().packets(0) {
                for b in p.data().iter() {
                    state.0 = (state.0 ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
                }
                state.1 += 1;
            }
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
    }

    #[cfg(windows)]
    fn cpu_time() -> Duration {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut create, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: GetProcessTimes fills the four FILETIMEs it is given for our own process.
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut create,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        }
        .expect("GetProcessTimes");
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        // 100 ns units.
        Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
    }

    #[cfg(target_vendor = "apple")]
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
        let encoder = VideoEncoderNode::open(factory(), config).expect("the encoder refused");
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
        let hash = Arc::new(Mutex::new((0xcbf2_9ce4_8422_2325_u64, 0_u64)));
        let sink = add(
            &mut graph,
            &mut ctl,
            Box::new(Sink {
                descriptor: NodeDescriptor::new(
                    NodeTypeId::new_static("org.rbs.example.sink"),
                    NodeKind::Output,
                    vec![Pin::one("packets", LinkKind::Packets)],
                    vec![],
                ),
                hash: Arc::clone(&hash),
            }),
        );
        let copies = Arc::new(AtomicU64::new(ctx.metrics().frames_cpu_copied()));
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
        let copied = ctx.metrics().frames_cpu_copied() - copies.load(Ordering::Relaxed);
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
        let (h, n) = *hash.lock().unwrap();
        println!("        stream hash {h:016x} over {n} packets, CPU readbacks {copied}");
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
        let info = ctx.adapter().get_info();
        println!("render device: {} ({:?})", info.name, info.backend);
        println!(
            "1080p60 H.264 CBR 8 Mbit/s, synthetic window, {seconds} s per mode after 1 s warm-up"
        );
        measure(&ctx, false, seconds);
        measure(&ctx, true, seconds);
    }
}
