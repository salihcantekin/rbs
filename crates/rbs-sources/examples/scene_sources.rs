//! Hardware check of the source type registry (spec `docs/specs/scene-sources.md`,
//! ADR-0039): a scene with the main display, the first camera, the default
//! microphone and the system audio is bound with the scene binder, every source node
//! the binder creates is instantiated from the registry, and the graph runs on the real
//! engine workers. Not an application.
//!
//! Windows: WGC (display, window), Media Foundation (camera), WASAPI (audio).
//!
//! ```text
//! cargo run -p rbs-sources --example scene_sources --release -- 10
//! ```
//!
//! Prints the installed source types and their targets, one line per second, and
//! fails when a source delivers nothing, a node does not start, or the canvas or the
//! mix stalls. Adapters are dev-dependencies of this example only (P1).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)] // A hardware check fails by panicking.

#[cfg(not(windows))]
fn main() {
    println!("scene_sources: this example wires the Windows adapters; run it on Windows.");
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows_check::run()
}

#[cfg(windows)]
mod windows_check {
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use rbs_audio::{AudioGraphConfig, InputStats, LevelMeter, MixerNode, MixerParams, Strip};
    use rbs_core::lifecycle::Lifecycle;
    use rbs_core::media::Size;
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_engine::{Engine, Pacing};
    use rbs_gpu::node::{CompositorNode, GpuRuntime, RenderLocal};
    use rbs_gpu::{CanvasDesc, GpuContext, GpuRequest};
    use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
    use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};
    use rbs_nodes::capture::CaptureNodeStats;
    use rbs_scene::SceneCollection;
    use rbs_scene::binder::{Role, SceneBinder};
    use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings, SourceId};
    use rbs_sources::types;
    use rbs_sources::{InstanceContext, SourceInstance, SourceTypes, node_layers};

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const AUDIO: PartitionId = PartitionId(1);

    /// Counts distinct frames (or blocks) arriving on its only input.
    struct Sink {
        desc: NodeDescriptor,
        count: Arc<AtomicU64>,
        last: Option<Timestamp>,
    }

    impl Sink {
        fn new(kind: LinkKind) -> (Self, Arc<AtomicU64>) {
            let count = Arc::new(AtomicU64::new(0));
            (
                Self {
                    desc: NodeDescriptor::new(
                        NodeTypeId::new("org.rbs.example.sink"),
                        NodeKind::Output,
                        vec![Pin::one("in", kind)],
                        vec![],
                    ),
                    count: Arc::clone(&count),
                    last: None,
                },
                count,
            )
        }
    }

    impl<X: Runtime> Node<X> for Sink {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.desc
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, X>) {}
        fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
            let inputs = ctx.inputs();
            let t = match self.desc.inputs[0].kind {
                LinkKind::Video => inputs.video(0, 0).map(|f| f.timestamp()),
                _ => inputs.audio(0, 0).map(|b| b.timestamp()),
            };
            if let Some(t) = t {
                if self.last != Some(t) {
                    self.last = Some(t);
                    self.count.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        }
    }

    fn add_sink<X: Runtime>(
        graph: &mut GraphDescription,
        from: NodeId,
        pin: &str,
        kind: LinkKind,
        partition: PartitionId,
    ) -> (NodeId, Sink, Arc<AtomicU64>) {
        let (sink, count) = Sink::new(kind);
        let id = graph
            .add_node(Arc::new(Node::<X>::descriptor(&sink).clone()))
            .unwrap();
        graph.set_partition(id, partition).unwrap();
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        graph.connect(from, pin, id, "in").unwrap();
        (id, sink, count)
    }

    enum Stats {
        Video(Arc<CaptureNodeStats>),
        Audio(Arc<InputStats>, Arc<LevelMeter>),
    }

    pub fn run() -> ExitCode {
        let seconds: u64 = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);

        // The composition root's part: register the platform adapters.
        let mut registry = SourceTypes::new();
        registry
            .register_capture(Arc::new(rbs_capture_wgc::WgcCaptureFactory::new()))
            .unwrap();
        registry
            .register_capture(Arc::new(rbs_capture_mf::MfCaptureFactory::new()))
            .unwrap();
        registry
            .register_audio(Arc::new(rbs_audio_io_wasapi::WasapiAudioBackend::new()))
            .unwrap();
        registry.probe();
        let installed = registry.installed();
        println!("installed source types:");
        for t in &installed {
            let targets = registry.targets(t).unwrap_or_default();
            println!("  {t}: {} target(s)", targets.len());
            for target in targets.iter().take(4) {
                println!(
                    "    {} [{}] {}{}",
                    target.name,
                    target.adapter,
                    target.target,
                    if target.is_default { " (default)" } else { "" }
                );
            }
        }

        // A scene: display full screen, camera picture in picture, microphone and
        // system audio.
        let mut col = SceneCollection::new();
        let (canvas, _) = col
            .add_canvas("Main", Size::new(WIDTH, HEIGHT), FrameRate::FPS_60)
            .unwrap();
        let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
        let mut wanted: Vec<(SourceId, &str)> = Vec::new();
        for (name, t, layout) in [
            (
                "Display",
                types::DISPLAY_SOURCE,
                Layout::new([0.0, 0.0], [WIDTH as f32, HEIGHT as f32]),
            ),
            (
                "Camera",
                types::CAMERA_SOURCE,
                Layout::new([1360.0, 740.0], [512.0, 288.0]),
            ),
            (
                "Microphone",
                types::AUDIO_INPUT_SOURCE,
                Layout::new([0.0, 0.0], [1.0, 1.0]),
            ),
            (
                "System audio",
                types::AUDIO_OUTPUT_SOURCE,
                Layout::new([0.0, 0.0], [1.0, 1.0]),
            ),
        ] {
            let type_id = NodeTypeId::new_static(t);
            if !installed.contains(&type_id) || registry.targets(&type_id).unwrap().is_empty() {
                println!("skipping {name}: nothing to capture");
                continue;
            }
            let (id, _) = col.add_source(name, type_id, Settings::new()).unwrap();
            col.add_item(scene, ItemContent::Source(id), layout, None)
                .unwrap();
            wanted.push((id, name));
        }
        col.set_program(canvas, Some(scene)).unwrap();

        let mut graph = GraphDescription::new();
        let mut binder = SceneBinder::new(registry.node_types()).with_audio(AUDIO);
        let report = binder.sync(&col, &mut graph).expect("bind");
        if !report.problems.is_empty() {
            println!("bind problems: {:?}", report.problems);
            return ExitCode::FAILURE;
        }

        let gpu = Arc::new(GpuContext::new(&GpuRequest::default()).expect("GPU"));
        println!("gpu: {}", gpu.adapter_name());
        let mut engine = Engine::new();
        let clock = engine.clock();
        let audio = AudioGraphConfig::default();
        let ctx = InstanceContext::new(Arc::clone(&clock), audio);

        // Instances for the nodes the binder created.
        let mut stats: Vec<(&str, Stats)> = Vec::new();
        let mut video_nodes: Vec<(NodeId, Box<dyn Node<GpuRuntime>>)> = Vec::new();
        let mut audio_nodes: Vec<(NodeId, Box<dyn Node<()>>)> = Vec::new();
        let mut strips = Vec::new();
        for (source, node) in &report.created_sources {
            let name = wanted
                .iter()
                .find(|(id, _)| id == source)
                .map_or("?", |(_, n)| *n);
            let partition = graph.node(*node).unwrap().partition;
            match registry
                .instantiate(col.source(*source).unwrap(), &ctx)
                .unwrap()
            {
                SourceInstance::Video(v) => {
                    println!(
                        "{name}: {} via {} (partition {partition})",
                        v.target, v.adapter
                    );
                    assert_eq!(partition, PartitionId::RENDER);
                    stats.push((name, Stats::Video(Arc::clone(&v.stats))));
                    video_nodes.push((*node, Box::new(v.node)));
                }
                SourceInstance::Audio(a) => {
                    println!(
                        "{name}: {} via {} (partition {partition})",
                        a.endpoint, a.adapter
                    );
                    assert_eq!(partition, AUDIO);
                    let meter = Arc::new(LevelMeter::new(2));
                    strips.push(Strip::new(*node, 1.0).with_meter(Arc::clone(&meter)));
                    stats.push((name, Stats::Audio(Arc::clone(&a.stats), meter)));
                    audio_nodes.push((*node, Box::new(a.node)));
                }
                other => panic!("unexpected instance {other:?}"),
            }
        }

        // The compositor and the mixer the binder made, plus sinks so the plan
        // schedules them (the record command attaches encoders here instead).
        let comp_id = binder.compositor(canvas, Role::Program).unwrap();
        let mixer_id = binder.mixer().unwrap();
        let (video_sink_id, video_sink, canvas_frames) = add_sink::<GpuRuntime>(
            &mut graph,
            comp_id,
            "canvas",
            LinkKind::Video,
            PartitionId::RENDER,
        );
        let (audio_sink_id, audio_sink, mix_blocks) =
            add_sink::<()>(&mut graph, mixer_id, "main", LinkKind::Audio, AUDIO);
        let bound = &report.compositors[&(canvas, Role::Program)];
        let (compositor, _layers) = CompositorNode::new(
            CanvasDesc {
                width: WIDTH,
                height: HEIGHT,
            },
            16,
            node_layers(bound, &graph.plan()),
        );
        let (mixer, _mix_params, mixer_stats) = MixerNode::new(audio, 16, MixerParams::new(strips));

        let frames = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        engine
            .add_partition::<GpuRuntime>(
                PartitionId::RENDER,
                Arc::clone(&gpu),
                RenderLocal::default(),
                Pacing::cadence(frames),
            )
            .unwrap();
        engine
            .add_partition::<()>(
                AUDIO,
                Arc::new(()),
                (),
                Pacing::stream(audio.cadence(clock.now()), 4),
            )
            .unwrap();
        for (id, node) in video_nodes {
            engine.adopt(PartitionId::RENDER, id, node).unwrap();
        }
        for (id, node) in audio_nodes {
            engine.adopt(AUDIO, id, node).unwrap();
        }
        engine
            .adopt::<GpuRuntime>(PartitionId::RENDER, comp_id, Box::new(compositor))
            .unwrap();
        engine
            .adopt::<GpuRuntime>(PartitionId::RENDER, video_sink_id, Box::new(video_sink))
            .unwrap();
        engine
            .adopt::<()>(AUDIO, mixer_id, Box::new(mixer))
            .unwrap();
        engine
            .adopt::<()>(AUDIO, audio_sink_id, Box::new(audio_sink))
            .unwrap();

        let applied = engine.apply(&graph).expect("apply");
        let mut ok = true;
        for (p, r) in &applied.reports {
            if !r.failed.is_empty() {
                println!("FAIL partition {p}: nodes did not start: {:?}", r.failed);
                ok = false;
            }
        }

        let start = Instant::now();
        for s in 1..=seconds {
            while start.elapsed() < Duration::from_secs(s) {
                let serviced = engine.service();
                for e in &serviced.errors {
                    println!("error: {e:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut line = format!(
                "t={s:>2}s canvas={} mix={}",
                canvas_frames.load(Ordering::Relaxed),
                mix_blocks.load(Ordering::Relaxed)
            );
            for (name, st) in &stats {
                match st {
                    Stats::Video(v) => line.push_str(&format!(
                        " | {name}: {} frames ({} imported, {} uploaded, {} gpu errors)",
                        v.frames.load(Ordering::Relaxed),
                        v.imported.load(Ordering::Relaxed),
                        v.uploaded.load(Ordering::Relaxed),
                        v.gpu_errors.load(Ordering::Relaxed),
                    )),
                    Stats::Audio(a, meter) => {
                        let c = a.read();
                        let l = meter.take();
                        line.push_str(&format!(
                            " | {name}: {:.1} dB, {:+} ppm, {} resyncs",
                            l.peak_db(0).max(l.peak_db(1)),
                            c.correction_ppm,
                            c.resyncs
                        ));
                    }
                }
            }
            println!("{line}");
        }
        let shutdown = engine.shutdown();
        for e in &shutdown.errors {
            println!("shutdown error: {e:?}");
            ok = false;
        }

        println!("--- summary after {seconds} s");
        let canvas = canvas_frames.load(Ordering::Relaxed);
        let mix = mix_blocks.load(Ordering::Relaxed);
        // 60 fps canvas and 100 audio blocks per second, with a second of start-up.
        let expect_canvas = seconds.saturating_sub(1) * 50;
        let expect_mix = seconds.saturating_sub(1) * 90;
        println!(
            "canvas frames {canvas} (want >= {expect_canvas}), mix blocks {mix} (want >= {expect_mix})"
        );
        println!(
            "mixer: invalid inputs {}, unknown inputs {}",
            mixer_stats.invalid_inputs(),
            mixer_stats.unknown_inputs()
        );
        if canvas < expect_canvas || mix < expect_mix {
            println!("FAIL: the canvas or the mix stalled");
            ok = false;
        }
        for (name, st) in &stats {
            let delivered = match st {
                Stats::Video(v) => v.frames.load(Ordering::Relaxed),
                Stats::Audio(a, _) => a.read().frames_in,
            };
            println!("{name}: {delivered} delivered");
            if delivered == 0 {
                println!("FAIL: {name} delivered nothing");
                ok = false;
            }
        }
        if ok {
            println!("PASS");
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}
