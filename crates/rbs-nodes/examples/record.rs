//! Records the main display (and optionally the first camera, picture in picture) to an
//! H.264 Matroska file with the platform's adapters: a hardware check of the nodes in
//! `rbs-nodes` (spec `docs/specs/io-nodes.md`, ADR-0038), not an application.
//!
//! Windows: WGC display capture, Media Foundation camera, NVENC or AMF (the first whose
//! probe succeeds, or `--encoder nvenc|amf`), Matroska, file.
//!
//! ```text
//! cargo run -p rbs-nodes --example record --release -- --seconds 10 --camera --out C:\temp\rbs.mkv
//! ```
//!
//! Adapters are dev-dependencies of this example only; the library never depends on one
//! (P1). The composition root (`rbs-app`, `rbs-cli`) does this wiring for real.

#![allow(clippy::unwrap_used, clippy::expect_used)] // A hardware check fails by panicking.

#[cfg(not(windows))]
fn main() {
    eprintln!("record: this example wires the Windows adapters; run it on Windows.");
}

#[cfg(windows)]
fn main() {
    windows_record::run();
}

#[cfg(windows)]
mod windows_record {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use rbs_capture::{CaptureFactory, CaptureRequest, TargetKind};
    use rbs_codec::{KeyframeInterval, RateControl, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::lifecycle::Lifecycle;
    use rbs_core::media::{CodecId, ContainerId, MemoryKind, Size};
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_engine::{Engine, Pacing};
    use rbs_gpu::node::{CompositorNode, GpuRuntime, NodeLayer, RenderLocal};
    use rbs_gpu::{CanvasDesc, GpuContext, GpuRequest, YuvFormat};
    use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
    use rbs_graph::node::NodeDescriptor;
    use rbs_graph::runtime::Node;
    use rbs_nodes::{CaptureSourceNode, OutputNode, VideoEncoderNode, VideoExportNode};
    use rbs_transport::{TransportConfig, TransportInput};

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const ENCODER: PartitionId = PartitionId(1);
    const OUTPUT: PartitionId = PartitionId(2);

    struct Args {
        seconds: u64,
        camera: bool,
        out: String,
        encoder: Option<String>,
    }

    fn args() -> Args {
        let mut a = Args {
            seconds: 10,
            camera: false,
            out: std::env::temp_dir()
                .join("rbs-record.mkv")
                .display()
                .to_string(),
            encoder: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--seconds" => a.seconds = it.next().and_then(|v| v.parse().ok()).unwrap_or(10),
                "--camera" => a.camera = true,
                "--out" => a.out = it.next().expect("--out needs a path"),
                "--encoder" => a.encoder = it.next(),
                other => panic!("unknown argument {other}"),
            }
        }
        a
    }

    /// `C:\dir\f.mkv` -> `file:///C:/dir/f.mkv` (output ports spec, file transport).
    fn file_url(path: &str) -> String {
        format!("file:///{}", path.replace('\\', "/"))
    }

    fn encoder_factory(choice: Option<&str>) -> Arc<dyn VideoEncoderFactory> {
        let nvenc: Arc<dyn VideoEncoderFactory> = Arc::new(rbs_codec_nvenc::NvencFactory::new());
        let amf: Arc<dyn VideoEncoderFactory> = Arc::new(rbs_codec_amf::AmfFactory::new());
        let candidates = match choice {
            Some("nvenc") => vec![nvenc],
            Some("amf") => vec![amf],
            _ => vec![nvenc, amf],
        };
        for f in candidates {
            match f.probe() {
                Ok(_) => {
                    println!("encoder: {}", f.display_name());
                    return f;
                }
                Err(e) => println!("encoder {} unavailable: {e}", f.display_name()),
            }
        }
        panic!("no hardware H.264 encoder");
    }

    fn add(graph: &mut GraphDescription, desc: &NodeDescriptor, p: PartitionId) -> NodeId {
        let id = graph.add_node(Arc::new(desc.clone())).unwrap();
        graph.set_partition(id, p).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        id
    }

