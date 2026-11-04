//! The studio: the program scene of one canvas running on the engine, shown in a
//! preview and metered, with recordings started and stopped while it runs (spec
//! `docs/specs/recording.md`, "Studio", ADR-0041).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio::{
    AudioGraphConfig, EncoderNode, InputParams, InputStats, LevelMeter, MeterSinkNode, MixerNode,
    MixerParams, MixerStats, Strip,
};
use rbs_codec::{AudioEncoderConfig, EncoderTuning, VideoEncoderConfig};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ContainerId, MemoryKind};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Applied, DEFAULT_DOORBELL_TIMER, Engine, Pacing, Serviced, WorkerStats};
use rbs_gpu::node::{CompositorNode, GpuRuntime, LayerSender, RenderLocal};
use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_gpu::{CanvasDesc, GpuContext};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::queue::QueueStats;
use rbs_graph::runtime::{Node, RuntimeEvent};
use rbs_nodes::capture::CaptureNodeStats;
use rbs_nodes::encode::EncoderNodeStats;
use rbs_nodes::export::{ExportEnd, ExportNodeStats};
use rbs_nodes::output::OutputNodeStats;
use rbs_nodes::{OutputNode, PreviewNode, PreviewNodeStats, VideoEncoderNode, VideoExportNode};
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindProblem, BindReport, BoundCompositor, BoundMixer, Role, SceneBinder};
use rbs_scene::model::{CanvasId, SourceId};
use rbs_sources::{InstanceContext, SourceInstance, SourceTypes, node_layers};
use rbs_transport::{ReconnectPolicy, Secret, TransportConfig, TransportInput};

use crate::recording::{
    AudioSettings, FinishReport, Partitions, Progress, RENDER, RecordAdapters, RecordError,
    SourceProgress, SourceState, StartFailure, StartReport, VideoSettings, add_active, is_fatal,
    peak_db, sum,
};
use crate::select::{self, Chosen};

/// Why a scheduled source has no instance yet: the one it replaces did not close.
const WAITING: &str = "waiting for the source it replaces to close";
/// Audio ticks a late audio worker still runs in order (ADR-0037).
const AUDIO_CATCH_UP: u32 = 4;
/// Packets the audio encoder node holds while its output queue is full.
const AUDIO_ENCODER_PACKETS: usize = 64;
/// Layers and strips the compositor and the mixer hold without allocating. Scenes edited
/// while the studio runs may grow up to this; more layers are not drawn.
pub const MAX_LAYERS: usize = 64;
/// Longest wait, on the master clock, for a recording's video tail after
/// [`Studio::end_recording`]; afterwards [`Studio::recording_tail_done`] is true anyway
/// (a stalled readback must not hold the stop up, ADR-0045).
pub const END_TAIL_LIMIT: Duration = Duration::from_millis(500);

/// How the studio runs its canvas.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StudioSettings {
    /// The canvas run (its program scene).
    pub canvas: CanvasId,
    /// Audio: the canvas's mixer, its inputs and a meter sink run, so meters move and a
    /// recording can take audio. `false`: audio sources are not opened.
    pub audio: bool,
    /// The audio graph's format, block size and latency.
    pub audio_graph: AudioGraphConfig,
    /// Partition ids.
    pub partitions: Partitions,
    /// A preview of the canvas for a UI (macOS and Windows). `None`: the canvas is only
    /// rendered while something records it.
    pub preview: Option<PreviewDesc>,
    /// Sync offset in ns of each audio source (ADR-0037): positive delays the source,
    /// negative advances it. Sources not listed have none. The audio input clamps it to
    /// its range (`rbs_sources::DEFAULT_MAX_AUDIO_OFFSET` at most).
    pub audio_offsets: BTreeMap<SourceId, i64>,
}

impl StudioSettings {
    /// `canvas` with audio, the default audio graph and partitions, and no preview.
    pub fn new(canvas: CanvasId) -> Self {
        Self {
            canvas,
            audio: true,
            audio_graph: AudioGraphConfig::default(),
            partitions: Partitions::default(),
            preview: None,
            audio_offsets: BTreeMap::new(),
        }
    }
}

/// Where and how a recording or a live stream is written.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSettings {
    /// Destination URL, for example `file:///C:/Videos/rec.mkv`, `rtmp://host/app` or
    /// `srt://host:port`. Never holds the secret.
    pub url: String,
    /// What the transport takes: the chunks of a muxer writing a container (files,
    /// SRT with MPEG-TS), or encoded packets it frames itself (RTMP); no muxer is
    /// chosen for packets.
    pub input: TransportInput,
    /// Stream key or stream id, passed to the transport only (never in the URL, logs
    /// or reports).
    pub secret: Option<Secret>,
    /// What the transport does when the connection drops.
    pub reconnect: ReconnectPolicy,
    /// Opaque transport options (`latency_ms` for SRT), passed through unread.
    pub transport_options: Vec<(String, String)>,
    /// Video encoding.
    pub video: VideoSettings,
    /// Audio encoding; `None` records video only.
    pub audio: Option<AudioSettings>,
}

impl OutputSettings {
    /// `url` in `container` with the default video and audio settings, no secret, no
    /// reconnect.
    pub fn new(url: impl Into<String>, container: ContainerId) -> Self {
        Self::with_input(url, TransportInput::Container(container))
    }

    /// Encoded packets to `url` (a transport that frames them itself, such as RTMP),
    /// with the default video and audio settings, no secret, no reconnect.
    pub fn packets(url: impl Into<String>) -> Self {
        Self::with_input(url, TransportInput::Packets)
    }

    fn with_input(url: impl Into<String>, input: TransportInput) -> Self {
        Self {
            url: url.into(),
            input,
            secret: None,
            reconnect: ReconnectPolicy::Off,
            transport_options: Vec::new(),
            video: VideoSettings::default(),
            audio: Some(AudioSettings::default()),
        }
    }

    /// The container the muxer writes, `None` for packet input.
    pub fn container(&self) -> Option<ContainerId> {
        match self.input {
            TransportInput::Container(c) => Some(c),
            _ => None,
        }
    }

    /// The transport configuration: URL, input, secret, reconnect and options, default
    /// queue limits.
    pub(crate) fn transport_config(&self) -> TransportConfig {
        let mut config =
            TransportConfig::new(self.url.clone(), self.input).with_reconnect(self.reconnect);
        if let Some(secret) = &self.secret {
            config = config.with_secret(secret.clone());
        }
        for (key, value) in &self.transport_options {
            config = config.with_adapter_option(key.clone(), value.clone());
        }
        config
    }
}

