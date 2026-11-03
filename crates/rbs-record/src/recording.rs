//! The recording: graph assembly, start, progress and finish.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, EncoderNode, InputStats, LevelMeter, MixerNode, MixerParams};
use rbs_audio::{MixerStats, Strip};
use rbs_codec::{
    AudioEncoderConfig, AudioEncoderFactory, KeyframeInterval, RateControl, VideoEncoderConfig,
    VideoEncoderFactory,
};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{CodecId, ContainerId, MemoryKind};
use rbs_core::registry::{AdapterId, Registry};
use rbs_core::time::Timestamp;
use rbs_core::timing::Cadence;
use rbs_engine::{DEFAULT_DOORBELL_TIMER, Engine, EngineError, Pacing, Serviced, WorkerStats};
use rbs_gpu::node::{CompositorNode, GpuRuntime, RenderLocal};
use rbs_gpu::{CanvasDesc, GpuContext, YuvFormat};
use rbs_graph::description::{GraphDescription, GraphError, NodeId, PartitionId};
use rbs_graph::node::NodeDescriptor;
use rbs_graph::runtime::{Node, RuntimeEvent};
use rbs_mux::MuxerFactory;
use rbs_nodes::capture::CaptureNodeStats;
use rbs_nodes::encode::EncoderNodeStats;
use rbs_nodes::export::ExportNodeStats;
use rbs_nodes::output::OutputNodeStats;
use rbs_nodes::{OutputNode, VideoExportNode};
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindError, BindProblem, Role, SceneBinder};
use rbs_scene::model::CanvasId;
use rbs_sources::{InstanceContext, SourceError, SourceInstance, SourceTypes, node_layers};
use rbs_transport::{TransportConfig, TransportFactory, TransportInput};

use crate::select::{self, Chosen};

/// The render partition: capture sources, the compositor and the video export.
pub const RENDER: PartitionId = PartitionId::RENDER;
/// The audio partition: audio inputs and the mixer, at block cadence.
pub const AUDIO: PartitionId = PartitionId(1);
/// The video encoder, a quarter frame after the render tick.
pub const VIDEO_ENCODER: PartitionId = PartitionId(2);
/// The audio encoder, one tick per queued block.
pub const AUDIO_ENCODER: PartitionId = PartitionId(3);
/// The muxed output (muxer and transport).
pub const OUTPUT: PartitionId = PartitionId(4);

/// Audio ticks a late audio worker still runs in order (ADR-0037).
const AUDIO_CATCH_UP: u32 = 4;
/// Packets the audio encoder node holds while its output queue is full.
const AUDIO_ENCODER_PACKETS: usize = 64;
/// Layers and strips the compositor and the mixer hold without allocating.
const MIN_CAPACITY: usize = 16;

/// The adapters a recording may use, each port's registry probed by the caller.
pub struct RecordAdapters {
    /// Capture and audio input adapters (source types).
    pub sources: SourceTypes,
    /// Video encoders.
    pub video_encoders: Registry<dyn VideoEncoderFactory>,
    /// Audio encoders.
    pub audio_encoders: Registry<dyn AudioEncoderFactory>,
    /// Muxers.
    pub muxers: Registry<dyn MuxerFactory>,
    /// Transports.
    pub transports: Registry<dyn TransportFactory>,
}

impl RecordAdapters {
    /// Empty registries; register adapters and probe each before
    /// [`Recording::start`].
    pub fn new() -> Self {
        Self {
            sources: SourceTypes::new(),
            video_encoders: Registry::new(),
            audio_encoders: Registry::new(),
            muxers: Registry::new(),
            transports: Registry::new(),
        }
    }

    /// Probes every registry.
    pub fn probe(&mut self) {
        self.sources.probe();
        self.video_encoders.probe_all();
        self.audio_encoders.probe_all();
        self.muxers.probe_all();
        self.transports.probe_all();
    }
}

impl Default for RecordAdapters {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RecordAdapters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordAdapters")
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

/// Video encoding of the recording.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSettings {
    /// Codec.
    pub codec: CodecId,
    /// Rate control.
    pub rate_control: RateControl,
    /// Keyframe interval.
    pub keyframe_interval: KeyframeInterval,
    /// The export's pixel format (NV12, or P010 for 10-bit).
    pub format: YuvFormat,
}