    pub fn run() {
        let args = args();
        let gpu = Arc::new(GpuContext::new(&GpuRequest::default()).expect("GPU"));
        println!("gpu: {}", gpu.adapter_name());
        let mut engine = Engine::new();
        let clock = engine.clock();
        let frames = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        engine
            .add_partition::<GpuRuntime>(
                PartitionId::RENDER,
                Arc::clone(&gpu),
                RenderLocal::default(),
                Pacing::cadence(frames),
            )
            .unwrap();
        // Encoders a quarter frame after render (engine spec, ADR-0027).
        engine
            .add_partition::<()>(
                ENCODER,
                Arc::new(()),
                (),
                Pacing::cadence_after(frames, Duration::from_nanos(4_166_667)),
            )
            .unwrap();
        engine
            .add_partition::<()>(OUTPUT, Arc::new(()), (), Pacing::output())
            .unwrap();

        let mut graph = GraphDescription::new();

        // Sources.
        let wgc: Arc<dyn CaptureFactory> = Arc::new(rbs_capture_wgc::WgcCaptureFactory::new());
        let display = wgc
            .enumerate(TargetKind::Display)
            .expect("displays")
            .into_iter()
            .next()
            .expect("a display");
        println!("display: {}", display.name);
        let screen = CaptureSourceNode::new(
            wgc,
            CaptureRequest::new(display.id.clone(), Arc::clone(&clock))
                .with_frame_rate(FrameRate::FPS_60)
                .with_cursor(true),
        );
        let screen_stats = screen.stats();
        let screen_id = add(
            &mut graph,
            Node::<GpuRuntime>::descriptor(&screen),
            PartitionId::RENDER,
        );
        let mut layers = vec![NodeLayer::new(
            screen_id,
            0,
            0.0,
            0.0,
            WIDTH as f32,
            HEIGHT as f32,
        )];
        let mut nodes: Vec<(NodeId, Box<dyn Node<GpuRuntime>>)> =
            vec![(screen_id, Box::new(screen))];
        let mut camera_stats = None;
        if args.camera {
            let mf: Arc<dyn CaptureFactory> = Arc::new(rbs_capture_mf::MfCaptureFactory::new());
            match mf
                .enumerate(TargetKind::Camera)
                .ok()
                .and_then(|t| t.into_iter().next())
            {
                Some(cam) => {
                    println!("camera: {}", cam.name);
                    let node = CaptureSourceNode::new(
                        mf,
                        CaptureRequest::new(cam.id.clone(), Arc::clone(&clock))
                            .with_size(Size::new(1280, 720))
                            .with_frame_rate(FrameRate::FPS_30),
                    );
                    camera_stats = Some(node.stats());
                    let id = add(
                        &mut graph,
                        Node::<GpuRuntime>::descriptor(&node),
                        PartitionId::RENDER,
                    );
                    layers.push(NodeLayer::new(id, 1, 1360.0, 740.0, 512.0, 288.0));
                    nodes.push((id, Box::new(node)));
                }
                None => println!("camera: none found"),
            }
        }

        // Canvas, export, encoder, output.
        let capacity = 16;
        let (comp, _layer_sender) = CompositorNode::new(
            CanvasDesc {
                width: WIDTH,
                height: HEIGHT,
            },
            capacity,
            layers,
        );
        let comp_id = add(
            &mut graph,
            Node::<GpuRuntime>::descriptor(&comp),
            PartitionId::RENDER,
        );
        let export = VideoExportNode::new(YuvFormat::Nv12, WIDTH, HEIGHT);
        let export_stats = export.stats();
        let config = VideoEncoderConfig::new(
            CodecId::H264,
            export.frame_format(),
            FrameRate::FPS_60,
            MemoryKind::System,
            RateControl::Cbr {
                bitrate_bps: 12_000_000,
            },
        )
        .with_keyframe_interval(KeyframeInterval::Duration(Duration::from_secs(2)));
        let export_id = add(
            &mut graph,
            Node::<GpuRuntime>::descriptor(&export),
            PartitionId::RENDER,
        );
        let encoder = VideoEncoderNode::open(encoder_factory(args.encoder.as_deref()), config)
            .expect("encoder session");
        let encoder_stats = encoder.stats();
        let output = OutputNode::new(
            vec![encoder.stream().clone()],
            Arc::new(rbs_transport_file::FileTransportFactory),
            TransportConfig::new(
                file_url(&args.out),
                TransportInput::Container(ContainerId::Matroska),
            ),
        )
        .with_muxer(
            Arc::new(rbs_mux_matroska::MatroskaMuxerFactory),
            ContainerId::Matroska,
            None,
        );
        let output_stats = output.stats();
        let encoder_id = add(&mut graph, Node::<()>::descriptor(&encoder), ENCODER);
        let output_id = add(&mut graph, Node::<()>::descriptor(&output), OUTPUT);

        for (id, _) in &nodes {
            graph.connect(*id, "video", comp_id, "layers").unwrap();
        }
        graph
            .connect(comp_id, "canvas", export_id, "canvas")
            .unwrap();
        graph
            .connect(export_id, "video", encoder_id, "video")
            .unwrap();
        graph
            .connect(encoder_id, "packets", output_id, "s0")
            .unwrap();

        for (id, node) in nodes {
            engine.adopt(PartitionId::RENDER, id, node).unwrap();
        }
        engine
            .adopt::<GpuRuntime>(PartitionId::RENDER, comp_id, Box::new(comp))
            .unwrap();
        engine
            .adopt::<GpuRuntime>(PartitionId::RENDER, export_id, Box::new(export))
            .unwrap();
        engine
            .adopt::<()>(ENCODER, encoder_id, Box::new(encoder))
            .unwrap();
        engine
            .adopt::<()>(OUTPUT, output_id, Box::new(output))
            .unwrap();

        let applied = engine.apply(&graph).expect("apply");
        for (p, report) in &applied.reports {
            if !report.failed.is_empty() {
                println!("partition {p:?}: could not start {:?}", report.failed);
            }
        }
        println!("recording {} s to {}", args.seconds, args.out);

        let start = Instant::now();
        let mut next_print = Duration::from_secs(1);
        while start.elapsed() < Duration::from_secs(args.seconds) {
            let s = engine.service();
            for e in &s.events {
                println!("event: {e:?}");
            }
            for e in &s.errors {
                println!("error: {e:?}");
            }
            if start.elapsed() >= next_print {
                next_print += Duration::from_secs(1);
                println!(
                    "t={:>2}s screen={} camera={} export={} (busy {}, lost {}) encoded={} packets={} written={} cpu_copies={}",
                    start.elapsed().as_secs(),
                    screen_stats.frames.load(Ordering::Relaxed),
                    camera_stats
                        .as_ref()
                        .map_or(0, |c| c.frames.load(Ordering::Relaxed)),
                    export_stats.frames.load(Ordering::Relaxed),
                    export_stats.busy.load(Ordering::Relaxed),
                    export_stats.lost.load(Ordering::Relaxed),
                    encoder_stats.submitted.load(Ordering::Relaxed),
                    encoder_stats.packets.load(Ordering::Relaxed),
                    output_stats.sent.load(Ordering::Relaxed),
                    gpu.metrics().frames_cpu_copied(),
                );
            }
            std::thread::sleep(
                s.wait
                    .unwrap_or(Duration::from_millis(50))
                    .min(Duration::from_millis(50)),
            );
        }
        let shutdown = engine.shutdown();
        for e in &shutdown.errors {
            println!("shutdown error: {e:?}");
        }
        println!(
            "done: encoded {} frames, {} busy drops, {} refused, {} lost at flush; output backlog peak {}, {} lost at finish",
            encoder_stats.submitted.load(Ordering::Relaxed),
            encoder_stats.busy_drops.load(Ordering::Relaxed),
            encoder_stats.refused.load(Ordering::Relaxed),
            encoder_stats.flush_lost.load(Ordering::Relaxed),
            output_stats.backlog_peak.load(Ordering::Relaxed),
            output_stats.finish_lost.load(Ordering::Relaxed),
        );
        let size = std::fs::metadata(&args.out).map(|m| m.len()).unwrap_or(0);
        println!("file: {} ({} bytes)", args.out, size);
    }
}