/// The running output's connection, for a UI that shows whether a stream is live.
/// Read from the output's counters only: no meter peaks are taken.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    /// The transport is connected now.
    pub connected: bool,
    /// Connections that reached the point of delivering, reconnections included.
    pub connects: u64,
    /// Connection attempts after a failure or a lost connection.
    pub reconnect_attempts: u64,
    /// Items the transport dropped (its queue filled while slow or down).
    pub transport_drops: u64,
}

/// What [`Studio::update`] did.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Updated {
    /// Binder problems: parts of the collection that are not shown or played.
    pub problems: Vec<BindProblem>,
    /// Nodes that did not join, and engine errors.
    pub failed: Vec<StartFailure>,
}

pub(crate) enum Watch {
    Video(Arc<CaptureNodeStats>),
    Audio(Arc<InputStats>, Arc<LevelMeter>),
}

/// A source's running instance.
struct Instance {
    partition: PartitionId,
    gpu: bool,
    watch: Watch,
    // The audio input's sync offset channel.
    offset: Option<ParamSender<InputParams>>,
}

/// One bound source.
struct Entry {
    source: SourceId,
    state: SourceState,
    instance: Option<Instance>,
    /// In the running plan.
    scheduled: bool,
}

/// A node the studio added to the graph, so it can be taken back.
#[derive(Debug, Clone, Copy)]
struct Added {
    node: NodeId,
    partition: PartitionId,
    gpu: bool,
}

/// Adapters chosen for a recording, nothing in the graph yet.
pub(crate) struct Prepared {
    export: VideoExportNode,
    video_encoder: VideoEncoderNode,
    video_chosen: Chosen,
    audio: Option<(EncoderNode, Chosen)>,
    output: OutputNode,
    muxer: Option<Chosen>,
    transport: Chosen,
}

impl Prepared {
    /// Chooses every adapter of a recording of a `width x height` canvas at `fps`; opens
    /// only the encoder sessions (their stream descriptors make the output).
    pub(crate) fn choose(
        adapters: &RecordAdapters,
        out: &OutputSettings,
        (width, height): (u32, u32),
        fps: FrameRate,
        audio_graph: AudioGraphConfig,
    ) -> Result<Self, RecordError> {
        // System memory: the export reads the canvas back (the P4 exception of
        // ADR-0038); native export will take the memory kind from the encoder.
        let export = VideoExportNode::new(out.video.format, width, height);
        let video_config = VideoEncoderConfig::new(
            out.video.codec,
            export.frame_format(),
            fps,
            MemoryKind::System,
            out.video.rate_control,
        )
        .with_keyframe_interval(out.video.keyframe_interval)
        .with_max_b_frames(out.video.max_b_frames)
        .with_tuning({
            let mut tuning = EncoderTuning::default();
            tuning.preset = out.video.preset;
            tuning
        });
        let video_config = match &out.video.profile {
            Some(profile) => video_config.with_profile(profile.clone()),
            None => video_config,
        };
        let (video_encoder, video_chosen) =
            select::video_encoder(&adapters.video_encoders, &video_config)?;
        let mut streams = vec![video_encoder.stream().clone()];
        let audio = match &out.audio {
            Some(a) => {
                let config = AudioEncoderConfig::new(a.codec, audio_graph.format, a.bitrate_bps);
                let (session, chosen) = select::audio_encoder(&adapters.audio_encoders, &config)?;
                streams.push(session.stream().clone());
                Some((EncoderNode::new(session, AUDIO_ENCODER_PACKETS), chosen))
            }
            None => None,
        };
        let muxer = match out.container() {
            Some(container) => Some((select::muxer(&adapters.muxers, container)?, container)),
            None => None,
        };
        let (transport_factory, transport) =
            select::transport(&adapters.transports, &out.url, out.input)?;
        let mut output = OutputNode::new(streams, transport_factory, out.transport_config());
        let muxer = match muxer {
            Some(((factory, chosen), container)) => {
                output = output.with_muxer(factory, container, None);
                Some(chosen)
            }
            None => None,
        };
        Ok(Self {
            export,
            video_encoder,
            video_chosen,
            audio,
            output,
            muxer,
            transport,
        })
    }
}

/// The running recording's nodes and counters.
struct Attached {
    report: StartReport,
    nodes: Vec<Added>,
    essential: BTreeMap<NodeId, &'static str>,
    failure: Option<String>,
    export: Arc<ExportNodeStats>,
    end: Arc<ExportEnd>,
    encoder: Arc<EncoderNodeStats>,
    audio_dropped: Option<Arc<AtomicU64>>,
    /// The audio encoder's end-of-stream flushes and tail losses (ADR-0045).
    audio_tail: Option<(Arc<AtomicU64>, Arc<AtomicU64>)>,
    output: Arc<OutputNodeStats>,
    /// The queues into the video and audio encoders' partitions, found after the
    /// apply that scheduled them and kept, so the final counters survive the draining
    /// apply that removes them.
    queues: Queues,
}

/// The cross-partition queues feeding a recording's encoders.
#[derive(Default)]
struct Queues {
    video: Option<Arc<QueueStats>>,
    audio: Option<Arc<QueueStats>>,
}

/// The mixer of the canvas and what the studio keeps of it.
struct Mixer {
    node: NodeId,
    params: ParamSender<MixerParams>,
    stats: Arc<MixerStats>,
    master: Arc<LevelMeter>,
}

/// The program scene of one canvas, running.
///
/// [`Studio::start`] binds the collection, builds the canvas's compositor and mixer and,
/// as asked, a preview for a UI and a meter sink, adds the partitions and starts the
/// engine. [`Studio::update`] applies scene edits while it runs.
/// [`Studio::start_recording`] attaches a video export, encoders and an output to the
/// running graph; [`Studio::stop_recording`] drains and removes them while the preview
/// and meters keep running (`Engine::apply_draining`). Control thread only; calls may
/// block while devices open or workers swap.
pub struct Studio {
    engine: Engine,
    settings: StudioSettings,
    size: (u32, u32),
    fps: FrameRate,
    binder: SceneBinder,
    graph: GraphDescription,
    /// The collection last bound (sources are looked up in it when a plan first
    /// schedules them).
    collection: SceneCollection,
    compositor: NodeId,
    layers: LayerSender,
    bound: BoundCompositor,
    mixer: Option<Mixer>,
    bound_mixer: Option<BoundMixer>,
    meters: BTreeMap<SourceId, Arc<LevelMeter>>,
    preview: Option<Arc<PreviewNodeStats>>,
    entries: Vec<Entry>,
    problems: Vec<BindProblem>,
    /// Studio nodes without which it cannot run (compositor, mixer, preview, sink).
    essential: BTreeMap<NodeId, &'static str>,
    failure: Option<String>,
    recording: Option<Attached>,
    render_worker: Option<Arc<WorkerStats>>,
    audio_worker: Option<Arc<WorkerStats>>,
    /// Retired instances whose take failed (their partition's swap was still in
    /// flight), with the source they belonged to; [`Studio::service`] retries them.
    pending_takes: Vec<(Added, Option<SourceId>)>,
    /// The mixer's strips last sent: producer and meter.
    strips: Vec<(NodeId, Option<Arc<LevelMeter>>)>,
}