impl Default for VideoSettings {
    /// H.264, CBR 12 Mbit/s, a keyframe every 2 s, NV12.
    fn default() -> Self {
        Self {
            codec: CodecId::H264,
            rate_control: RateControl::Cbr {
                bitrate_bps: 12_000_000,
            },
            keyframe_interval: KeyframeInterval::Duration(Duration::from_secs(2)),
            format: YuvFormat::Nv12,
        }
    }
}

/// Audio encoding of the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    /// Codec.
    pub codec: CodecId,
    /// Bitrate in bit/s.
    pub bitrate_bps: u64,
}

impl Default for AudioSettings {
    /// AAC at 160 kbit/s.
    fn default() -> Self {
        Self {
            codec: CodecId::Aac,
            bitrate_bps: 160_000,
        }
    }
}

/// What to record and where.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSettings {
    /// The canvas recorded (its program scene).
    pub canvas: CanvasId,
    /// Destination URL, for example `file:///C:/Videos/rec.mkv`.
    pub url: String,
    /// Container.
    pub container: ContainerId,
    /// Video encoding.
    pub video: VideoSettings,
    /// Audio encoding; `None` records video only.
    pub audio: Option<AudioSettings>,
    /// The audio graph's format, block size and latency.
    pub audio_graph: AudioGraphConfig,
}

impl RecordSettings {
    /// `canvas` to `url` in `container` with the default video and audio settings.
    pub fn new(canvas: CanvasId, url: impl Into<String>, container: ContainerId) -> Self {
        Self {
            canvas,
            url: url.into(),
            container,
            video: VideoSettings::default(),
            audio: Some(AudioSettings::default()),
            audio_graph: AudioGraphConfig::default(),
        }
    }
}

/// Why a recording could not start.
#[non_exhaustive]
#[derive(Debug)]
pub enum RecordError {
    /// The collection has no canvas with this id.
    UnknownCanvas(CanvasId),
    /// The URL has no scheme.
    InvalidUrl(String),
    /// The binder refused the collection.
    Bind(BindError),
    /// The binder could not represent parts of the collection.
    Problems(Vec<BindProblem>),
    /// The canvas has no program compositor or mixer.
    NotBound(&'static str),
    /// A source could not be instantiated.
    Source {
        /// The source's name.
        name: String,
        /// Why.
        error: SourceError,
    },
    /// A source instance of a kind this crate does not know.
    UnsupportedSource(String),
    /// No adapter of a port could serve the recording: each one tried, and why not.
    NoAdapter {
        /// The port.
        port: &'static str,
        /// Adapters tried or rejected, with the reason.
        tried: Vec<(AdapterId, String)>,
    },
    /// The graph refused an edit (a bug in this crate).
    Graph(GraphError),
    /// The engine refused an edit.
    Engine(EngineError),
    /// Nodes the recording needs did not start: `(node, reason)`.
    Start(Vec<(String, String)>),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::UnknownCanvas(id) => write!(f, "no canvas {id:?}"),
            RecordError::InvalidUrl(url) => write!(f, "not a URL: {url}"),
            RecordError::Bind(e) => write!(f, "binding the scene failed: {e}"),
            RecordError::Problems(p) => write!(f, "the scene cannot be bound: {p:?}"),
            RecordError::NotBound(what) => write!(f, "the canvas has no {what}"),
            RecordError::Source { name, error } => write!(f, "source {name}: {error}"),
            RecordError::UnsupportedSource(name) => {
                write!(f, "source {name}: unsupported instance kind")
            }
            RecordError::NoAdapter { port, tried } => {
                write!(f, "no {port} can serve the recording")?;
                for (id, why) in tried {
                    write!(f, "; {id}: {why}")?;
                }
                Ok(())
            }
            RecordError::Graph(e) => write!(f, "graph: {e}"),
            RecordError::Engine(e) => write!(f, "engine: {e}"),
            RecordError::Start(nodes) => {
                write!(f, "the recording did not start")?;
                for (node, why) in nodes {
                    write!(f, "; {node}: {why}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RecordError {}

impl From<GraphError> for RecordError {
    fn from(e: GraphError) -> Self {
        RecordError::Graph(e)
    }
}

impl From<EngineError> for RecordError {
    fn from(e: EngineError) -> Self {
        RecordError::Engine(e)
    }
}

/// A source of the recorded canvas as started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceState {
    /// The source's name.
    pub name: String,
    /// The adapter capturing it.
    pub adapter: AdapterId,
    /// The capture target or audio endpoint id.
    pub target: String,
    /// Why it did not start or stopped, if it did not or did.
    pub failed: Option<String>,
}

/// What [`Recording::start`] chose and started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartReport {
    /// The video encoder.
    pub video_encoder: Chosen,
    /// The audio encoder, when audio is recorded.
    pub audio_encoder: Option<Chosen>,
    /// The muxer.
    pub muxer: Chosen,
    /// The transport.
    pub transport: Chosen,
    /// The sources bound for the collection, in binder order.
    pub sources: Vec<SourceState>,
}

/// Counters of one source.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceProgress {
    /// The source's name.
    pub name: String,
    /// Frames delivered: video frames, or audio sample frames read from the device.
    pub frames: u64,
    /// Peak level in dBFS since the last [`Recording::progress`] (audio only).
    pub peak_db: Option<f32>,
    /// Audio timeline restarts (audio only).
    pub resyncs: u64,
    /// GPU import/upload errors (video) or device read errors (audio).
    pub errors: u64,
}

/// Counters of a running recording, read on the control thread.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Progress {
    /// Per source, in [`StartReport::sources`] order.
    pub sources: Vec<SourceProgress>,
    /// Canvas frames handed to the video encoder.
    pub frames_exported: u64,
    /// Canvas frames the export dropped (readback ring full) or lost.
    pub export_drops: u64,
    /// Frames the video encoder took.
    pub frames_encoded: u64,
    /// Video frames dropped or refused at the encoder, and packets lost in its carry-over.
    pub encoder_drops: u64,
    /// Audio packets the audio encoder node lost (its carry-over was full).
    pub audio_packet_drops: u64,
    /// Audio blocks lost at the queue between the mixer and the audio encoder.
    pub audio_queue_drops: u64,
    /// Packets the output took, all streams.
    pub packets_written: u64,
    /// Packets the muxer dropped as out of order.
    pub mux_drops: u64,
    /// Largest output backlog (transport queue full).
    pub backlog_peak: u64,
    /// Render ticks skipped by a late render worker.
    pub late_render_ticks: u64,
    /// Audio ticks skipped by a late audio worker.
    pub late_audio_ticks: u64,
    /// Master peak in dBFS since the last [`Recording::progress`], when audio is recorded.
    pub master_peak_db: Option<f32>,
}

/// The result of [`Recording::finish`].
#[derive(Debug, Clone, PartialEq)]
pub struct FinishReport {
    /// The counters at the end.
    pub progress: Progress,
    /// Why the recording failed while it ran, if it did.
    pub failure: Option<String>,
    /// Errors while the engine drained.
    pub shutdown_errors: Vec<String>,
    /// Packets the encoders still held at shutdown (lost, engine spec open question).
    pub flush_lost: u64,
    /// Output items not delivered when the output finished.
    pub finish_lost: u64,
}

enum Watch {
    Video(Arc<CaptureNodeStats>),
    Audio(Arc<InputStats>, Arc<LevelMeter>),
}

/// A running recording. It owns the engine; [`Recording::finish`] stops it.
pub struct Recording {
    engine: Engine,
    started: StartReport,
    watches: Vec<(NodeId, Watch)>,
    /// Nodes without which the recording cannot go on.
    essential: BTreeMap<NodeId, &'static str>,
    names: BTreeMap<NodeId, String>,
    failure: Option<String>,
    export: Arc<ExportNodeStats>,
    encoder: Arc<EncoderNodeStats>,
    audio_dropped: Option<Arc<AtomicU64>>,
    output: Arc<OutputNodeStats>,
    master: Option<Arc<LevelMeter>>,
    mixer: Option<Arc<MixerStats>>,
    render_worker: Option<Arc<WorkerStats>>,
    audio_worker: Option<Arc<WorkerStats>>,
    // Parameter senders: kept so the nodes' channels stay open (gain, layers, offsets
    // are not changed while recording headless yet).
    _keep: Vec<Box<dyn std::any::Any>>,
}

impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recording")
            .field("started", &self.started)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

/// Adds `desc` to `graph`, Active, in `partition`.
fn add(
    graph: &mut GraphDescription,
    desc: &NodeDescriptor,
    partition: PartitionId,
) -> Result<NodeId, GraphError> {
    let id = graph.add_node(Arc::new(desc.clone()))?;
    graph.set_partition(id, partition)?;
    graph.set_state(id, Lifecycle::Standby)?;
    graph.set_state(id, Lifecycle::Active)?;
    Ok(id)
}

fn sum(counters: &[&AtomicU64]) -> u64 {
    counters.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

impl Recording {
    /// Binds `collection`, attaches the encoders and the output to the canvas of
    /// `settings`, and starts `engine` on `gpu`. The engine must have no partitions yet;
    /// the recording adds [`RENDER`], [`AUDIO`], [`VIDEO_ENCODER`], [`AUDIO_ENCODER`]
    /// and [`OUTPUT`]. Control thread; may block while devices open.
    ///
    /// A source that does not start is reported in [`StartReport::sources`] and the
    /// recording goes on without it; a compositor, mixer, export, encoder or output that
    /// does not start fails the call, after the engine was shut down.
    pub fn start(
        mut engine: Engine,
        gpu: Arc<GpuContext>,
        collection: &SceneCollection,
        adapters: &RecordAdapters,
        settings: &RecordSettings,
    ) -> Result<Self, RecordError> {
        let canvas = collection
            .canvas(settings.canvas)
            .ok_or(RecordError::UnknownCanvas(settings.canvas))?;
        let (width, height) = (canvas.size.width, canvas.size.height);
        let fps = canvas.frame_rate;
        let clock = engine.clock();
        let audio_graph = settings.audio_graph;

        // Adapters for the encoders and the output, chosen before anything opens.
        let export = VideoExportNode::new(settings.video.format, width, height);
        let video_config = VideoEncoderConfig::new(
            settings.video.codec,
            export.frame_format(),
            fps,
            MemoryKind::System,
            settings.video.rate_control,
        )
        .with_keyframe_interval(settings.video.keyframe_interval);
        let (video_encoder, video_chosen) =
            select::video_encoder(&adapters.video_encoders, &video_config)?;
        let mut streams = vec![video_encoder.stream().clone()];
        let audio_encoder = match &settings.audio {
            Some(audio) => {
                let config =
                    AudioEncoderConfig::new(audio.codec, audio_graph.format, audio.bitrate_bps);
                let (session, chosen) = select::audio_encoder(&adapters.audio_encoders, &config)?;
                streams.push(session.stream().clone());
                Some((EncoderNode::new(session, AUDIO_ENCODER_PACKETS), chosen))
            }
            None => None,
        };
        let (muxer, muxer_chosen) = select::muxer(&adapters.muxers, settings.container)?;
        let (transport, transport_chosen) =
            select::transport(&adapters.transports, &settings.url, settings.container)?;

        // The scene.
        let mut graph = GraphDescription::new();
        let mut binder = SceneBinder::new(adapters.sources.node_types()).with_audio(AUDIO);
        let report = binder
            .sync(collection, &mut graph)
            .map_err(RecordError::Bind)?;
        if !report.problems.is_empty() {
            return Err(RecordError::Problems(report.problems));
        }
        let ctx = InstanceContext::new(Arc::clone(&clock), audio_graph);
        let mut names = BTreeMap::new();
        let mut sources = Vec::new();
        let mut watches = Vec::new();
        let mut meters = BTreeMap::new();
        let mut keep: Vec<Box<dyn std::any::Any>> = Vec::new();
        let mut render_nodes: Vec<(NodeId, Box<dyn Node<GpuRuntime>>)> = Vec::new();
        let mut audio_nodes: Vec<(NodeId, Box<dyn Node<()>>)> = Vec::new();
        for (source_id, node) in &report.created_sources {
            let Some(source) = collection.source(*source_id) else {
                continue;
            };
            let name = source.name.clone();
            let instance = adapters
                .sources
                .instantiate(source, &ctx)
                .map_err(|error| RecordError::Source {
                    name: name.clone(),
                    error,
                })?;
            match instance {
                SourceInstance::Video(v) => {
                    sources.push(SourceState {
                        name: name.clone(),
                        adapter: v.adapter,
                        target: v.target.as_str().to_owned(),
                        failed: None,
                    });
                    watches.push((*node, Watch::Video(v.stats)));
                    render_nodes.push((*node, Box::new(v.node)));
                }
                SourceInstance::Audio(a) => {
                    sources.push(SourceState {
                        name: name.clone(),
                        adapter: a.adapter,
                        target: a.endpoint.as_str().to_owned(),
                        failed: None,
                    });
                    let meter = Arc::new(LevelMeter::new(audio_graph.format.channels.channels()));
                    meters.insert(*source_id, Arc::clone(&meter));
                    watches.push((*node, Watch::Audio(a.stats, meter)));
                    keep.push(Box::new(a.offset));
                    audio_nodes.push((*node, Box::new(a.node)));
                }
                _ => return Err(RecordError::UnsupportedSource(name)),
            }
            names.insert(*node, name);
        }

        // Video: compositor -> export -> encoder.
        let canvas_id = settings.canvas;
        let compositor_id = binder
            .compositor(canvas_id, Role::Program)
            .ok_or(RecordError::NotBound("program compositor"))?;
        let mut essential = BTreeMap::new();
        essential.insert(compositor_id, "compositor");
        let export_stats = export.stats();
        let export_id = add(&mut graph, Node::<GpuRuntime>::descriptor(&export), RENDER)?;
        essential.insert(export_id, "video export");
        let encoder_stats = video_encoder.stats();
        let encoder_id = add(
            &mut graph,
            Node::<()>::descriptor(&video_encoder),
            VIDEO_ENCODER,
        )?;
        essential.insert(encoder_id, "video encoder");
        let output = OutputNode::new(
            streams,
            transport,
            TransportConfig::new(
                settings.url.clone(),
                TransportInput::Container(settings.container),
            ),
        )
        .with_muxer(muxer, settings.container, None);
        let output_stats = output.stats();
        let output_id = add(&mut graph, Node::<()>::descriptor(&output), OUTPUT)?;
        essential.insert(output_id, "output");
        graph.connect(compositor_id, "canvas", export_id, "canvas")?;
        graph.connect(export_id, "video", encoder_id, "video")?;
        graph.connect(encoder_id, "packets", output_id, "s0")?;

        // Audio: mixer -> encoder.
        let mut audio = None;
        if let Some((node, chosen)) = audio_encoder {
            let mixer_id = binder
                .mixer(canvas_id)
                .ok_or(RecordError::NotBound("mixer"))?;
            essential.insert(mixer_id, "mixer");
            let dropped = node.dropped();
            let id = add(&mut graph, Node::<()>::descriptor(&node), AUDIO_ENCODER)?;
            essential.insert(id, "audio encoder");
            graph.connect(mixer_id, "main", id, "in")?;
            graph.connect(id, "packets", output_id, "s1")?;
            audio = Some((mixer_id, id, node, chosen, dropped));
        }

        // Instances that need the final plan: layers in link order, mixer strips.
        let bound = report
            .compositors
            .get(&(canvas_id, Role::Program))
            .ok_or(RecordError::NotBound("program compositor"))?;
        let layers = node_layers(bound, &graph.plan());
        let (compositor, layer_sender) = CompositorNode::new(
            CanvasDesc { width, height },
            u32::try_from(layers.len().max(MIN_CAPACITY)).unwrap_or(u32::MAX),
            layers,
        );
        keep.push(Box::new(layer_sender));

        let frames = Cadence::frames(Timestamp::ZERO, fps);
        engine.add_partition::<GpuRuntime>(
            RENDER,
            Arc::clone(&gpu),
            RenderLocal::default(),
            Pacing::cadence(frames),
        )?;
        engine.add_partition::<()>(
            AUDIO,
            Arc::new(()),
            (),
            Pacing::stream(audio_graph.cadence(clock.now()), AUDIO_CATCH_UP),
        )?;
        engine.add_partition::<()>(
            VIDEO_ENCODER,
            Arc::new(()),
            (),
            Pacing::cadence_after(frames, fps.frame_duration() / 4),
        )?;
        engine.add_partition::<()>(
            AUDIO_ENCODER,
            Arc::new(()),
            (),
            Pacing::doorbell(DEFAULT_DOORBELL_TIMER),
        )?;
        engine.add_partition::<()>(OUTPUT, Arc::new(()), (), Pacing::output())?;

        for (id, node) in render_nodes {
            engine.adopt(RENDER, id, node)?;
        }
        for (id, node) in audio_nodes {
            engine.adopt(AUDIO, id, node)?;
        }
        engine.adopt::<GpuRuntime>(RENDER, compositor_id, Box::new(compositor))?;
        engine.adopt::<GpuRuntime>(RENDER, export_id, Box::new(export))?;
        engine.adopt::<()>(VIDEO_ENCODER, encoder_id, Box::new(video_encoder))?;
        engine.adopt::<()>(OUTPUT, output_id, Box::new(output))?;
        let mut master = None;
        let mut mixer_stats = None;
        let mut audio_dropped = None;
        let mut audio_chosen = None;
        if let Some((mixer_id, id, node, chosen, dropped)) = audio {
            let strips: Vec<Strip> = report
                .mixers
                .get(&canvas_id)
                .map(|m| {
                    m.inputs
                        .iter()
                        .map(|i| {
                            let strip = Strip::new(i.producer, 1.0);
                            match meters.get(&i.source) {
                                Some(m) => strip.with_meter(Arc::clone(m)),
                                None => strip,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let meter = Arc::new(LevelMeter::new(audio_graph.format.channels.channels()));
            let capacity = strips.len().max(MIN_CAPACITY);
            let (mixer, params, stats) = MixerNode::new(
                audio_graph,
                capacity,
                MixerParams::new(strips).with_master_meter(Arc::clone(&meter)),
            );
            keep.push(Box::new(params));
            engine.adopt::<()>(AUDIO, mixer_id, Box::new(mixer))?;
            engine.adopt::<()>(AUDIO_ENCODER, id, Box::new(node))?;
            master = Some(meter);
            mixer_stats = Some(stats);
            audio_dropped = Some(dropped);
            audio_chosen = Some(chosen);
        }

        let applied = engine.apply(&graph)?;
        let mut fatal = Vec::new();
        for error in &applied.errors {
            fatal.push(("engine".to_owned(), error.to_string()));
        }
        for (link, problem) in &applied.problems {
            fatal.push((format!("link {link:?}"), format!("{problem:?}")));
        }
        for report in applied.reports.values() {
            for (node, why) in &report.failed {
                if let Some(role) = essential.get(node) {
                    fatal.push(((*role).to_owned(), why.to_string()));
                } else if let Some(name) = names.get(node) {
                    if let Some(s) = sources.iter_mut().find(|s| &s.name == name) {
                        s.failed = Some(why.to_string());
                    }
                }
            }
        }
        if !fatal.is_empty() {
            let _ = engine.shutdown();
            return Err(RecordError::Start(fatal));
        }
        for (id, role) in &essential {
            names.insert(*id, (*role).to_owned());
        }
        let render_worker = engine.worker_stats(RENDER);
        let audio_worker = engine.worker_stats(AUDIO);
        Ok(Self {
            engine,
            started: StartReport {
                video_encoder: video_chosen,
                audio_encoder: audio_chosen,
                muxer: muxer_chosen,
                transport: transport_chosen,
                sources,
            },
            watches,
            essential,
            names,
            failure: None,
            export: export_stats,
            encoder: encoder_stats,
            audio_dropped,
            output: output_stats,
            master,
            mixer: mixer_stats,
            render_worker,
            audio_worker,
            _keep: keep,
        })
    }

    /// What was chosen and started.
    pub fn started(&self) -> &StartReport {
        &self.started
    }

    /// The engine's clock (the master clock of every timestamp in the file).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The name of a node of the recording (a source name or a role such as
    /// `"video encoder"`), for messages about engine events.
    pub fn node_name(&self, node: NodeId) -> Option<&str> {
        self.names.get(&node).map(String::as_str)
    }

    /// Runs the engine's control loop once (restarts, swaps) and notes failures: a
    /// source that fails is marked in [`StartReport::sources`]; an essential node that
    /// fails for good, or an output failure, ends the recording ([`Recording::failure`]).
    /// Call it again after `wait` (at most about 100 ms).
    pub fn service(&mut self) -> Serviced {
        let serviced = self.engine.service();
        for (_, event) in &serviced.events {
            let RuntimeEvent::Failed {
                node,
                cause,
                retry_at,
                ..
            } = event
            else {
                continue;
            };
            if let Some(role) = self.essential.get(node) {
                if retry_at.is_none() && self.failure.is_none() {
                    self.failure = Some(format!("{role}: {cause:?}"));
                }
            } else if let Some(name) = self.names.get(node) {
                if let Some(s) = self.started.sources.iter_mut().find(|s| &s.name == name) {
                    s.failed = Some(format!("{cause:?}"));
                }
            }
        }
        serviced
    }

    /// Why the recording cannot go on, if it cannot: the output's first permanent
    /// failure, or an essential node that failed for good.
    pub fn failure(&self) -> Option<String> {
        self.output
            .failure
            .get()
            .cloned()
            .or_else(|| self.failure.clone())
    }

    /// The counters now. Peak levels are those since the previous call.
    pub fn progress(&self) -> Progress {
        let mut sources = Vec::with_capacity(self.watches.len());
        for ((_, watch), state) in self.watches.iter().zip(&self.started.sources) {
            sources.push(match watch {
                Watch::Video(v) => SourceProgress {
                    name: state.name.clone(),
                    frames: v.frames.load(Ordering::Relaxed),
                    peak_db: None,
                    resyncs: 0,
                    errors: v.gpu_errors.load(Ordering::Relaxed),
                },
                Watch::Audio(a, meter) => {
                    let c = a.read();
                    let levels = meter.take();
                    let peak = (0..levels.channels())
                        .map(|ch| levels.peak_db(ch))
                        .fold(f32::NEG_INFINITY, f32::max);
                    SourceProgress {
                        name: state.name.clone(),
                        frames: c.frames_in,
                        peak_db: Some(peak),
                        resyncs: c.resyncs,
                        errors: c.read_errors,
                    }
                }
            });
        }
        let audio_queue_drops = self
            .engine
            .crossings()
            .iter()
            .filter(|c| c.from == AUDIO && c.to == AUDIO_ENCODER)
            .map(|c| c.stats.overflows())
            .sum();
        let e = &self.encoder;
        Progress {
            sources,
            frames_exported: self.export.frames.load(Ordering::Relaxed),
            export_drops: sum(&[&self.export.busy, &self.export.lost]),
            frames_encoded: e.submitted.load(Ordering::Relaxed),
            encoder_drops: sum(&[&e.busy_drops, &e.refused, &e.overflow_drops]),
            audio_packet_drops: self
                .audio_dropped
                .as_ref()
                .map_or(0, |d| d.load(Ordering::Relaxed)),
            audio_queue_drops,
            packets_written: self.output.packets.load(Ordering::Relaxed),
            mux_drops: self.output.mux_drops.load(Ordering::Relaxed),
            backlog_peak: self.output.backlog_peak.load(Ordering::Relaxed),
            late_render_ticks: self.render_worker.as_ref().map_or(0, |w| w.late()),
            late_audio_ticks: self.audio_worker.as_ref().map_or(0, |w| w.late()),
            master_peak_db: self.master.as_ref().map(|m| {
                let levels = m.take();
                (0..levels.channels())
                    .map(|ch| levels.peak_db(ch))
                    .fold(f32::NEG_INFINITY, f32::max)
            }),
        }
    }

    /// Mixer inputs that had no strip or an invalid block (both should stay 0).
    pub fn mixer_anomalies(&self) -> u64 {
        self.mixer
            .as_ref()
            .map_or(0, |m| m.invalid_inputs() + m.unknown_inputs())
    }

    /// Stops the recording: drains the engine in pipeline order (sources first, the
    /// output last, which writes the container's trailer) and reports.
    pub fn finish(mut self) -> FinishReport {
        let shutdown = self.engine.shutdown();
        let progress = self.progress();
        FinishReport {
            progress,
            failure: self.failure(),
            shutdown_errors: shutdown.errors.iter().map(ToString::to_string).collect(),
            flush_lost: self.encoder.flush_lost.load(Ordering::Relaxed),
            finish_lost: self.output.finish_lost.load(Ordering::Relaxed),
        }
    }
}