impl fmt::Debug for Studio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Studio")
            .field("settings", &self.settings)
            .field("sources", &self.sources())
            .field("recording", &self.is_recording())
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

/// Whether a start-time problem stops the studio: `strict` (a recording that must show
/// every source of its scene) or never.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strictness {
    /// Problems and instantiation errors are reported; the studio runs without them.
    Lenient,
    /// A problem that leaves out a source of the program scene, or a source that cannot
    /// be instantiated, fails the start before anything opens.
    Strict,
}

impl Studio {
    /// Binds `collection`, builds the canvas's compositor, its mixer and meter sink
    /// (`settings.audio`) and a preview (`settings.preview`), adds the partitions and
    /// applies. The partitions of `settings.partitions` and [`RENDER`] must be new to the
    /// engine; the encoder and output partitions get a worker only while a recording runs.
    ///
    /// A source that cannot be instantiated or does not start is reported in
    /// [`Studio::sources`] and the studio runs without it. The compositor, mixer, preview
    /// or sink not starting fails the call after the engine was shut down.
    pub fn start(
        engine: Engine,
        gpu: Arc<GpuContext>,
        collection: &SceneCollection,
        sources: &SourceTypes,
        settings: &StudioSettings,
    ) -> Result<Self, RecordError> {
        Self::start_with(
            engine,
            gpu,
            collection,
            sources,
            settings,
            Strictness::Lenient,
            None,
        )
    }

    /// [`Studio::start`] with a strictness and, for [`Recording`](crate::Recording), a
    /// recording added to the graph before anything is instantiated: with `Strict`, every
    /// source the recording schedules is instantiated before the first apply opens any
    /// device.
    pub(crate) fn start_with(
        mut engine: Engine,
        gpu: Arc<GpuContext>,
        collection: &SceneCollection,
        sources: &SourceTypes,
        settings: &StudioSettings,
        strictness: Strictness,
        recording: Option<Prepared>,
    ) -> Result<Self, RecordError> {
        let canvas_id = settings.canvas;
        let canvas = collection
            .canvas(canvas_id)
            .ok_or(RecordError::UnknownCanvas(canvas_id))?;
        let program = canvas.program.ok_or(RecordError::NoProgram(canvas_id))?;
        let (width, height) = (canvas.size.width, canvas.size.height);
        let fps = canvas.frame_rate;
        let parts = settings.partitions;

        let mut graph = GraphDescription::new();
        let mut binder = SceneBinder::new(sources.node_types());
        if settings.audio {
            binder = binder.with_audio(parts.audio);
        }
        let report = binder
            .sync(collection, &mut graph)
            .map_err(RecordError::Bind)?;
        if strictness == Strictness::Strict {
            let shown = collection
                .visible_sources(canvas_id, program)
                .map_err(|_| RecordError::NoProgram(canvas_id))?;
            let fatal: Vec<BindProblem> = report
                .problems
                .iter()
                .filter(|p| is_fatal(collection, &shown, settings.audio, p))
                .cloned()
                .collect();
            if !fatal.is_empty() {
                return Err(RecordError::Problems(fatal));
            }
        }
        let compositor_id = binder
            .compositor(canvas_id, Role::Program)
            .ok_or(RecordError::NotBound("program compositor"))?;
        let bound = report
            .compositors
            .get(&(canvas_id, Role::Program))
            .cloned()
            .ok_or(RecordError::NotBound("program compositor"))?;
        let mut essential = BTreeMap::new();
        essential.insert(compositor_id, "compositor");

        // Partitions: no worker until a schedule with nodes is applied (ADR-0013).
        let frames = Cadence::frames(Timestamp::ZERO, fps);
        let clock = engine.clock();
        engine.add_partition::<GpuRuntime>(
            RENDER,
            Arc::clone(&gpu),
            RenderLocal::default(),
            Pacing::cadence(frames),
        )?;
        engine.add_partition::<()>(
            parts.video_encoder,
            Arc::new(()),
            (),
            Pacing::cadence_after(frames, fps.frame_duration() / 4),
        )?;
        engine.add_partition::<()>(parts.output, Arc::new(()), (), Pacing::output())?;
        if settings.audio {
            engine.add_partition::<()>(
                parts.audio,
                Arc::new(()),
                (),
                Pacing::stream(settings.audio_graph.cadence(clock.now()), AUDIO_CATCH_UP),
            )?;
            engine.add_partition::<()>(
                parts.audio_encoder,
                Arc::new(()),
                (),
                Pacing::doorbell(DEFAULT_DOORBELL_TIMER),
            )?;
        }

        let (compositor, layers) =
            CompositorNode::new(CanvasDesc { width, height }, MAX_LAYERS as u32, Vec::new());
        engine.adopt::<GpuRuntime>(RENDER, compositor_id, Box::new(compositor))?;

        let mut preview = None;
        if let Some(desc) = settings.preview {
            let node = PreviewNode::new(desc);
            preview = Some(node.stats());
            let id = add_active(&mut graph, Node::<GpuRuntime>::descriptor(&node), RENDER)?;
            graph.connect(compositor_id, "canvas", id, "canvas")?;
            engine.adopt::<GpuRuntime>(RENDER, id, Box::new(node))?;
            essential.insert(id, "preview");
        }

        let mut mixer = None;
        if settings.audio {
            let mixer_id = binder
                .mixer(canvas_id)
                .ok_or(RecordError::NotBound("mixer"))?;
            essential.insert(mixer_id, "mixer");
            let master = Arc::new(LevelMeter::new(
                settings.audio_graph.format.channels.channels(),
            ));
            let (node, params, stats) = MixerNode::new(
                settings.audio_graph,
                MAX_LAYERS,
                MixerParams::new(Vec::new()).with_master_meter(Arc::clone(&master)),
            );
            engine.adopt::<()>(parts.audio, mixer_id, Box::new(node))?;
            let sink = MeterSinkNode::new();
            let sink_id = add_active(&mut graph, Node::<()>::descriptor(&sink), parts.audio)?;
            graph.connect(mixer_id, "main", sink_id, "in")?;
            engine.adopt::<()>(parts.audio, sink_id, Box::new(sink))?;
            essential.insert(sink_id, "meter sink");
            mixer = Some(Mixer {
                node: mixer_id,
                params,
                stats,
                master,
            });
        }

        let render_worker = engine.worker_stats(RENDER);
        let audio_worker = engine.worker_stats(parts.audio);
        let mut studio = Self {
            engine,
            settings: settings.clone(),
            size: (width, height),
            fps,
            binder,
            graph,
            collection: collection.clone(),
            compositor: compositor_id,
            layers,
            bound,
            mixer,
            bound_mixer: None,
            meters: BTreeMap::new(),
            preview,
            entries: Vec::new(),
            problems: Vec::new(),
            essential,
            failure: None,
            recording: None,
            render_worker,
            audio_worker,
            pending_takes: Vec::new(),
            strips: Vec::new(),
        };
        if let Some(p) = recording
            && let Err(e) = studio.add_recording(p)
        {
            let _ = studio.engine.shutdown();
            return Err(e);
        }
        match studio.reconcile(sources, Some(report), strictness) {
            Ok((failed, _)) => {
                let fatal: Vec<StartFailure> =
                    failed.into_iter().filter(|f| f.node != "source").collect();
                if !fatal.is_empty() {
                    return Err(studio.fail(fatal));
                }
                let sources = studio.sources();
                let problems = studio.problems.clone();
                if let Some(r) = studio.recording.as_mut() {
                    r.report.sources = sources;
                    r.report.problems = problems;
                }
                Ok(studio)
            }
            Err(e) => {
                let _ = studio.engine.shutdown();
                Err(e)
            }
        }
    }

    /// Shuts the engine down and turns `failed` into [`RecordError::Start`].
    fn fail(mut self, mut failed: Vec<StartFailure>) -> RecordError {
        for e in self.engine.shutdown().errors {
            failed.push(StartFailure {
                node: "shutdown".to_owned(),
                reason: e.to_string(),
            });
        }
        RecordError::Start(failed)
    }

    /// Applies an edited collection while the studio runs: new sources are opened and
    /// shown, removed ones closed, layout and order changes reach the compositor, and the
    /// mixer gets a strip per audio input. A source whose settings changed is closed
    /// before its replacement opens (an exclusive camera). The canvas's size and rate
    /// cannot change (`NotBound`).
    pub fn update(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
    ) -> Result<Updated, RecordError> {
        let canvas = collection
            .canvas(self.settings.canvas)
            .ok_or(RecordError::UnknownCanvas(self.settings.canvas))?;
        if (canvas.size.width, canvas.size.height) != self.size || canvas.frame_rate != self.fps {
            return Err(RecordError::NotBound("canvas of the same size and rate"));
        }
        // Checked before anything changes: without a program scene the binder would
        // retire the compositor the studio runs.
        if canvas.program.is_none() {
            return Err(RecordError::NoProgram(self.settings.canvas));
        }
        let report = self
            .binder
            .sync(collection, &mut self.graph)
            .map_err(RecordError::Bind)?;
        self.collection = collection.clone();
        let (failed, problems) = self.reconcile(sources, Some(report), Strictness::Lenient)?;
        Ok(Updated { problems, failed })
    }

    /// Brings the engine in line with `self.graph`: entries for the sources a sync
    /// created and retired, instances for every scheduled source without one, the
    /// compositor's layers and the mixer's strips, one apply, then the retired instances
    /// taken back. Returns the nodes that did not join and the binder's problems.
    fn reconcile(
        &mut self,
        sources: &SourceTypes,
        report: Option<BindReport>,
        strictness: Strictness,
    ) -> Result<(Vec<StartFailure>, Vec<BindProblem>), RecordError> {
        let mut failed = Vec::new();
        let mut gone: Vec<Entry> = Vec::new();
        let collection = &self.collection;
        if let Some(report) = report {
            if report.retired_compositors.contains(&self.compositor)
                || self
                    .mixer
                    .as_ref()
                    .is_some_and(|m| report.retired_mixers.contains(&m.node))
            {
                // `update` checks the canvas first, so the binder should never do this;
                // the graph no longer matches the engine, so the studio cannot go on.
                self.failure = Some("the program compositor or mixer was retired".to_owned());
                return Err(RecordError::NotBound("program compositor"));
            }
            self.problems = report.problems.clone();
            if let Some(b) = report
                .compositors
                .get(&(self.settings.canvas, Role::Program))
            {
                self.bound = b.clone();
            }
            self.bound_mixer = report.mixers.get(&self.settings.canvas).cloned();
            let retired: BTreeSet<NodeId> = report.retired_sources.iter().copied().collect();
            let (old, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|e| retired.contains(&e.state.node));
            self.entries = kept;
            gone = old;
            for (source_id, node) in &report.created_sources {
                let Some(source) = collection.source(*source_id) else {
                    continue;
                };
                self.entries.push(Entry {
                    source: *source_id,
                    state: SourceState {
                        node: *node,
                        name: source.name.clone(),
                        adapter: rbs_core::registry::AdapterId::new(""),
                        target: String::new(),
                        failed: None,
                    },
                    instance: None,
                    scheduled: false,
                });
            }
            // Break before make: a replaced source's old instance (an exclusive camera)
            // is released before its replacement opens.
            if !report.replaced_sources.is_empty() {
                let mut first = self.graph.clone();
                for (_, _, new) in &report.replaced_sources {
                    let _ = first.set_state(*new, Lifecycle::Standby);
                    let _ = first.remove_node(*new);
                }
                let applied = self.engine.apply(&first)?;
                failed.extend(engine_failures(&applied));
                for e in gone.iter_mut() {
                    if !take_instance(&mut self.engine, e, &mut self.pending_takes) {
                        failed.push(StartFailure {
                            node: "source".to_owned(),
                            reason: format!("{}: did not close yet", e.state.name),
                        });
                    }
                }
            }
        }

        // Instances for the sources the plan schedules (only those are opened).
        let plan = self.graph.plan();
        let scheduled: BTreeSet<NodeId> = plan.order.iter().copied().collect();
        let ctx = InstanceContext::new(self.engine.clock(), self.settings.audio_graph);
        for entry in &mut self.entries {
            entry.scheduled = scheduled.contains(&entry.state.node);
            if !entry.scheduled || entry.instance.is_some() {
                continue;
            }
            // Break before make: the instance it replaces still holds the device.
            if self
                .pending_takes
                .iter()
                .any(|(_, s)| *s == Some(entry.source))
            {
                entry.state.failed = Some(WAITING.to_owned());
                continue;
            }
            let Some(source) = collection.source(entry.source) else {
                continue;
            };
            let instance = match sources.instantiate(source, &ctx) {
                Ok(i) => {
                    entry.state.failed = None;
                    i
                }
                Err(error) if strictness == Strictness::Strict => {
                    return Err(RecordError::Source {
                        name: entry.state.name.clone(),
                        error,
                    });
                }
                Err(error) => {
                    entry.state.failed = Some(error.to_string());
                    continue;
                }
            };
            match instance {
                SourceInstance::Video(v) => {
                    entry.state.adapter = v.adapter;
                    entry.state.target = v.target.as_str().to_owned();
                    self.engine
                        .adopt::<GpuRuntime>(RENDER, entry.state.node, Box::new(v.node))?;
                    entry.instance = Some(Instance {
                        partition: RENDER,
                        gpu: true,
                        watch: Watch::Video(v.stats),
                        offset: None,
                    });
                }
                SourceInstance::Audio(a) => {
                    entry.state.adapter = a.adapter;
                    entry.state.target = a.endpoint.as_str().to_owned();
                    let meter = Arc::clone(self.meters.entry(entry.source).or_insert_with(|| {
                        Arc::new(LevelMeter::new(
                            self.settings.audio_graph.format.channels.channels(),
                        ))
                    }));
                    let partition = self.settings.partitions.audio;
                    let mut offset = a.offset;
                    // Sent before the node runs, so its first block already has it.
                    if let Some(&ns) = self.settings.audio_offsets.get(&entry.source) {
                        offset.send(input_params(ns));
                    }
                    self.engine
                        .adopt::<()>(partition, entry.state.node, Box::new(a.node))?;
                    entry.instance = Some(Instance {
                        partition,
                        gpu: false,
                        watch: Watch::Audio(a.stats, meter),
                        offset: Some(offset),
                    });
                }
                _ => {
                    if strictness == Strictness::Strict {
                        return Err(RecordError::UnsupportedSource(entry.state.name.clone()));
                    }
                    entry.state.failed = Some("unsupported instance kind".to_owned());
                }
            }
        }

        // Layers in link order of the final plan, strips per mixer input.
        let _ = self.layers.send(node_layers(&self.bound, &plan));
        // Strips: until the swap, the old schedule may still feed inputs that leave, so
        // the strips sent before the apply cover the old and the new inputs; the ones of
        // inputs that left go after it.
        let mut trimmed = None;
        if let (Some(mixer), Some(bound)) = (&mut self.mixer, &self.bound_mixer) {
            let live: BTreeSet<SourceId> = self.entries.iter().map(|e| e.source).collect();
            self.meters.retain(|s, _| live.contains(s));
            let new: Vec<(NodeId, Option<Arc<LevelMeter>>)> = bound
                .inputs
                .iter()
                .map(|i| (i.producer, self.meters.get(&i.source).cloned()))
                .collect();
            let mut union = new.clone();
            union.extend(
                self.strips
                    .iter()
                    .filter(|(p, _)| !new.iter().any(|(n, _)| n == p))
                    .cloned(),
            );
            let _ = mixer.params.send(mixer_params(&union, &mixer.master));
            if union.len() != new.len() {
                trimmed = Some(mixer_params(&new, &mixer.master));
            }
            self.strips = new;
        }

        let applied = self.engine.apply(&self.graph);
        if let (Some(params), Some(mixer)) = (trimmed, &mut self.mixer) {
            let _ = mixer.params.send(params);
        }
        let applied = applied?;
        failed.extend(engine_failures(&applied));
        for report in applied.reports.values() {
            for (node, why) in &report.failed {
                let role = self.essential.get(node).copied().or_else(|| {
                    self.recording
                        .as_ref()
                        .and_then(|r| r.essential.get(node).copied())
                });
                if let Some(role) = role {
                    failed.push(StartFailure {
                        node: role.to_owned(),
                        reason: why.to_string(),
                    });
                } else if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node) {
                    // The first reason stays: a source without an instance is reported
                    // again on every apply as having none.
                    if e.state.failed.is_none() {
                        e.state.failed = Some(why.to_string());
                        failed.push(StartFailure {
                            node: "source".to_owned(),
                            reason: format!("{}: {why}", e.state.name),
                        });
                    }
                }
            }
        }
        for e in gone.iter_mut() {
            take_instance(&mut self.engine, e, &mut self.pending_takes);
        }
        Ok((failed, self.problems.clone()))
    }

    /// The bound sources the running plan schedules, in binder order, each with its
    /// adapter, target and why it is not running (if it is not).
    pub fn sources(&self) -> Vec<SourceState> {
        self.entries
            .iter()
            .filter(|e| e.scheduled)
            .map(|e| e.state.clone())
            .collect()
    }

    /// True while a source waits for the instance it replaces to close (its partition's
    /// swap was still in flight). [`Studio::service`] takes such instances back; once
    /// this is false, an `update` with the same collection opens the waiting sources.
    pub fn waiting(&self) -> bool {
        self.pending_takes.iter().any(|(_, s)| s.is_some())
    }

    /// Binder problems of the last start or update.
    pub fn problems(&self) -> &[BindProblem] {
        &self.problems
    }

    /// Where a UI connects to the preview, once the preview runs.
    pub fn preview(&self) -> Option<PreviewEndpoint> {
        self.preview.as_ref().and_then(|p| p.endpoint())
    }

    /// The preview's counters, if there is a preview.
    pub fn preview_stats(&self) -> Option<&Arc<PreviewNodeStats>> {
        self.preview.as_ref()
    }

    /// The settings the studio runs with.
    pub fn settings(&self) -> &StudioSettings {
        &self.settings
    }

    /// The engine (its clock is the master clock).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Sets the sync offset of an audio source in ns: positive delays it, negative
    /// advances it, 0 removes it (ADR-0037). It is kept per source, so it also applies
    /// when the source opens later or is rebound after a settings change. A running
    /// input takes it on its next block: a change below 5 ms slews, a larger one
    /// resyncs once. Returns true if a running input of that source got it.
    pub fn set_audio_offset(&mut self, source: SourceId, offset_ns: i64) -> bool {
        if offset_ns == 0 {
            self.settings.audio_offsets.remove(&source);
        } else {
            self.settings.audio_offsets.insert(source, offset_ns);
        }
        let live = self
            .entries
            .iter_mut()
            .filter(|e| e.source == source)
            .filter_map(|e| e.instance.as_mut()?.offset.as_mut());
        let mut sent = false;
        for offset in live {
            offset.send(input_params(offset_ns));
            sent = true;
        }
        sent
    }

    /// The level meter of a source's mixer strip.
    pub fn meter(&self, source: SourceId) -> Option<&Arc<LevelMeter>> {
        self.meters.get(&source)
    }

    /// The mixer's master meter.
    pub fn master_meter(&self) -> Option<&Arc<LevelMeter>> {
        self.mixer.as_ref().map(|m| &m.master)
    }

    /// Mixer inputs that had no strip or an invalid block (both should stay 0).
    pub fn mixer_anomalies(&self) -> u64 {
        self.mixer
            .as_ref()
            .map_or(0, |m| m.stats.invalid_inputs() + m.stats.unknown_inputs())
    }

    /// The name of a node: a source name or a role such as `"video encoder"`.
    pub fn node_name(&self, node: NodeId) -> Option<&str> {
        self.essential
            .get(&node)
            .copied()
            .or_else(|| {
                self.recording
                    .as_ref()
                    .and_then(|r| r.essential.get(&node).copied())
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.state.node == node)
                    .map(|e| e.state.name.as_str())
            })
    }

    /// Runs the engine's control loop once and notes failures: a source that fails is
    /// marked in [`Studio::sources`] until it runs again; a studio node that fails for
    /// good sets [`Studio::failure`], a recording node [`Studio::recording_failure`].
    /// Call it again after `wait` (at most about 100 ms).
    pub fn service(&mut self) -> Serviced {
        let serviced = self.engine.service();
        if !self.pending_takes.is_empty() {
            let engine = &mut self.engine;
            self.pending_takes.retain(|(a, _)| !take(engine, *a));
        }
        for (_, event) in &serviced.events {
            match event {
                RuntimeEvent::Failed {
                    node,
                    cause,
                    retry_at,
                    ..
                } => {
                    if let Some(role) = self.essential.get(node) {
                        if retry_at.is_none() && self.failure.is_none() {
                            self.failure = Some(format!("{role}: {cause:?}"));
                        }
                    } else if let Some(r) = self
                        .recording
                        .as_mut()
                        .filter(|r| r.essential.contains_key(node))
                    {
                        if retry_at.is_none() && r.failure.is_none() {
                            let role = r.essential.get(node).copied().unwrap_or("recording");
                            r.failure = Some(format!("{role}: {cause:?}"));
                        }
                    } else if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node)
                    {
                        e.state.failed = Some(format!("{cause:?}"));
                    }
                }
                RuntimeEvent::Restarted { node, .. } | RuntimeEvent::Recovered { node } => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node) {
                        e.state.failed = None;
                    }
                }
                _ => {}
            }
        }
        serviced
    }

    /// Why the studio cannot go on: its compositor, mixer, preview or sink failed for good.
    pub fn failure(&self) -> Option<String> {
        self.failure.clone()
    }

    /// True while a recording runs.
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// What the running recording chose and records.
    pub fn recording(&self) -> Option<&StartReport> {
        self.recording.as_ref().map(|r| &r.report)
    }

    /// Why the running recording cannot go on: the output's first permanent failure, or
    /// one of its nodes failed for good. The composition root then stops it.
    pub fn recording_failure(&self) -> Option<String> {
        let r = self.recording.as_ref()?;
        r.output
            .failure
            .get()
            .cloned()
            .or_else(|| r.failure.clone())
    }

    /// Starts recording the canvas to `out` while the studio runs. Adapters are chosen
    /// first (`NoAdapter` before anything opens); then a video export, the video encoder,
    /// the audio encoder (with `out.audio`) and an output are added and applied. Sources
    /// only now scheduled (without a preview) are opened. If one of the recording's
    /// nodes does not start, they are all removed again and the call fails with
    /// `Start`; the studio keeps running. The output may have created the destination.
    pub fn start_recording(
        &mut self,
        adapters: &RecordAdapters,
        out: &OutputSettings,
    ) -> Result<&StartReport, RecordError> {
        if self.recording.is_some() {
            return Err(RecordError::AlreadyRecording);
        }
        if out.audio.is_some() && self.mixer.is_none() {
            return Err(RecordError::NotBound("mixer"));
        }
        let prepared = Prepared::choose(
            adapters,
            out,
            self.size,
            self.fps,
            self.settings.audio_graph,
        )?;
        self.attach(prepared, &adapters.sources, Strictness::Lenient)
    }

    /// Adds a prepared recording's nodes to the graph and adopts their instances; applies
    /// nothing. All or nothing: on an error the graph and the partitions are as before.
    fn add_recording(&mut self, p: Prepared) -> Result<(), RecordError> {
        let parts = self.settings.partitions;
        let mut graph = self.graph.clone();
        let mut nodes = Vec::new();
        let mut essential = BTreeMap::new();
        let export_stats = p.export.stats();
        let export_end = p.export.end();
        let export_id = add_active(
            &mut graph,
            Node::<GpuRuntime>::descriptor(&p.export),
            RENDER,
        )?;
        let encoder_stats = p.video_encoder.stats();
        let encoder_id = add_active(
            &mut graph,
            Node::<()>::descriptor(&p.video_encoder),
            parts.video_encoder,
        )?;
        let output_stats = p.output.stats();
        let output_id = add_active(&mut graph, Node::<()>::descriptor(&p.output), parts.output)?;
        for (node, partition, gpu, role) in [
            (export_id, RENDER, true, "video export"),
            (encoder_id, parts.video_encoder, false, "video encoder"),
            (output_id, parts.output, false, "output"),
        ] {
            nodes.push(Added {
                node,
                partition,
                gpu,
            });
            essential.insert(node, role);
        }
        graph.connect(self.compositor, "canvas", export_id, "canvas")?;
        graph.connect(export_id, "video", encoder_id, "video")?;
        graph.connect(encoder_id, "packets", output_id, "s0")?;
        let mut audio = None;
        if let Some((node, chosen)) = p.audio {
            let mixer = self
                .mixer
                .as_ref()
                .map(|m| m.node)
                .ok_or(RecordError::NotBound("mixer"))?;
            let id = add_active(
                &mut graph,
                Node::<()>::descriptor(&node),
                parts.audio_encoder,
            )?;
            graph.connect(mixer, "main", id, "in")?;
            graph.connect(id, "packets", output_id, "s1")?;
            nodes.push(Added {
                node: id,
                partition: parts.audio_encoder,
                gpu: false,
            });
            essential.insert(id, "audio encoder");
            audio = Some((id, node, chosen));
        }

        // Instances last; one that is refused takes the adopted ones back.
        let audio_dropped = audio.as_ref().map(|(_, node, _)| node.dropped());
        let audio_tail = audio
            .as_ref()
            .map(|(_, node, _)| (node.flushes(), node.tail_lost()));
        let mut adopted = Vec::new();
        let mut adopt =
            |engine: &mut Engine, a: Added, r: Result<(), rbs_engine::EngineError>| match r {
                Ok(()) => {
                    adopted.push(a);
                    Ok(())
                }
                Err(e) => {
                    for a in adopted.drain(..) {
                        take(engine, a);
                    }
                    Err(RecordError::from(e))
                }
            };
        let r = self
            .engine
            .adopt::<GpuRuntime>(RENDER, export_id, Box::new(p.export));
        adopt(&mut self.engine, nodes[0], r)?;
        let r = self
            .engine
            .adopt::<()>(parts.video_encoder, encoder_id, Box::new(p.video_encoder));
        adopt(&mut self.engine, nodes[1], r)?;
        let r = self
            .engine
            .adopt::<()>(parts.output, output_id, Box::new(p.output));
        adopt(&mut self.engine, nodes[2], r)?;
        let mut audio_chosen = None;
        if let Some((id, node, chosen)) = audio {
            let r = self
                .engine
                .adopt::<()>(parts.audio_encoder, id, Box::new(node));
            adopt(&mut self.engine, nodes[3], r)?;
            audio_chosen = Some(chosen);
        }

        self.graph = graph;
        self.recording = Some(Attached {
            report: StartReport {
                video_encoder: p.video_chosen,
                audio_encoder: audio_chosen,
                muxer: p.muxer,
                transport: p.transport,
                sources: Vec::new(),
                problems: self.problems.clone(),
            },
            nodes,
            essential,
            failure: None,
            export: export_stats,
            end: export_end,
            encoder: encoder_stats,
            audio_dropped,
            audio_tail,
            output: output_stats,
            queues: Queues::default(),
        });
        Ok(())
    }

    /// Adds a prepared recording to the running graph and applies it. If a node of the
    /// recording, the engine or a link fails, or (`Strict`) a source cannot be
    /// instantiated, the recording is removed again and the call fails.
    pub(crate) fn attach(
        &mut self,
        p: Prepared,
        sources: &SourceTypes,
        strictness: Strictness,
    ) -> Result<&StartReport, RecordError> {
        self.add_recording(p)?;
        // Sources scheduled only now (no preview) are opened; then one apply.
        let failed = match self.reconcile(sources, None, strictness) {
            Ok((failed, _)) => failed,
            Err(e) => {
                let _ = self.detach(false);
                return Err(e);
            }
        };
        let recording_essential = self
            .recording
            .as_ref()
            .map(|r| r.essential.clone())
            .unwrap_or_default();
        let fatal: Vec<StartFailure> = failed
            .into_iter()
            .filter(|f| {
                f.node != "source"
                    && (recording_essential.values().any(|r| *r == f.node)
                        || f.node == "engine"
                        || f.node.starts_with("link"))
            })
            .collect();
        if !fatal.is_empty() {
            let mut fatal = fatal;
            for e in self.detach(false).1 {
                fatal.push(StartFailure {
                    node: "rollback".to_owned(),
                    reason: e,
                });
            }
            return Err(RecordError::Start(fatal));
        }
        let sources = self.sources();
        let r = self
            .recording
            .as_mut()
            .ok_or(RecordError::NotBound("recording"))?;
        r.report.sources = sources;
        // The encoders' inputs cross partitions; their queues exist from this apply on.
        let queue_to = |node: Option<&Added>| {
            let node = node?.node;
            self.engine
                .crossings()
                .iter()
                .find(|c| c.link.to.node == node)
                .map(|c| Arc::clone(&c.stats))
        };
        r.queues = Queues {
            video: queue_to(r.nodes.get(1)),
            audio: queue_to(r.nodes.get(3)),
        };
        Ok(&r.report)
    }

    /// Removes the recording's nodes from the graph and applies, draining the encoder
    /// and output partitions first when `drain`; takes their instances back. Returns the
    /// engine's errors.
    fn detach(&mut self, drain: bool) -> (Option<Attached>, Vec<String>) {
        let Some(r) = self.recording.take() else {
            return (None, Vec::new());
        };
        let nodes = r.nodes.clone();
        for n in &nodes {
            let _ = self.graph.set_state(n.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(n.node);
        }
        let applied = if drain {
            self.engine.apply_draining(&self.graph)
        } else {
            self.engine.apply(&self.graph)
        };
        let errors: Vec<String> = match applied {
            Ok(a) => a.errors.iter().map(ToString::to_string).collect(),
            Err(e) => vec![e.to_string()],
        };
        for n in nodes {
            if !take(&mut self.engine, n) {
                self.pending_takes.push((n, None));
            }
        }
        // The plan without the recording may leave sources unscheduled (no preview);
        // the layer hints follow the new plan.
        let plan = self.graph.plan();
        let scheduled: BTreeSet<NodeId> = plan.order.iter().copied().collect();
        for e in &mut self.entries {
            e.scheduled = scheduled.contains(&e.state.node);
        }
        let _ = self.layers.send(node_layers(&self.bound, &plan));
        (Some(r), errors)
    }

    /// Ends the running recording's video now, on the master clock (ADR-0045): the
    /// export takes no canvas after this time and keeps passing on the frames it still
    /// reads back. Call [`Studio::stop_recording`] once [`Studio::recording_tail_done`]
    /// is true, so the last frames reach the file; until then the studio and the
    /// recording keep running (call [`Studio::service`] as usual). False when nothing
    /// records. A second call keeps the first end.
    pub fn end_recording(&mut self) -> bool {
        let Some(r) = self.recording.as_ref() else {
            return false;
        };
        r.end.end_at(self.engine.clock().now());
        true
    }

    /// True when [`Studio::stop_recording`] loses no video frame up to the end: after
    /// [`Studio::end_recording`], once the export has passed on every canvas it took up
    /// to the end and the video encoder has taken the last of them, or once
    /// [`END_TAIL_LIMIT`] has passed on the master clock. True when nothing
    /// records; false while a recording runs that was not ended.
    pub fn recording_tail_done(&self) -> bool {
        let Some(r) = self.recording.as_ref() else {
            return true;
        };
        match r.end.requested() {
            None => false,
            Some(at) => {
                // End to end: the export passed on its last frame, and the encoder took
                // it (the queue between them keeps only the newest frame).
                let delivered = r.end.ended()
                    && r.encoder.last_frame.load(Ordering::Relaxed)
                        >= r.export.last_frame.load(Ordering::Relaxed);
                delivered || self.engine.clock().now() >= at + END_TAIL_LIMIT
            }
        }
    }

    /// Stops the running recording: drains its encoders and output in pipeline order
    /// (the output writes the container's trailer and finishes the transport) and removes
    /// them, while the studio keeps running. `None` when nothing records.
    pub fn stop_recording(&mut self) -> Option<FinishReport> {
        let (r, errors) = self.detach(true);
        let r = r?;
        let progress = self.progress_of(Some(&r));
        let failure = r
            .output
            .failure
            .get()
            .cloned()
            .or_else(|| r.failure.clone());
        let flush_lost = r.encoder.flush_lost.load(Ordering::Relaxed);
        let finish_lost = r.output.finish_lost.load(Ordering::Relaxed);
        Some(FinishReport {
            progress,
            failure,
            shutdown_errors: errors,
            flush_lost,
            finish_lost,
        })
    }

    /// The running output's connection; `None` when nothing records or streams.
    /// Cheap (a few atomic loads) and, unlike [`Studio::progress`], takes no meter peaks.
    pub fn link(&self) -> Option<Link> {
        let o = &self.recording.as_ref()?.output;
        Some(Link {
            connected: o.connected.load(Ordering::Relaxed),
            connects: o.connects.load(Ordering::Relaxed),
            reconnect_attempts: o.reconnect_attempts.load(Ordering::Relaxed),
            transport_drops: o.transport_drops.load(Ordering::Relaxed),
        })
    }

    /// Counters now: sources, and the recording's when one runs. Peak levels are those
    /// since the previous call.
    pub fn progress(&self) -> Progress {
        self.progress_of(self.recording.as_ref())
    }

    fn progress_of(&self, r: Option<&Attached>) -> Progress {
        let sources = self
            .entries
            .iter()
            .filter(|e| e.scheduled)
            .filter_map(|e| e.instance.as_ref().map(|i| (e, i)))
            .map(|(e, i)| match &i.watch {
                Watch::Video(v) => SourceProgress {
                    name: e.state.name.clone(),
                    frames: v.frames.load(Ordering::Relaxed),
                    peak_db: None,
                    resyncs: 0,
                    errors: v.gpu_errors.load(Ordering::Relaxed),
                },
                Watch::Audio(a, meter) => {
                    let c = a.read();
                    SourceProgress {
                        name: e.state.name.clone(),
                        frames: c.frames_in,
                        peak_db: Some(peak_db(meter)),
                        resyncs: c.resyncs,
                        errors: c.read_errors,
                    }
                }
            })
            .collect();
        let mut p = Progress {
            sources,
            late_render_ticks: self.render_worker.as_ref().map_or(0, |w| w.late()),
            late_audio_ticks: self.audio_worker.as_ref().map_or(0, |w| w.late()),
            master_peak_db: self.mixer.as_ref().map(|m| peak_db(&m.master)),
            ..Progress::default()
        };
        if let Some(r) = r {
            let e = &r.encoder;
            p.frames_exported = r.export.frames.load(Ordering::Relaxed);
            p.export_drops = sum(&[&r.export.busy, &r.export.lost]);
            p.frames_encoded = e.submitted.load(Ordering::Relaxed);
            p.encoder_drops = sum(&[&e.busy_drops, &e.refused, &e.overflow_drops]);
            p.encoder_skipped = r.queues.video.as_ref().map_or(0, |q| q.video_drops());
            p.audio_queue_drops = r.queues.audio.as_ref().map_or(0, |q| q.overflows());
            p.audio_packet_drops = r
                .audio_dropped
                .as_ref()
                .map_or(0, |d| d.load(Ordering::Relaxed));
            if let Some((flushes, lost)) = &r.audio_tail {
                p.audio_flushes = flushes.load(Ordering::Relaxed);
                p.audio_tail_lost = lost.load(Ordering::Relaxed);
            }
            p.packets_taken = r.output.packets.load(Ordering::Relaxed);
            p.mux_drops = r.output.mux_drops.load(Ordering::Relaxed);
            p.start_drops = r.output.start_drops.load(Ordering::Relaxed);
            p.backlog_peak = r.output.backlog_peak.load(Ordering::Relaxed);
            p.transport_drops = r.output.transport_drops.load(Ordering::Relaxed);
            p.connects = r.output.connects.load(Ordering::Relaxed);
            p.reconnect_attempts = r.output.reconnect_attempts.load(Ordering::Relaxed);
            p.connected = r.output.connected.load(Ordering::Relaxed);
        }
        p
    }

    /// Stops any recording, then the engine. Returns the recording's report, if one ran,
    /// and the shutdown's errors.
    pub fn shutdown(&mut self) -> (Option<FinishReport>, Vec<String>) {
        let report = self.stop_recording();
        let errors = self
            .engine
            .shutdown()
            .errors
            .iter()
            .map(ToString::to_string)
            .collect();
        (report, errors)
    }
}

/// Engine errors and links that could not cross partitions, as failures.
/// The input parameters for a sync offset in ns (positive delays).
fn input_params(offset_ns: i64) -> InputParams {
    let d = Duration::from_nanos(offset_ns.unsigned_abs());
    if offset_ns < 0 {
        InputParams::advanced(d)
    } else {
        InputParams::delayed(d)
    }
}

fn engine_failures(applied: &Applied) -> Vec<StartFailure> {
    let mut out = Vec::new();
    for error in &applied.errors {
        out.push(StartFailure {
            node: "engine".to_owned(),
            reason: error.to_string(),
        });
    }
    for (link, problem) in &applied.problems {
        out.push(StartFailure {
            node: format!("link {link:?}"),
            reason: format!("{problem:?}"),
        });
    }
    out
}

/// Takes a retired source's instance back from its partition (it is dropped here). A
/// take that fails is queued in `pending` with the source; returns whether it was taken.
fn take_instance(
    engine: &mut Engine,
    entry: &mut Entry,
    pending: &mut Vec<(Added, Option<SourceId>)>,
) -> bool {
    let Some(i) = entry.instance.take() else {
        return true;
    };
    let a = Added {
        node: entry.state.node,
        partition: i.partition,
        gpu: i.gpu,
    };
    if take(engine, a) {
        true
    } else {
        pending.push((a, Some(entry.source)));
        false
    }
}

/// Takes an idle instance back from its partition and drops it. False when the
/// partition still runs it (its swap is in flight): try again later. An instance that
/// is not there any more counts as taken.
fn take(engine: &mut Engine, a: Added) -> bool {
    let r = if a.gpu {
        engine.take::<GpuRuntime>(a.partition, a.node).map(drop)
    } else {
        engine.take::<()>(a.partition, a.node).map(drop)
    };
    r.is_ok()
}

/// The mixer's parameters for `strips` (gain 1) and the master meter.
fn mixer_params(
    strips: &[(NodeId, Option<Arc<LevelMeter>>)],
    master: &Arc<LevelMeter>,
) -> MixerParams {
    let strips = strips
        .iter()
        .map(|(producer, meter)| {
            let strip = Strip::new(*producer, 1.0);
            match meter {
                Some(m) => strip.with_meter(Arc::clone(m)),
                None => strip,
            }
        })
        .collect();
    MixerParams::new(strips).with_master_meter(Arc::clone(master))
}
