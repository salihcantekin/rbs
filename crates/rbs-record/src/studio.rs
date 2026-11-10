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
use rbs_audio_filters::{
    AudioFilterNode, AudioFilterStats, AudioFilterType, DspSetup, FilterMeter,
};
use rbs_codec::{AudioEncoderConfig, EncoderTuning, VideoEncoderConfig};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ContainerId, MemoryKind, PixelFormat, StreamDescriptor};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_engine::{Applied, DEFAULT_DOORBELL_TIMER, Engine, Pacing, Serviced, WorkerStats};
use rbs_filters::{FilterNode, FilterStats, FilterType, Values};
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
use rbs_nodes::{
    OutputNode, PreviewNode, PreviewNodeStats, VideoEncoderNode, VideoExportNode,
    VirtualCameraNode, VirtualCameraNodeStats,
};
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindProblem, BindReport, BoundCompositor, BoundMixer, Role, SceneBinder};
use rbs_scene::model::{CanvasId, FilterId, MasterAudio, SceneId, SourceAudio, SourceId};
use rbs_sources::{
    InstanceContext, SourceInstance, SourceTypes, filter_values_lenient, node_layers,
};
use rbs_transport::{ReconnectPolicy, Secret, TransportConfig, TransportInput};
use rbs_vcam::{CameraFormat, VcamError, VirtualCameraConfig, VirtualCameraFactory};

use crate::recording::{
    AudioSettings, FinishReport, Partitions, Progress, RENDER, RecordAdapters, RecordError,
    SourceProgress, SourceState, StartFailure, StartReport, VideoSettings, add_active, is_fatal,
    peak_db, sum,
};
use crate::select::{self, Chosen};

mod replay;
pub use replay::ReplaySettings;

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
/// [`Studio::end_output`]; afterwards [`Studio::output_tail_done`] is true anyway
/// (a stalled readback must not hold the stop up, ADR-0045).
pub const END_TAIL_LIMIT: Duration = Duration::from_millis(500);

/// Canvases a studio serves at once, the main one included (ADR-0093 decision 1).
pub const MAX_CANVASES: usize = 2;

/// A further canvas the studio runs beside the main one (ADR-0093): its own compositor
/// and picture; sources and their filter chains are shared with the main canvas.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtraCanvas {
    /// The canvas. It needs a program scene and the main canvas's frame rate.
    pub canvas: CanvasId,
    /// Its picture for a UI. `None`: no picture, and the canvas is only composed while
    /// an output of it runs (P13): a closed panel costs nothing.
    pub preview: Option<PreviewDesc>,
}

impl ExtraCanvas {
    /// `canvas` without a picture; outputs can still take it.
    pub fn new(canvas: CanvasId) -> Self {
        Self {
            canvas,
            preview: None,
        }
    }

    /// The same canvas with a picture described by `preview`.
    pub fn with_preview(mut self, preview: PreviewDesc) -> Self {
        self.preview = Some(preview);
        self
    }
}

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
    /// In studio mode, a second picture of `preview`'s size for the preview scene
    /// (ADR-0061). Off: the preview scene is not rendered (the plan leaves its
    /// compositor out). Only a UI that can show two pictures sets it.
    pub scene_preview: bool,
    /// Sync offset in ns of each audio source (ADR-0037): positive delays the source,
    /// negative advances it. Sources not listed have none. The audio input clamps it to
    /// its range (`rbs_sources::DEFAULT_MAX_AUDIO_OFFSET` at most).
    pub audio_offsets: BTreeMap<SourceId, i64>,
    /// Further canvases run beside `canvas` (ADR-0093), at most
    /// [`MAX_CANVASES`]` - 1`, each at the main canvas's frame rate. Empty: one canvas,
    /// nothing of the others runs.
    pub extra_canvases: Vec<ExtraCanvas>,
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
            scene_preview: false,
            audio_offsets: BTreeMap::new(),
            extra_canvases: Vec::new(),
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
    /// The canvas this output takes (ADR-0093): the main canvas or one of
    /// [`StudioSettings::extra_canvases`]. `None`: the main canvas.
    pub canvas: Option<CanvasId>,
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
            canvas: None,
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

/// One bound filter (ADR-0062 decision 8).
struct FilterEntry {
    filter: FilterId,
    source: SourceId,
    node: NodeId,
    /// Its running instance.
    instance: Option<FilterInstance>,
}

/// A running filter: the value channel the studio keeps and what the node shares
/// (ADR-0062 decision 8; ADR-0082 decision 8 for audio filters).
enum FilterInstance {
    /// A video filter on the render partition.
    Video(ParamSender<Values>, Arc<FilterStats>),
    /// An audio filter on the audio partition, with its meter if it has one.
    Audio {
        sender: ParamSender<Values>,
        meter: Option<Arc<FilterMeter>>,
        stats: Arc<AudioFilterStats>,
    },
}

impl FilterInstance {
    fn sender(&mut self) -> &mut ParamSender<Values> {
        match self {
            FilterInstance::Video(sender, _) | FilterInstance::Audio { sender, .. } => sender,
        }
    }
}

/// The built-in type of a filter: video or audio (ADR-0082 decision 8).
#[derive(Clone, Copy)]
enum FilterKind {
    Video(&'static dyn FilterType),
    Audio(&'static dyn AudioFilterType),
}

impl FilterKind {
    /// The stored settings as values, and the keys that had to be adjusted.
    fn values_lenient(self, settings: &rbs_scene::model::Settings) -> (Values, Vec<String>) {
        match self {
            FilterKind::Video(ty) => filter_values_lenient(ty, settings),
            FilterKind::Audio(ty) => filter_values_lenient(ty, settings),
        }
    }
}

/// The key of a [`FilterSettingsAdjusted`] that means the filter as a whole could not
/// be made, so its source plays unfiltered.
pub const FILTER_UNAVAILABLE: &str = "*";

/// A filter's stored settings that were adjusted to run it (ADR-0062 decision 8): the
/// key was unknown, out of range or of the wrong kind. The document keeps them until
/// the user edits that filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSettingsAdjusted {
    /// The filter.
    pub filter: FilterId,
    /// The setting's key.
    pub key: String,
}

/// A node the studio added to the graph, so it can be taken back.
#[derive(Debug, Clone, Copy)]
struct Added {
    node: NodeId,
    partition: PartitionId,
    gpu: bool,
}

/// Outputs a studio runs at once (ADR-0047): a recording and a live stream.
pub const MAX_OUTPUTS: usize = 2;

/// What the virtual camera announces and RBS sends (ADR-0066 decision 7): NV12 at this
/// size (both even) and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VirtualCameraSettings {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second the camera announces; RBS sends at this rate, or at the canvas
    /// rate when that is lower.
    pub rate: FrameRate,
}

impl VirtualCameraSettings {
    /// A camera of `width x height` at `rate`.
    pub fn new(width: u32, height: u32, rate: FrameRate) -> Self {
        Self {
            width,
            height,
            rate,
        }
    }
}

/// The virtual camera while it sends ([`Studio::virtual_camera`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VirtualCameraState {
    /// The configuration its session was opened with.
    pub config: VirtualCameraConfig,
    /// Its node's counters: frames handed to the session.
    pub stats: Arc<VirtualCameraNodeStats>,
    /// The session's counters, when the adapter shares them: what its sender did with
    /// the frames (`sent` to the consumer side, `consumer_absent` while no app reads,
    /// `format_pending`; its `consumer_busy` includes the node's own skips).
    pub session: Option<rbs_vcam::VcamCountersReader>,
    /// Why its node failed, until it runs again. The studio runs on either way.
    pub failure: Option<String>,
}

/// The virtual camera's node while it sends.
struct CameraRun {
    node: Added,
    config: VirtualCameraConfig,
    stats: Arc<VirtualCameraNodeStats>,
    session: Option<rbs_vcam::VcamCountersReader>,
    failure: Option<String>,
}

/// One of a studio's running outputs, from [`Studio::start_output`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(u64);

/// The video half of a prepared output: a new export and encoder, or a running
/// encoder chain shared with another output.
enum VideoPlan {
    New {
        export: Box<VideoExportNode>,
        encoder: Box<VideoEncoderNode>,
        chosen: Chosen,
        settings: VideoSettings,
    },
    Shared(usize),
}

/// The audio half of a prepared output.
enum AudioPlan {
    None,
    New {
        node: Box<EncoderNode>,
        chosen: Chosen,
        settings: AudioSettings,
        stream: StreamDescriptor,
    },
    Shared(usize),
}

/// The encoder configuration of `video` for frames of `format` at `fps`.
fn video_encoder_config(
    video: &VideoSettings,
    format: rbs_core::media::VideoFormat,
    fps: FrameRate,
) -> VideoEncoderConfig {
    let config = VideoEncoderConfig::new(
        video.codec,
        format,
        fps,
        MemoryKind::System,
        video.rate_control,
    )
    .with_keyframe_interval(video.keyframe_interval)
    .with_max_b_frames(video.max_b_frames)
    .with_tuning({
        let mut tuning = EncoderTuning::default();
        tuning.preset = video.preset;
        tuning
    });
    match &video.profile {
        Some(profile) => config.with_profile(profile.clone()),
        None => config,
    }
}

/// Adapters chosen for an output, nothing in the graph yet.
pub(crate) struct Prepared {
    /// The canvas the video comes from.
    canvas: CanvasId,
    video: VideoPlan,
    audio: AudioPlan,
    output: OutputNode,
    muxer: Option<Chosen>,
    transport: Chosen,
}

impl Prepared {
    /// Chooses every adapter of an output of a `width x height` canvas at `fps`; opens
    /// only the encoder sessions it does not share (their stream descriptors make the
    /// output). A running encoder in `running` is shared when the output's settings
    /// equal the ones it was opened with and its adapter is a candidate of this
    /// output's selection (ADR-0047 decision 3).
    pub(crate) fn choose(
        adapters: &RecordAdapters,
        out: &OutputSettings,
        canvas: CanvasId,
        (width, height): (u32, u32),
        fps: FrameRate,
        audio_graph: AudioGraphConfig,
        running: &Chains,
    ) -> Result<Self, RecordError> {
        // System memory: the export reads the canvas back (the P4 exception of
        // ADR-0038); native export will take the memory kind from the encoder.
        let export = VideoExportNode::new(out.video.format, width, height);
        let video_config = video_encoder_config(&out.video, export.frame_format(), fps);
        let shared_video = running.video.iter().position(|c| {
            c.as_ref().is_some_and(|c| {
                c.canvas == canvas
                    && c.settings == out.video
                    // An export asked to end takes no later canvas (ADR-0045).
                    && c.end.requested().is_none()
                    && select::is_video_candidate(
                        &adapters.video_encoders,
                        &video_config,
                        &c.chosen.adapter,
                    )
            })
        });
        let (video, video_stream) =
            match shared_video.and_then(|i| Some((i, running.video[i].as_ref()?))) {
                Some((i, chain)) => (VideoPlan::Shared(i), chain.stream.clone()),
                None => {
                    let (encoder, chosen) =
                        select::video_encoder(&adapters.video_encoders, &video_config)?;
                    let stream = encoder.stream().clone();
                    (
                        VideoPlan::New {
                            export: Box::new(export),
                            encoder: Box::new(encoder),
                            chosen,
                            settings: out.video.clone(),
                        },
                        stream,
                    )
                }
            };
        let mut streams = vec![video_stream];
        let audio = match &out.audio {
            Some(a) => {
                let config = AudioEncoderConfig::new(a.codec, audio_graph.format, a.bitrate_bps);
                let shared = running.audio.iter().enumerate().find_map(|(i, c)| {
                    let c = c.as_ref()?;
                    (c.settings == *a
                        && select::is_audio_candidate(
                            &adapters.audio_encoders,
                            &config,
                            &c.chosen.adapter,
                        ))
                    .then_some((i, c))
                });
                match shared {
                    Some((i, chain)) => {
                        streams.push(chain.stream.clone());
                        AudioPlan::Shared(i)
                    }
                    None => {
                        let (session, chosen) =
                            select::audio_encoder(&adapters.audio_encoders, &config)?;
                        let stream = session.stream().clone();
                        streams.push(stream.clone());
                        AudioPlan::New {
                            node: Box::new(EncoderNode::new(session, AUDIO_ENCODER_PACKETS)),
                            chosen,
                            settings: a.clone(),
                            stream,
                        }
                    }
                }
            }
            None => AudioPlan::None,
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
            canvas,
            video,
            audio,
            output,
            muxer,
            transport,
        })
    }
}

/// A running video export and encoder, used by one or more outputs.
struct VideoChain {
    /// The canvas it exports; an encoder is shared only between outputs of one canvas.
    canvas: CanvasId,
    settings: VideoSettings,
    chosen: Chosen,
    stream: StreamDescriptor,
    export: Added,
    encoder: Added,
    export_stats: Arc<ExportNodeStats>,
    /// Ends the export (ADR-0045); used only while one output takes the chain.
    end: Arc<ExportEnd>,
    encoder_stats: Arc<EncoderNodeStats>,
    /// The queue into the encoder's partition, found after the apply that scheduled it.
    queue: Option<Arc<QueueStats>>,
}

/// A running audio encoder, used by one or more outputs.
struct AudioChain {
    settings: AudioSettings,
    chosen: Chosen,
    stream: StreamDescriptor,
    encoder: Added,
    dropped: Arc<AtomicU64>,
    /// End-of-stream flushes and tail losses (ADR-0045).
    flushes: Arc<AtomicU64>,
    tail_lost: Arc<AtomicU64>,
    queue: Option<Arc<QueueStats>>,
}

/// The encoder chains running, by slot (the slot picks their partitions).
#[derive(Default)]
pub(crate) struct Chains {
    video: [Option<VideoChain>; MAX_OUTPUTS],
    audio: [Option<AudioChain>; MAX_OUTPUTS],
}

/// Encoder counters when an output joined a running chain; its progress counts from
/// there (ADR-0047 decision 7).
#[derive(Debug, Clone, Copy, Default)]
struct Base {
    frames_exported: u64,
    export_drops: u64,
    frames_encoded: u64,
    encoder_drops: u64,
    encoder_skipped: u64,
    audio_queue_drops: u64,
    audio_packet_drops: u64,
    audio_flushes: u64,
    audio_tail_lost: u64,
}

/// A running output, its encoder chains and counters.
struct Attached {
    id: OutputId,
    /// Its output slot (the partition of its output node).
    slot: usize,
    report: StartReport,
    output: Added,
    output_stats: Arc<OutputNodeStats>,
    /// Slots of the chains it takes packets from.
    video: usize,
    audio: Option<usize>,
    failure: Option<String>,
    export: Arc<ExportNodeStats>,
    encoder: Arc<EncoderNodeStats>,
    audio_dropped: Option<Arc<AtomicU64>>,
    /// The audio encoder's flushes and tail losses (ADR-0045).
    audio_tail: Option<(Arc<AtomicU64>, Arc<AtomicU64>)>,
    /// When [`Studio::end_output`] ended it, and whether it ended the export too (the
    /// chain was its alone) or only waits for the shared encoder to pass that time.
    ended: Option<(Timestamp, bool)>,
    /// The queues into the video and audio encoders' partitions, kept so the final
    /// counters survive the draining apply that removes them.
    queues: Queues,
    base: Base,
}

/// The cross-partition queues feeding an output's encoders.
#[derive(Default, Clone)]
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
/// [`Studio::start_output`] attaches an output (a recording or a live stream) to the
/// running graph, with a video export and encoders of its own or shared with a running
/// output whose settings match; up to [`MAX_OUTPUTS`] run at once (ADR-0047).
/// [`Studio::stop_output`] drains and removes what only that output used while the
/// preview, the meters and the other output keep running (`Engine::apply_draining`).
/// Control thread only; calls may block while devices open or workers swap.
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
    /// The preview scene's compositor and picture while the canvas is in studio mode
    /// (ADR-0061); `None` otherwise, so nothing of it runs.
    scene_preview: Option<ScenePreview>,
    /// The further canvases that run (ADR-0093), in `settings.extra_canvases` order.
    extras: Vec<ExtraRun>,
    /// The scene each following canvas showed last (the `KeepLast` fallback).
    last_follow: BTreeMap<CanvasId, SceneId>,
    entries: Vec<Entry>,
    problems: Vec<BindProblem>,
    /// Studio nodes without which it cannot run (compositor, mixer, preview, sink).
    essential: BTreeMap<NodeId, &'static str>,
    failure: Option<String>,
    /// The running outputs, in start order.
    outputs: Vec<Attached>,
    /// The replay buffer while armed (ADR-0088).
    replay: Option<replay::ReplayRun>,
    /// The encoder chains they use.
    chains: Chains,
    next_output: u64,
    /// Nodes the last apply reported as failed to join.
    failed_nodes: BTreeSet<NodeId>,
    render_worker: Option<Arc<WorkerStats>>,
    audio_worker: Option<Arc<WorkerStats>>,
    /// Retired instances whose take failed (their partition's swap was still in
    /// flight), with the source they belonged to; [`Studio::service`] retries them.
    pending_takes: Vec<(Added, Option<SourceId>)>,
    /// The mixer's strips last sent.
    strips: Vec<MixStrip>,
    /// Volume and mute per source, from the last [`Studio::update`] or
    /// [`Studio::set_mix`].
    mix: BTreeMap<SourceId, SourceAudio>,
    /// The canvas's master volume and mute (ADR-0081), from the same calls.
    master: MasterAudio,
    /// Bound filters, with an instance once scheduled.
    filters: Vec<FilterEntry>,
    /// Stored filter settings adjusted on load.
    filter_problems: Vec<FilterSettingsAdjusted>,
    /// The virtual camera while it sends (ADR-0066 decision 6); `None` otherwise, so
    /// nothing of it runs.
    camera: Option<CameraRun>,
}

impl fmt::Debug for Studio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Studio")
            .field("settings", &self.settings)
            .field("sources", &self.sources())
            .field("outputs", &self.outputs())
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
        let mut last_follow = BTreeMap::new();
        let followed = with_followers(collection, &mut last_follow);
        let collection = &followed;
        let canvas = collection
            .canvas(canvas_id)
            .ok_or(RecordError::UnknownCanvas(canvas_id))?;
        let program = canvas.program.ok_or(RecordError::NoProgram(canvas_id))?;
        let (width, height) = (canvas.size.width, canvas.size.height);
        let fps = canvas.frame_rate;
        let parts = settings.partitions;
        if settings.extra_canvases.len() + 1 > MAX_CANVASES {
            return Err(RecordError::TooManyCanvases);
        }
        for (i, extra) in settings.extra_canvases.iter().enumerate() {
            if extra.canvas == canvas_id
                || settings.extra_canvases[..i]
                    .iter()
                    .any(|e| e.canvas == extra.canvas)
            {
                return Err(RecordError::TooManyCanvases);
            }
            let other = collection
                .canvas(extra.canvas)
                .ok_or(RecordError::UnknownCanvas(extra.canvas))?;
            if other.program.is_none() {
                return Err(RecordError::NoProgram(extra.canvas));
            }
            if other.frame_rate != fps {
                return Err(RecordError::CanvasRate(extra.canvas));
            }
        }

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
        for p in (0..MAX_OUTPUTS)
            .map(|slot| parts.slot(slot))
            .chain([parts.replay])
        {
            engine.add_partition::<()>(
                p.video_encoder,
                Arc::new(()),
                (),
                Pacing::cadence_after(frames, fps.frame_duration() / 4),
            )?;
            engine.add_partition::<()>(p.output, Arc::new(()), (), Pacing::output())?;
        }
        if settings.audio {
            engine.add_partition::<()>(
                parts.audio,
                Arc::new(()),
                (),
                Pacing::stream(settings.audio_graph.cadence(clock.now()), AUDIO_CATCH_UP),
            )?;
            for p in (0..MAX_OUTPUTS)
                .map(|slot| parts.slot(slot))
                .chain([parts.replay])
            {
                engine.add_partition::<()>(
                    p.audio_encoder,
                    Arc::new(()),
                    (),
                    Pacing::doorbell(DEFAULT_DOORBELL_TIMER),
                )?;
            }
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

        let mut extras = Vec::new();
        for extra in &settings.extra_canvases {
            let bound = report
                .compositors
                .get(&(extra.canvas, Role::Program))
                .cloned()
                .ok_or(RecordError::NotBound("program compositor"))?;
            let size = collection
                .canvas(extra.canvas)
                .map(|c| (c.size.width, c.size.height))
                .ok_or(RecordError::UnknownCanvas(extra.canvas))?;
            let view = attach_compositor(&mut engine, &mut graph, bound, size, extra.preview)?;
            essential.insert(view.compositor, "compositor of a further canvas");
            if let Some((picture, _)) = &view.picture {
                essential.insert(*picture, "preview of a further canvas");
            }
            extras.push(ExtraRun {
                canvas: extra.canvas,
                size,
                view,
            });
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
                MixerParams::new(Vec::new())
                    .with_master(master_of(collection, canvas_id).gain())
                    .with_master_meter(Arc::clone(&master)),
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
            scene_preview: None,
            extras,
            last_follow,
            entries: Vec::new(),
            problems: Vec::new(),
            essential,
            failure: None,
            outputs: Vec::new(),
            replay: None,
            chains: Chains::default(),
            next_output: 0,
            failed_nodes: BTreeSet::new(),
            render_worker,
            audio_worker,
            pending_takes: Vec::new(),
            strips: Vec::new(),
            mix: mix_of(collection),
            master: master_of(collection, canvas_id),
            filters: Vec::new(),
            filter_problems: Vec::new(),
            camera: None,
        };
        if let Some(p) = recording
            && let Err(e) = studio.add_output(p)
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
                for r in &mut studio.outputs {
                    r.report.sources.clone_from(&sources);
                    r.report.problems.clone_from(&problems);
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
        self.update_with(collection, sources, None)
    }

    /// Starts running a further canvas while the studio runs (ADR-0093 decision 1,
    /// Standby to Active): the same checks as [`Studio::start`], then an
    /// [`Studio::update`] that adds its compositor and, with a preview description, its
    /// picture. Sources the canvas shows open as in any update; sources already open are
    /// shared. Fails with `TooManyCanvases`, `UnknownCanvas`, `NoProgram` or
    /// `CanvasRate` before anything changes.
    pub fn activate_canvas(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
        extra: ExtraCanvas,
    ) -> Result<Updated, RecordError> {
        if self.extras.len() + 2 > MAX_CANVASES
            || extra.canvas == self.settings.canvas
            || self.extras.iter().any(|e| e.canvas == extra.canvas)
        {
            return Err(RecordError::TooManyCanvases);
        }
        self.update_with(collection, sources, Some(extra))
    }

    /// Stops running a further canvas (Active to Standby): its compositor and picture
    /// leave the engine after the apply, and nothing of it is scheduled any more. The
    /// canvas stays in the collection. Fails with `CanvasBusy` while an output takes
    /// it (stop that first) and `UnknownCanvas` when it does not run.
    pub fn deactivate_canvas(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
        canvas: CanvasId,
    ) -> Result<Updated, RecordError> {
        let Some(at) = self.extras.iter().position(|e| e.canvas == canvas) else {
            return Err(RecordError::UnknownCanvas(canvas));
        };
        if self
            .chains
            .video
            .iter()
            .flatten()
            .any(|c| c.canvas == canvas)
        {
            return Err(RecordError::CanvasBusy(canvas));
        }
        let run = self.extras.remove(at);
        self.settings.extra_canvases.retain(|e| e.canvas != canvas);
        let mut leaving = vec![Added {
            node: run.view.compositor,
            partition: RENDER,
            gpu: true,
        }];
        self.essential.remove(&run.view.compositor);
        if let Some((picture, _)) = run.view.picture {
            if self.graph.node(picture).is_some() {
                let _ = self.graph.set_state(picture, Lifecycle::Standby);
                let _ = self.graph.remove_node(picture);
            }
            self.essential.remove(&picture);
            leaving.push(Added {
                node: picture,
                partition: RENDER,
                gpu: true,
            });
        }
        let updated = self.update_with(collection, sources, None);
        for a in leaving {
            if !take(&mut self.engine, a) {
                self.pending_takes.push((a, None));
            }
        }
        updated
    }

    fn update_with(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
        add: Option<ExtraCanvas>,
    ) -> Result<Updated, RecordError> {
        let followed = with_followers(collection, &mut self.last_follow);
        let collection = &followed;
        let canvas = collection
            .canvas(self.settings.canvas)
            .ok_or(RecordError::UnknownCanvas(self.settings.canvas))?;
        if (canvas.size.width, canvas.size.height) != self.size || canvas.frame_rate != self.fps {
            return Err(RecordError::NotBound("canvas of the same size and rate"));
        }
        for extra in &self.extras {
            let other = collection
                .canvas(extra.canvas)
                .ok_or(RecordError::UnknownCanvas(extra.canvas))?;
            if (other.size.width, other.size.height) != extra.size || other.frame_rate != self.fps {
                return Err(RecordError::NotBound("canvas of the same size and rate"));
            }
            if other.program.is_none() {
                return Err(RecordError::NoProgram(extra.canvas));
            }
        }
        // Checked before anything changes: without a program scene the binder would
        // retire the compositor the studio runs.
        if canvas.program.is_none() {
            return Err(RecordError::NoProgram(self.settings.canvas));
        }
        let fps = self.fps;
        let new = match add {
            Some(extra) => {
                let other = collection
                    .canvas(extra.canvas)
                    .ok_or(RecordError::UnknownCanvas(extra.canvas))?;
                if other.program.is_none() {
                    return Err(RecordError::NoProgram(extra.canvas));
                }
                if other.frame_rate != fps {
                    return Err(RecordError::CanvasRate(extra.canvas));
                }
                Some((extra, (other.size.width, other.size.height)))
            }
            None => None,
        };
        let report = self
            .binder
            .sync(collection, &mut self.graph)
            .map_err(RecordError::Bind)?;
        if let Some((extra, size)) = new {
            let bound = report
                .compositors
                .get(&(extra.canvas, Role::Program))
                .cloned()
                .ok_or(RecordError::NotBound("program compositor"))?;
            let view = attach_compositor(
                &mut self.engine,
                &mut self.graph,
                bound,
                size,
                extra.preview,
            )?;
            self.essential
                .insert(view.compositor, "compositor of a further canvas");
            if let Some((picture, _)) = &view.picture {
                self.essential
                    .insert(*picture, "preview of a further canvas");
            }
            self.settings.extra_canvases.push(extra);
            self.extras.push(ExtraRun {
                canvas: extra.canvas,
                size,
                view,
            });
        }
        self.collection = collection.clone();
        self.mix = mix_of(collection);
        self.master = master_of(collection, self.settings.canvas);
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
        self.failed_nodes.clear();
        let mut gone: Vec<Entry> = Vec::new();
        // The preview scene's compositor and picture when studio mode ends, taken back
        // after the apply.
        let mut leaving: Vec<Added> = Vec::new();
        if let Some(report) = &report {
            self.sync_scene_preview(report, &mut leaving)?;
        }
        let mut gone_filters: Vec<FilterEntry> = Vec::new();
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
            for extra in &mut self.extras {
                if report.retired_compositors.contains(&extra.view.compositor) {
                    self.failure = Some("a further canvas's compositor was retired".to_owned());
                    return Err(RecordError::NotBound("program compositor"));
                }
                if let Some(b) = report.compositors.get(&(extra.canvas, Role::Program)) {
                    extra.view.bound = b.clone();
                }
            }
            self.bound_mixer = report.mixers.get(&self.settings.canvas).cloned();
            let retired: BTreeSet<NodeId> = report.retired_sources.iter().copied().collect();
            let (old, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|e| retired.contains(&e.state.node));
            self.entries = kept;
            gone = old;
            let retired_filters: BTreeSet<NodeId> =
                report.retired_filters.iter().copied().collect();
            let (old_filters, kept_filters): (Vec<FilterEntry>, Vec<FilterEntry>) =
                std::mem::take(&mut self.filters)
                    .into_iter()
                    .partition(|f| retired_filters.contains(&f.node));
            self.filters = kept_filters;
            gone_filters = old_filters;
            for (filter, node) in &report.created_filters {
                let Some(source) = collection
                    .sources()
                    .find(|s| s.filters.iter().any(|f| f.id == *filter))
                else {
                    continue;
                };
                self.filters.push(FilterEntry {
                    filter: *filter,
                    source: source.id,
                    node: *node,
                    instance: None,
                });
            }
            for (filter, _) in &report.changed_filters {
                let values = filter_of(collection, *filter)
                    .map(|(kind, settings)| kind.values_lenient(settings).0);
                if let (Some(values), Some(entry)) = (
                    values,
                    self.filters.iter_mut().find(|f| f.filter == *filter),
                ) {
                    if let Some(instance) = &mut entry.instance {
                        instance.sender().send(values);
                    }
                }
            }
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
                        fault: None,
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
                    entry.state.fault = None;
                    i
                }
                Err(error) if strictness == Strictness::Strict => {
                    return Err(RecordError::Source {
                        name: entry.state.name.clone(),
                        error,
                    });
                }
                Err(error) => {
                    entry.state.fault = Some(error.fault());
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
                SourceInstance::Still(st) => {
                    // Needs no adapter or permission (ADR-0074).
                    self.engine
                        .adopt::<GpuRuntime>(RENDER, entry.state.node, Box::new(st.node))?;
                    entry.instance = Some(Instance {
                        partition: RENDER,
                        gpu: true,
                        watch: Watch::Video(st.stats),
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

        // Instances for the filters the plan schedules, warmed for their source's size.
        let mut adjusted = Vec::new();
        for entry in &mut self.filters {
            let Some((kind, settings)) = filter_of(collection, entry.filter) else {
                continue;
            };
            let (values, keys) = kind.values_lenient(settings);
            adjusted.extend(keys.into_iter().map(|key| FilterSettingsAdjusted {
                filter: entry.filter,
                key,
            }));
            if entry.instance.is_some() || !scheduled.contains(&entry.node) {
                continue;
            }
            match kind {
                FilterKind::Video(ty) => {
                    let size = self
                        .entries
                        .iter()
                        .find(|e| e.source == entry.source)
                        .and_then(|e| match &e.instance.as_ref()?.watch {
                            Watch::Video(stats) => stats.picture_size(),
                            _ => None,
                        })
                        .map(|(w, h)| [w, h]);
                    let shared: Arc<dyn FilterType> = Arc::new(ty);
                    let (node, sender, stats) = FilterNode::new(shared, values, size);
                    self.engine
                        .adopt::<GpuRuntime>(RENDER, entry.node, Box::new(node))?;
                    entry.instance = Some(FilterInstance::Video(sender, stats));
                }
                FilterKind::Audio(ty) => {
                    let graph = self.settings.audio_graph;
                    let setup = DspSetup::new(graph.format, graph.block_frames);
                    // A filter that cannot be made passes audio through, so its source
                    // stays audible; the filter is reported (ADR-0082 decision 8).
                    let (node, (sender, meter, stats)) =
                        match AudioFilterNode::new(ty, setup, values) {
                            Ok(made) => made,
                            Err(_) => {
                                adjusted.push(FilterSettingsAdjusted {
                                    filter: entry.filter,
                                    key: FILTER_UNAVAILABLE.to_owned(),
                                });
                                AudioFilterNode::pass_through(ty, setup, values)
                            }
                        };
                    self.engine.adopt::<()>(
                        self.settings.partitions.audio,
                        entry.node,
                        Box::new(node),
                    )?;
                    entry.instance = Some(FilterInstance::Audio {
                        sender,
                        meter,
                        stats,
                    });
                }
            }
        }
        self.filter_problems = adjusted;

        // Layers in link order of the final plan, strips per mixer input.
        let _ = self.layers.send(node_layers(&self.bound, &plan));
        if let Some(sp) = &mut self.scene_preview {
            let _ = sp.layers.send(node_layers(&sp.bound, &plan));
        }
        for extra in &mut self.extras {
            let _ = extra
                .view
                .layers
                .send(node_layers(&extra.view.bound, &plan));
        }
        // Strips: until the swap, the old schedule may still feed inputs that leave, so
        // the strips sent before the apply cover the old and the new inputs; the ones of
        // inputs that left go after it.
        let mut trimmed = None;
        if let (Some(mixer), Some(bound)) = (&mut self.mixer, &self.bound_mixer) {
            let live: BTreeSet<SourceId> = self.entries.iter().map(|e| e.source).collect();
            self.meters.retain(|s, _| live.contains(s));
            let new: Vec<MixStrip> = bound
                .inputs
                .iter()
                .map(|i| MixStrip {
                    producer: i.producer,
                    source: i.source,
                    meter: self.meters.get(&i.source).cloned(),
                    audio: self.mix.get(&i.source).copied().unwrap_or_default(),
                })
                .collect();
            let mut union = new.clone();
            union.extend(
                self.strips
                    .iter()
                    .filter(|old| !new.iter().any(|n| n.producer == old.producer))
                    .cloned(),
            );
            let _ = mixer
                .params
                .send(mixer_params(&union, &mixer.master, self.master));
            if union.len() != new.len() {
                trimmed = Some(mixer_params(&new, &mixer.master, self.master));
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
                let role = self
                    .essential
                    .get(node)
                    .copied()
                    .or_else(|| self.output_role(*node));
                if let Some(role) = role {
                    self.failed_nodes.insert(*node);
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
        for a in leaving {
            if !take(&mut self.engine, a) {
                self.pending_takes.push((a, None));
            }
        }
        for f in gone_filters {
            if let Some(instance) = &f.instance {
                let a = match instance {
                    FilterInstance::Video(..) => Added {
                        node: f.node,
                        partition: RENDER,
                        gpu: true,
                    },
                    FilterInstance::Audio { .. } => Added {
                        node: f.node,
                        partition: self.settings.partitions.audio,
                        gpu: false,
                    },
                };
                if !take(&mut self.engine, a) {
                    self.pending_takes.push((a, None));
                }
            }
        }
        Ok((failed, self.problems.clone()))
    }

    /// Starts or ends the preview scene's compositor and picture as the binder bound
    /// or retired the canvas's preview compositor (studio mode, ADR-0061). With a
    /// preview description, a new one is adopted on [`RENDER`] with a [`PreviewNode`] of
    /// its own; a retired one's nodes leave the graph and go to `leaving`, to be taken
    /// back after the apply. Neither is essential.
    fn sync_scene_preview(
        &mut self,
        report: &BindReport,
        leaving: &mut Vec<Added>,
    ) -> Result<(), RecordError> {
        let gpu = |node| Added {
            node,
            partition: RENDER,
            gpu: true,
        };
        if let Some(sp) = &self.scene_preview
            && report.retired_compositors.contains(&sp.compositor)
        {
            if let Some(sp) = self.scene_preview.take() {
                leaving.push(gpu(sp.compositor));
                let (picture, _) = sp.picture;
                if self.graph.node(picture).is_some() {
                    let _ = self.graph.set_state(picture, Lifecycle::Standby);
                    let _ = self.graph.remove_node(picture);
                }
                leaving.push(gpu(picture));
            }
        }
        // Only for a UI: without a picture the compositor has no consumer and the
        // plan leaves it and the preview-only sources out (ADR-0061 decision 2).
        let Some(desc) = self
            .settings
            .preview
            .filter(|_| self.settings.scene_preview)
        else {
            return Ok(());
        };
        let Some(bound) = report
            .compositors
            .get(&(self.settings.canvas, Role::Preview))
        else {
            return Ok(());
        };
        if let Some(sp) = &mut self.scene_preview {
            sp.bound = bound.clone();
            return Ok(());
        }
        let (width, height) = self.size;
        let (compositor, layers) =
            CompositorNode::new(CanvasDesc { width, height }, MAX_LAYERS as u32, Vec::new());
        self.engine
            .adopt::<GpuRuntime>(RENDER, bound.node, Box::new(compositor))?;
        let node = PreviewNode::new(desc);
        let stats = node.stats();
        let id = add_active(
            &mut self.graph,
            Node::<GpuRuntime>::descriptor(&node),
            RENDER,
        )?;
        self.graph.connect(bound.node, "canvas", id, "canvas")?;
        self.engine
            .adopt::<GpuRuntime>(RENDER, id, Box::new(node))?;
        self.scene_preview = Some(ScenePreview {
            compositor: bound.node,
            layers,
            bound: bound.clone(),
            picture: (id, stats),
        });
        Ok(())
    }

    /// Where a UI connects to the preview scene's picture (studio mode, ADR-0061), once
    /// it runs. `None` outside studio mode and without a preview description.
    pub fn scene_preview(&self) -> Option<PreviewEndpoint> {
        self.scene_preview
            .as_ref()
            .and_then(|sp| sp.picture.1.endpoint())
    }

    /// True while the preview scene's compositor and picture run (studio mode with a
    /// preview description, ADR-0061).
    pub fn studio_mode(&self) -> bool {
        self.scene_preview.is_some()
    }

    /// Volume and mute only (ADR-0059, ADR-0081): takes the sources' `audio` and the
    /// canvas's master from `collection` and sends the mixer the strips it already has
    /// at the new levels and the new master gain, without binding
    /// or applying the graph, so a fader drag stays cheap. The mixer ramps to them.
    /// Every other change goes through [`Studio::update`], which also sends the levels.
    /// Returns false if the studio has no mixer.
    pub fn set_mix(&mut self, collection: &SceneCollection) -> bool {
        self.mix = mix_of(collection);
        self.master = master_of(collection, self.settings.canvas);
        for strip in &mut self.strips {
            if let Some(audio) = self.mix.get(&strip.source) {
                strip.audio = *audio;
            }
        }
        let Some(mixer) = &mut self.mixer else {
            return false;
        };
        let _ = mixer
            .params
            .send(mixer_params(&self.strips, &mixer.master, self.master));
        true
    }

    /// The bound sources the running plan schedules, in binder order, each with its
    /// adapter, target and why it is not running (if it is not).
    pub fn sources(&self) -> Vec<SourceState> {
        self.entries
            .iter()
            .filter(|e| e.scheduled)
            .map(|e| {
                let mut state = e.state.clone();
                // The node's own counters say why a running instance failed.
                state.fault = match (&state.failed, e.instance.as_ref().map(|i| &i.watch)) {
                    (None, _) => None,
                    (Some(_), Some(Watch::Video(stats))) => stats.fault().or(state.fault),
                    (Some(_), _) => state.fault,
                };
                state
            })
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

    /// Where a UI connects to the picture of a further canvas (ADR-0093). `None` for the
    /// main canvas (see [`Studio::preview`]) and for a canvas the studio does not run.
    pub fn canvas_preview(&self, canvas: CanvasId) -> Option<PreviewEndpoint> {
        self.extras
            .iter()
            .find(|e| e.canvas == canvas)
            .and_then(|e| e.view.picture.as_ref()?.1.endpoint())
    }

    /// The counters of a further canvas's picture.
    pub fn canvas_preview_stats(&self, canvas: CanvasId) -> Option<&Arc<PreviewNodeStats>> {
        self.extras
            .iter()
            .find(|e| e.canvas == canvas)
            .and_then(|e| e.view.picture.as_ref().map(|p| &p.1))
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

    /// Sends `values` to a running filter without changing the collection (a slider
    /// drag; ADR-0062 decision 3). The next [`Studio::update`] sends the collection's
    /// values again. Returns true if a running instance got them.
    pub fn preview_filter(&mut self, filter: FilterId, values: Values) -> bool {
        match self
            .filters
            .iter_mut()
            .find(|f| f.filter == filter)
            .and_then(|f| f.instance.as_mut())
        {
            Some(instance) => {
                instance.sender().send(values);
                true
            }
            None => false,
        }
    }

    /// The user's "reload" of a source (ADR-0077): forgets its failure record so the
    /// engine prepares its node again at its next service, for a window that opened
    /// again or a device that came back. Returns whether the source had a failure
    /// record; a healthy source is left running.
    pub fn reload_source(&mut self, source: SourceId) -> bool {
        let Some(node) = self
            .entries
            .iter()
            .find(|e| e.source == source)
            .map(|e| e.state.node)
        else {
            return false;
        };
        self.engine.retry(node)
    }

    /// Sends the collection's values to a running filter again (a cancelled drag).
    pub fn restore_filter(&mut self, filter: FilterId) -> bool {
        let Some(values) =
            filter_of(&self.collection, filter).map(|(kind, s)| kind.values_lenient(s).0)
        else {
            return false;
        };
        self.preview_filter(filter, values)
    }

    /// Stored filter settings adjusted to run them, from the last start or update.
    pub fn filter_problems(&self) -> &[FilterSettingsAdjusted] {
        &self.filter_problems
    }

    /// Statistics of a running filter (its warm size and resizes).
    pub fn filter_stats(&self, filter: FilterId) -> Option<&Arc<FilterStats>> {
        let f = self.filters.iter().find(|f| f.filter == filter)?;
        match f.instance.as_ref()? {
            FilterInstance::Video(_, stats) => Some(stats),
            FilterInstance::Audio { .. } => None,
        }
    }

    /// The live meter of a running audio filter that has one (a noise gate's input
    /// level and gain reduction, ADR-0082); read it only while an editor shows it.
    pub fn audio_filter_meter(&self, filter: FilterId) -> Option<&Arc<FilterMeter>> {
        let f = self.filters.iter().find(|f| f.filter == filter)?;
        match f.instance.as_ref()? {
            FilterInstance::Audio { meter, .. } => meter.as_ref(),
            FilterInstance::Video(..) => None,
        }
    }

    /// Blocks an audio filter dropped because they were not in the graph's format.
    pub fn audio_filter_invalid_inputs(&self, filter: FilterId) -> Option<u64> {
        let f = self.filters.iter().find(|f| f.filter == filter)?;
        match f.instance.as_ref()? {
            FilterInstance::Audio { stats, .. } => Some(stats.invalid_inputs()),
            FilterInstance::Video(..) => None,
        }
    }

    /// Width and height in pixels of the picture a video source delivers now (its
    /// capture node's report, ADR-0058); `None` for an audio source, a source that is
    /// not running, or before its capture opened.
    pub fn picture_size(&self, source: SourceId) -> Option<(u32, u32)> {
        let entry = self.entries.iter().find(|e| e.source == source)?;
        match &entry.instance.as_ref()?.watch {
            Watch::Video(stats) => stats.picture_size(),
            _ => None,
        }
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
            .or_else(|| self.output_role(node))
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.state.node == node)
                    .map(|e| e.state.name.as_str())
            })
    }

    /// Runs the engine's control loop once and notes failures: a source that fails is
    /// marked in [`Studio::sources`] until it runs again; a studio node that fails for
    /// good sets [`Studio::failure`], a node of an output or of an encoder it uses
    /// [`Studio::output_failure`] of every output using it.
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
                    } else if let Some(role) = self.output_role(*node) {
                        // An encoder chain's node fails every output that uses it;
                        // an output node only its own (ADR-0047 decision 7).
                        if retry_at.is_none() {
                            let users = self.users(*node);
                            for r in self.outputs.iter_mut().filter(|r| users.contains(&r.id)) {
                                if r.failure.is_none() {
                                    r.failure = Some(format!("{role}: {cause:?}"));
                                }
                            }
                        }
                    } else if let Some(c) = self.camera.as_mut().filter(|c| c.node.node == *node) {
                        // Not essential: the studio and its outputs run on (ADR-0066).
                        c.failure = Some(format!("{cause:?}"));
                    } else if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node)
                    {
                        e.state.failed = Some(format!("{cause:?}"));
                    }
                }
                RuntimeEvent::Restarted { node, .. } | RuntimeEvent::Recovered { node } => {
                    if let Some(c) = self.camera.as_mut().filter(|c| c.node.node == *node) {
                        c.failure = None;
                    }
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

    /// The running outputs, in start order.
    pub fn outputs(&self) -> Vec<OutputId> {
        self.outputs.iter().map(|r| r.id).collect()
    }

    fn attached(&self, id: OutputId) -> Option<&Attached> {
        self.outputs.iter().find(|r| r.id == id)
    }

    /// What a running output chose and records or streams.
    pub fn output(&self, id: OutputId) -> Option<&StartReport> {
        self.attached(id).map(|r| &r.report)
    }

    /// Why a running output cannot go on: its output's first permanent failure, or one
    /// of its nodes (its own or an encoder it shares) failed for good. The composition
    /// root then stops it with [`Studio::stop_output`].
    pub fn output_failure(&self, id: OutputId) -> Option<String> {
        let r = self.attached(id)?;
        r.output_stats
            .failure
            .get()
            .cloned()
            .or_else(|| r.failure.clone())
    }

    /// The role of a node of an output or of an encoder chain.
    fn output_role(&self, node: NodeId) -> Option<&'static str> {
        if self.outputs.iter().any(|r| r.output.node == node) {
            return Some("output");
        }
        for c in self.chains.video.iter().flatten() {
            if c.export.node == node {
                return Some("video export");
            }
            if c.encoder.node == node {
                return Some("video encoder");
            }
        }
        self.chains
            .audio
            .iter()
            .flatten()
            .any(|c| c.encoder.node == node)
            .then_some("audio encoder")
    }

    /// The outputs that use `node`: the output itself, or every output taking packets
    /// from the encoder chain it belongs to.
    fn users(&self, node: NodeId) -> Vec<OutputId> {
        let video = self.chains.video.iter().position(|c| {
            c.as_ref()
                .is_some_and(|c| c.export.node == node || c.encoder.node == node)
        });
        let audio = self
            .chains
            .audio
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.encoder.node == node));
        self.outputs
            .iter()
            .filter(|r| {
                r.output.node == node
                    || video == Some(r.video)
                    || (audio.is_some() && audio == r.audio)
            })
            .map(|r| r.id)
            .collect()
    }

    /// Starts an output (a recording or a live stream) of the canvas to `out` while the
    /// studio runs. Adapters are chosen first (`NoAdapter` before anything opens), and a
    /// running encoder is shared when `out`'s settings equal its own (ADR-0047); then a
    /// video export and video encoder, an audio encoder (with `out.audio`), each unless
    /// shared, and an output are added and applied. Sources only now scheduled (without
    /// a preview) are opened. If one of the new nodes does not start, they are all
    /// removed again and the call fails with `Start`; the studio and the other output
    /// keep running. The output may have created the destination.
    /// `TooManyOutputs` while [`MAX_OUTPUTS`] run.
    pub fn start_output(
        &mut self,
        adapters: &RecordAdapters,
        out: &OutputSettings,
    ) -> Result<OutputId, RecordError> {
        if self.outputs.len() >= MAX_OUTPUTS {
            return Err(RecordError::TooManyOutputs);
        }
        if out.audio.is_some() && self.mixer.is_none() {
            return Err(RecordError::NotBound("mixer"));
        }
        let canvas = out.canvas.unwrap_or(self.settings.canvas);
        let size = self.canvas_size(canvas)?;
        let prepared = Prepared::choose(
            adapters,
            out,
            canvas,
            size,
            self.fps,
            self.settings.audio_graph,
            &self.chains,
        )?;
        self.attach(prepared, &adapters.sources, Strictness::Lenient)
    }

    /// The compositor node of a canvas the studio runs.
    fn compositor_of(&self, canvas: CanvasId) -> Result<NodeId, RecordError> {
        if canvas == self.settings.canvas {
            return Ok(self.compositor);
        }
        self.extras
            .iter()
            .find(|e| e.canvas == canvas)
            .map(|e| e.view.compositor)
            .ok_or(RecordError::UnknownCanvas(canvas))
    }

    /// The size of a canvas the studio runs.
    fn canvas_size(&self, canvas: CanvasId) -> Result<(u32, u32), RecordError> {
        if canvas == self.settings.canvas {
            return Ok(self.size);
        }
        self.extras
            .iter()
            .find(|e| e.canvas == canvas)
            .map(|e| e.size)
            .ok_or(RecordError::UnknownCanvas(canvas))
    }

    /// Encoder counters of the chains in `video` and `audio` slots now.
    fn base_of(&self, video: usize, audio: Option<usize>) -> Base {
        let mut b = Base::default();
        if let Some(c) = &self.chains.video[video] {
            b.frames_exported = c.export_stats.frames.load(Ordering::Relaxed);
            b.export_drops = sum(&[&c.export_stats.busy, &c.export_stats.lost]);
            let e = &c.encoder_stats;
            b.frames_encoded = e.submitted.load(Ordering::Relaxed);
            b.encoder_drops = sum(&[&e.busy_drops, &e.refused, &e.overflow_drops]);
            b.encoder_skipped = c.queue.as_ref().map_or(0, |q| q.video_drops());
        }
        if let Some(c) = audio.and_then(|i| self.chains.audio[i].as_ref()) {
            b.audio_queue_drops = c.queue.as_ref().map_or(0, |q| q.overflows());
            b.audio_packet_drops = c.dropped.load(Ordering::Relaxed);
            b.audio_flushes = c.flushes.load(Ordering::Relaxed);
            b.audio_tail_lost = c.tail_lost.load(Ordering::Relaxed);
        }
        b
    }

    /// Adds a prepared output's nodes to the graph and adopts their instances; applies
    /// nothing. All or nothing: on an error the graph, the chains and the partitions are
    /// as before. Returns the output and the nodes it added.
    fn add_output(&mut self, p: Prepared) -> Result<(OutputId, Vec<Added>), RecordError> {
        let parts = self.settings.partitions;
        let canvas = p.canvas;
        let compositor = self.compositor_of(canvas)?;
        // Partitions are taken by role, each the first free one whose earlier nodes
        // were all taken back (a take still pending means its swap or drain is in
        // flight, ADR-0047). With MAX_OUTPUTS outputs and at most one chain of each kind
        // per output, a free one of each role exists while fewer outputs run, once the
        // pending takes are done; until then the start fails with `OutputFinishing`.
        let pending: BTreeSet<PartitionId> = self
            .pending_takes
            .iter()
            .map(|(a, _)| a.partition)
            .collect();
        let slot = (0..MAX_OUTPUTS)
            .find(|s| {
                !self.outputs.iter().any(|r| r.slot == *s)
                    && !pending.contains(&parts.slot(*s).output)
            })
            .ok_or(RecordError::OutputFinishing)?;
        let mut graph = self.graph.clone();
        let mut nodes: Vec<Added> = Vec::new();

        let output_stats = p.output.stats();
        let output_part = parts.slot(slot).output;
        let output_id = add_active(&mut graph, Node::<()>::descriptor(&p.output), output_part)?;
        let output = Added {
            node: output_id,
            partition: output_part,
            gpu: false,
        };

        // Video: a new chain in a free slot, or the shared one's encoder.
        let mut new_video = None;
        let (video_slot, video_encoder_node) = match &p.video {
            VideoPlan::Shared(i) => {
                let c = self.chains.video[*i]
                    .as_ref()
                    .ok_or(RecordError::NotBound("shared video encoder"))?;
                (*i, c.encoder.node)
            }
            VideoPlan::New {
                export, encoder, ..
            } => {
                let i = (0..MAX_OUTPUTS)
                    .find(|i| {
                        self.chains.video[*i].is_none()
                            && !pending.contains(&parts.slot(*i).video_encoder)
                    })
                    .ok_or(RecordError::OutputFinishing)?;
                let part = parts.slot(i).video_encoder;
                let export_id = add_active(
                    &mut graph,
                    Node::<GpuRuntime>::descriptor(&**export),
                    RENDER,
                )?;
                let encoder_id = add_active(&mut graph, Node::<()>::descriptor(&**encoder), part)?;
                graph.connect(compositor, "canvas", export_id, "canvas")?;
                graph.connect(export_id, "video", encoder_id, "video")?;
                let e = Added {
                    node: export_id,
                    partition: RENDER,
                    gpu: true,
                };
                let v = Added {
                    node: encoder_id,
                    partition: part,
                    gpu: false,
                };
                nodes.push(e);
                nodes.push(v);
                new_video = Some((e, v));
                (i, encoder_id)
            }
        };
        graph.connect(video_encoder_node, "packets", output_id, "s0")?;

        let mut new_audio = None;
        let audio_slot = match &p.audio {
            AudioPlan::None => None,
            AudioPlan::Shared(i) => {
                let c = self.chains.audio[*i]
                    .as_ref()
                    .ok_or(RecordError::NotBound("shared audio encoder"))?;
                graph.connect(c.encoder.node, "packets", output_id, "s1")?;
                Some(*i)
            }
            AudioPlan::New { node, .. } => {
                let mixer = self
                    .mixer
                    .as_ref()
                    .map(|m| m.node)
                    .ok_or(RecordError::NotBound("mixer"))?;
                let i = (0..MAX_OUTPUTS)
                    .find(|i| {
                        self.chains.audio[*i].is_none()
                            && !pending.contains(&parts.slot(*i).audio_encoder)
                    })
                    .ok_or(RecordError::OutputFinishing)?;
                let part = parts.slot(i).audio_encoder;
                let id = add_active(&mut graph, Node::<()>::descriptor(&**node), part)?;
                graph.connect(mixer, "main", id, "in")?;
                graph.connect(id, "packets", output_id, "s1")?;
                let a = Added {
                    node: id,
                    partition: part,
                    gpu: false,
                };
                nodes.push(a);
                new_audio = Some(a);
                Some(i)
            }
        };
        nodes.push(output);

        // Instances last; one that is refused takes the adopted ones back.
        let mut adopted: Vec<Added> = Vec::new();
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
        let mut video_chain = None;
        let mut video_shared = true;
        if let (
            VideoPlan::New {
                export,
                encoder,
                chosen,
                settings,
            },
            Some((e, v)),
        ) = (p.video, new_video)
        {
            video_shared = false;
            let export_stats = export.stats();
            let end = export.end();
            let encoder_stats = encoder.stats();
            let stream = encoder.stream().clone();
            let r = self.engine.adopt::<GpuRuntime>(RENDER, e.node, export);
            adopt(&mut self.engine, e, r)?;
            let r = self.engine.adopt::<()>(v.partition, v.node, encoder);
            adopt(&mut self.engine, v, r)?;
            video_chain = Some(VideoChain {
                canvas,
                settings,
                chosen,
                stream,
                export: e,
                encoder: v,
                export_stats,
                end,
                encoder_stats,
                queue: None,
            });
        }
        let mut audio_chain = None;
        let mut audio_shared = audio_slot.is_some();
        if let (
            AudioPlan::New {
                node,
                chosen,
                settings,
                stream,
            },
            Some(a),
        ) = (p.audio, new_audio)
        {
            audio_shared = false;
            let dropped = node.dropped();
            let (flushes, tail_lost) = (node.flushes(), node.tail_lost());
            let r = self.engine.adopt::<()>(a.partition, a.node, node);
            adopt(&mut self.engine, a, r)?;
            audio_chain = Some(AudioChain {
                settings,
                chosen,
                stream,
                encoder: a,
                dropped,
                flushes,
                tail_lost,
                queue: None,
            });
        }
        let r = self
            .engine
            .adopt::<()>(output.partition, output.node, Box::new(p.output));
        adopt(&mut self.engine, output, r)?;

        // Committed: the graph, the chains and the output.
        self.graph = graph;
        let base = self.base_of(video_slot, audio_slot);
        if let Some(c) = video_chain {
            self.chains.video[video_slot] = Some(c);
        }
        if let (Some(c), Some(i)) = (audio_chain, audio_slot) {
            self.chains.audio[i] = Some(c);
        }
        let (video_chosen, export, encoder, video_queue) = match &self.chains.video[video_slot] {
            Some(c) => (
                c.chosen.clone(),
                Arc::clone(&c.export_stats),
                Arc::clone(&c.encoder_stats),
                c.queue.clone(),
            ),
            None => return Err(RecordError::NotBound("video encoder")),
        };
        let audio = audio_slot.and_then(|i| self.chains.audio[i].as_ref());
        let audio_chosen = audio.map(|c| c.chosen.clone());
        let audio_dropped = audio.map(|c| Arc::clone(&c.dropped));
        let audio_tail = audio.map(|c| (Arc::clone(&c.flushes), Arc::clone(&c.tail_lost)));
        let audio_queue = audio.and_then(|c| c.queue.clone());
        let id = OutputId(self.next_output);
        self.next_output += 1;
        self.outputs.push(Attached {
            id,
            slot,
            report: StartReport {
                video_encoder: video_chosen,
                audio_encoder: audio_chosen,
                muxer: p.muxer,
                transport: p.transport,
                sources: Vec::new(),
                problems: self.problems.clone(),
                video_shared,
                audio_shared,
            },
            output,
            output_stats,
            video: video_slot,
            audio: audio_slot,
            failure: None,
            export,
            encoder,
            audio_dropped,
            audio_tail,
            ended: None,
            queues: Queues {
                video: video_queue,
                audio: audio_queue,
            },
            base,
        });
        Ok((id, nodes))
    }

    /// Adds a prepared output to the running graph and applies it. If one of its new
    /// nodes, the engine or a link fails, or (`Strict`) a source cannot be instantiated,
    /// the output is removed again and the call fails.
    pub(crate) fn attach(
        &mut self,
        p: Prepared,
        sources: &SourceTypes,
        strictness: Strictness,
    ) -> Result<OutputId, RecordError> {
        let (id, added) = self.add_output(p)?;
        // Sources scheduled only now (no preview) are opened; then one apply.
        let failed = match self.reconcile(sources, None, strictness) {
            Ok((failed, _)) => failed,
            Err(e) => {
                let _ = self.detach(id, false);
                return Err(e);
            }
        };
        let new_failed = added.iter().any(|a| self.failed_nodes.contains(&a.node));
        let engine_failed = failed
            .iter()
            .any(|f| f.node == "engine" || f.node.starts_with("link"));
        if new_failed || engine_failed {
            let mut fatal: Vec<StartFailure> =
                failed.into_iter().filter(|f| f.node != "source").collect();
            if let Some((_, errors, _)) = self.detach(id, false) {
                for e in errors {
                    fatal.push(StartFailure {
                        node: "rollback".to_owned(),
                        reason: e,
                    });
                }
            }
            return Err(RecordError::Start(fatal));
        }
        self.finish_attach(id);
        Ok(id)
    }

    /// After the apply that scheduled an output: its sources in the report, and the
    /// queues into new encoder partitions (they exist from this apply on).
    fn finish_attach(&mut self, id: OutputId) {
        let sources = self.sources();
        let queue_to = |engine: &Engine, node: NodeId| {
            engine
                .crossings()
                .iter()
                .find(|c| c.link.to.node == node)
                .map(|c| Arc::clone(&c.stats))
        };
        for c in self.chains.video.iter_mut().flatten() {
            if c.queue.is_none() {
                c.queue = queue_to(&self.engine, c.encoder.node);
            }
        }
        for c in self.chains.audio.iter_mut().flatten() {
            if c.queue.is_none() {
                c.queue = queue_to(&self.engine, c.encoder.node);
            }
        }
        let video = self
            .outputs
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.video, r.audio));
        let Some((v, a)) = video else {
            return;
        };
        let video_queue = self.chains.video[v].as_ref().and_then(|c| c.queue.clone());
        let audio_queue = a
            .and_then(|i| self.chains.audio[i].as_ref())
            .and_then(|c| c.queue.clone());
        if let Some(r) = self.outputs.iter_mut().find(|r| r.id == id) {
            r.report.sources = sources;
            r.queues = Queues {
                video: video_queue,
                audio: audio_queue,
            };
        }
    }

    /// Finishes the start of [`Recording`](crate::Recording)'s output.
    pub(crate) fn finish_first_attach(&mut self) -> Option<OutputId> {
        let id = self.outputs.first()?.id;
        self.finish_attach(id);
        Some(id)
    }

    /// Removes outputs' nodes, and the encoder chains no remaining output uses, from the
    /// graph in one apply, draining the partitions this empties when `drain`; takes their
    /// instances back. Returns each output with whether its video encoder was removed
    /// (drained, when `drain`), and the engine's errors.
    fn detach_all(
        &mut self,
        ids: &[OutputId],
        drain: bool,
    ) -> (Vec<(Attached, bool)>, Vec<String>) {
        let (gone, kept): (Vec<Attached>, Vec<Attached>) = std::mem::take(&mut self.outputs)
            .into_iter()
            .partition(|r| ids.contains(&r.id));
        self.outputs = kept;
        if gone.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let mut nodes: Vec<Added> = gone.iter().map(|r| r.output).collect();
        let mut removed = Vec::new();
        for r in gone {
            let video_alone = !self.outputs.iter().any(|o| o.video == r.video);
            if video_alone && let Some(c) = self.chains.video[r.video].take() {
                nodes.push(c.export);
                nodes.push(c.encoder);
            }
            if let Some(a) = r.audio
                && !self.outputs.iter().any(|o| o.audio == Some(a))
                && let Some(c) = self.chains.audio[a].take()
            {
                nodes.push(c.encoder);
            }
            removed.push((r, video_alone));
        }
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
        // The plan without the outputs may leave sources unscheduled (no preview);
        // the layer hints follow the new plan.
        let plan = self.graph.plan();
        let scheduled: BTreeSet<NodeId> = plan.order.iter().copied().collect();
        for e in &mut self.entries {
            e.scheduled = scheduled.contains(&e.state.node);
        }
        let _ = self.layers.send(node_layers(&self.bound, &plan));
        if let Some(sp) = &mut self.scene_preview {
            let _ = sp.layers.send(node_layers(&sp.bound, &plan));
        }
        for extra in &mut self.extras {
            let _ = extra
                .view
                .layers
                .send(node_layers(&extra.view.bound, &plan));
        }
        (removed, errors)
    }

    /// [`Studio::detach_all`] for one output.
    fn detach(&mut self, id: OutputId, drain: bool) -> Option<(Attached, Vec<String>, bool)> {
        let (mut removed, errors) = self.detach_all(&[id], drain);
        let (r, video_alone) = removed.pop()?;
        Some((r, errors, video_alone))
    }

    /// The report of an output removed by [`Studio::detach_all`].
    fn finish_report(
        &self,
        r: &Attached,
        errors: Vec<String>,
        drained_video: bool,
    ) -> FinishReport {
        let progress = self.progress_of(r);
        let failure = r
            .output_stats
            .failure
            .get()
            .cloned()
            .or_else(|| r.failure.clone());
        // A shared encoder that keeps running is not flushed: its held packets are the
        // other output's.
        let flush_lost = if drained_video {
            r.encoder.flush_lost.load(Ordering::Relaxed)
        } else {
            0
        };
        let finish_lost = r.output_stats.finish_lost.load(Ordering::Relaxed);
        FinishReport {
            progress,
            failure,
            shutdown_errors: errors,
            flush_lost,
            finish_lost,
        }
    }

    /// Ends a running output's video now, on the master clock (ADR-0045): call
    /// [`Studio::stop_output`] once [`Studio::output_tail_done`] is true, so the frames
    /// up to now reach it; until then it keeps running. When its video encoder is its
    /// alone, the export takes no later canvas and passes on what it still reads back;
    /// a shared export keeps running for the other output, and the tail is in once the
    /// encoder has taken a frame from now on. False when `id` does not run. A second
    /// call keeps the first end.
    pub fn end_output(&mut self, id: OutputId) -> bool {
        let now = self.engine.clock().now();
        let alone = self.attached(id).map(|r| {
            r.ended.map_or_else(
                || self.outputs.iter().filter(|o| o.video == r.video).count() == 1,
                |(_, alone)| alone,
            )
        });
        let Some(alone) = alone else {
            return false;
        };
        let Some(r) = self.outputs.iter_mut().find(|r| r.id == id) else {
            return false;
        };
        if r.ended.is_some() {
            return true;
        }
        if alone && let Some(c) = &self.chains.video[r.video] {
            c.end.end_at(now);
        }
        r.ended = Some((now, alone));
        true
    }

    /// True when [`Studio::stop_output`] loses no video frame up to the end: after
    /// [`Studio::end_output`], once the frames up to the end have reached the video
    /// encoder (an export of its own: it passed on every canvas it took, and the
    /// encoder took the last of them), or once [`END_TAIL_LIMIT`] has passed on the
    /// master clock. True when `id` does not run; false while it runs and was not ended.
    pub fn output_tail_done(&self, id: OutputId) -> bool {
        let Some(r) = self.attached(id) else {
            return true;
        };
        let Some((at, alone)) = r.ended else {
            return false;
        };
        let taken = r.encoder.last_frame.load(Ordering::Relaxed);
        let delivered = if alone {
            // End to end: the export passed on its last frame, and the encoder took it
            // (the queue between them keeps only the newest frame).
            self.chains.video[r.video]
                .as_ref()
                .is_none_or(|c| c.end.ended())
                && taken >= r.export.last_frame.load(Ordering::Relaxed)
        } else {
            // A shared export keeps going: the encoder took a frame at or after the end.
            taken > at.as_nanos()
        };
        delivered || self.engine.clock().now() >= at + END_TAIL_LIMIT
    }

    /// Stops a running output: drains, in pipeline order, its output (which writes the
    /// container's trailer and finishes the transport) and the encoders no other output
    /// uses, and removes them, while the studio and the other output keep running.
    /// `None` when `id` does not run.
    pub fn stop_output(&mut self, id: OutputId) -> Option<FinishReport> {
        let (r, errors, drained_video) = self.detach(id, true)?;
        Some(self.finish_report(&r, errors, drained_video))
    }

    /// A running output's connection; `None` when `id` does not run.
    /// Cheap (a few atomic loads) and, unlike [`Studio::output_progress`], takes no
    /// meter peaks.
    pub fn link(&self, id: OutputId) -> Option<Link> {
        let o = &self.attached(id)?.output_stats;
        Some(Link {
            connected: o.connected.load(Ordering::Relaxed),
            connects: o.connects.load(Ordering::Relaxed),
            reconnect_attempts: o.reconnect_attempts.load(Ordering::Relaxed),
            transport_drops: o.transport_drops.load(Ordering::Relaxed),
        })
    }

    /// Counters now of the sources and workers. Peak levels are those since the
    /// previous call (of this or [`Studio::output_progress`]).
    pub fn progress(&self) -> Progress {
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
        Progress {
            sources,
            late_render_ticks: self.render_worker.as_ref().map_or(0, |w| w.late()),
            late_audio_ticks: self.audio_worker.as_ref().map_or(0, |w| w.late()),
            master_peak_db: self.mixer.as_ref().map(|m| peak_db(&m.master)),
            ..Progress::default()
        }
    }

    /// [`Studio::progress`] plus a running output's counters, counted from its start
    /// (an output that joined a running encoder does not count what it encoded
    /// before). `None` when `id` does not run.
    pub fn output_progress(&self, id: OutputId) -> Option<Progress> {
        Some(self.progress_of(self.attached(id)?))
    }

    /// A running output's counters, as in [`Studio::output_progress`], without the
    /// sources and without taking any meter's peak: for a statistics view read once a
    /// second next to level meters that must keep their peaks. The late tick counters
    /// of the workers are included. `None` when `id` does not run.
    pub fn output_counters(&self, id: OutputId) -> Option<Progress> {
        let r = self.attached(id)?;
        let workers = Progress {
            late_render_ticks: self.render_worker.as_ref().map_or(0, |w| w.late()),
            late_audio_ticks: self.audio_worker.as_ref().map_or(0, |w| w.late()),
            ..Progress::default()
        };
        Some(Self::counters_of(r, workers))
    }

    fn progress_of(&self, r: &Attached) -> Progress {
        Self::counters_of(r, self.progress())
    }

    /// `p` with `r`'s output counters filled in.
    fn counters_of(r: &Attached, mut p: Progress) -> Progress {
        let b = &r.base;
        let e = &r.encoder;
        let since = |now: u64, base: u64| now.saturating_sub(base);
        p.frames_exported = since(r.export.frames.load(Ordering::Relaxed), b.frames_exported);
        p.export_drops = since(sum(&[&r.export.busy, &r.export.lost]), b.export_drops);
        p.frames_encoded = since(e.submitted.load(Ordering::Relaxed), b.frames_encoded);
        p.encoder_drops = since(
            sum(&[&e.busy_drops, &e.refused, &e.overflow_drops]),
            b.encoder_drops,
        );
        p.encoder_skipped = since(
            r.queues.video.as_ref().map_or(0, |q| q.video_drops()),
            b.encoder_skipped,
        );
        p.audio_queue_drops = since(
            r.queues.audio.as_ref().map_or(0, |q| q.overflows()),
            b.audio_queue_drops,
        );
        p.audio_packet_drops = since(
            r.audio_dropped
                .as_ref()
                .map_or(0, |d| d.load(Ordering::Relaxed)),
            b.audio_packet_drops,
        );
        if let Some((flushes, lost)) = &r.audio_tail {
            p.audio_flushes = since(flushes.load(Ordering::Relaxed), b.audio_flushes);
            p.audio_tail_lost = since(lost.load(Ordering::Relaxed), b.audio_tail_lost);
        }
        let o = &r.output_stats;
        p.packets_taken = o.packets.load(Ordering::Relaxed);
        p.mux_drops = o.mux_drops.load(Ordering::Relaxed);
        p.start_drops = o.start_drops.load(Ordering::Relaxed);
        p.backlog_peak = o.backlog_peak.load(Ordering::Relaxed);
        p.transport_drops = o.transport_drops.load(Ordering::Relaxed);
        p.connects = o.connects.load(Ordering::Relaxed);
        p.reconnect_attempts = o.reconnect_attempts.load(Ordering::Relaxed);
        p.connected = o.connected.load(Ordering::Relaxed);
        p
    }

    /// Starts sending the program to a virtual camera (ADR-0066 decision 6): opens a
    /// session of `factory` for `settings` on this (the control) thread, then adds a
    /// [`VirtualCameraNode`] on [`RENDER`] after the program compositor and applies. It
    /// is not one of the [`MAX_OUTPUTS`] outputs and touches none of them. Fails with
    /// `InvalidConfig` while the camera already sends; an engine or graph refusal is
    /// `Os { code: 0, .. }`, and nothing of the camera stays.
    pub fn start_virtual_camera(
        &mut self,
        factory: &dyn VirtualCameraFactory,
        settings: VirtualCameraSettings,
    ) -> Result<(), VcamError> {
        if self.camera.is_some() {
            return Err(VcamError::InvalidConfig(
                "the virtual camera already sends".into(),
            ));
        }
        let config = VirtualCameraConfig::new(
            CameraFormat::new(
                settings.width,
                settings.height,
                settings.rate,
                PixelFormat::Nv12,
            ),
            MemoryKind::System,
        );
        let session = factory.open(&config)?;
        let counters = session.counters();
        let (width, height) = self.size;
        let node = VirtualCameraNode::new(session, config, width, height);
        let stats = node.stats();
        let engine_error = |e: String| VcamError::Os {
            code: 0,
            message: e,
        };
        let mut graph = self.graph.clone();
        let id = add_active(&mut graph, Node::<GpuRuntime>::descriptor(&node), RENDER)
            .map_err(|e| engine_error(e.to_string()))?;
        graph
            .connect(self.compositor, "canvas", id, "canvas")
            .map_err(|e| engine_error(e.to_string()))?;
        // A refused adoption drops the node, and with it the session, here.
        self.engine
            .adopt::<GpuRuntime>(RENDER, id, Box::new(node))
            .map_err(|e| engine_error(e.to_string()))?;
        let added = Added {
            node: id,
            partition: RENDER,
            gpu: true,
        };
        if let Err(e) = self.engine.apply(&graph) {
            if !take(&mut self.engine, added) {
                self.pending_takes.push((added, None));
            }
            return Err(engine_error(e.to_string()));
        }
        self.graph = graph;
        self.camera = Some(CameraRun {
            node: added,
            config,
            stats,
            session: counters,
            failure: None,
        });
        Ok(())
    }

    /// Stops sending to the virtual camera: removes its node with a plain swap. The node
    /// comes back through the pending takes and is dropped here, on the control thread,
    /// with its session (whose drop joins the adapter's sender thread), never on
    /// [`RENDER`]. The camera itself stays installed. False when it was not sending.
    pub fn stop_virtual_camera(&mut self) -> bool {
        let Some(run) = self.camera.take() else {
            return false;
        };
        let _ = self.graph.set_state(run.node.node, Lifecycle::Standby);
        let _ = self.graph.remove_node(run.node.node);
        // An apply error leaves the node to the pending takes like any other.
        let _ = self.engine.apply(&self.graph);
        if !take(&mut self.engine, run.node) {
            self.pending_takes.push((run.node, None));
        }
        true
    }

    /// The virtual camera while it sends: its configuration, counters and failure.
    pub fn virtual_camera(&self) -> Option<VirtualCameraState> {
        self.camera.as_ref().map(|c| VirtualCameraState {
            config: c.config,
            stats: Arc::clone(&c.stats),
            session: c.session.clone(),
            failure: c.failure.clone(),
        })
    }

    /// Stops every output in one draining apply (a shared encoder is flushed for all
    /// of them), then the engine. Returns each output's report, in start order, and the
    /// shutdown's errors.
    pub fn shutdown(&mut self) -> (Vec<(OutputId, FinishReport)>, Vec<String>) {
        self.stop_virtual_camera();
        let ids = self.outputs();
        let (removed, errors) = self.detach_all(&ids, true);
        let reports = removed
            .iter()
            .map(|(r, drained)| (r.id, self.finish_report(r, errors.clone(), *drained)))
            .collect();
        let errors = self
            .engine
            .shutdown()
            .errors
            .iter()
            .map(ToString::to_string)
            .collect();
        (reports, errors)
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
/// The built-in type and stored settings of a filter of the collection; `None` when the
/// filter is gone or its type is not built in.
fn filter_of(
    collection: &SceneCollection,
    filter: FilterId,
) -> Option<(FilterKind, &rbs_scene::model::Settings)> {
    let f = collection
        .sources()
        .flat_map(|s| s.filters.iter())
        .find(|f| f.id == filter)?;
    let kind = match rbs_filters::find(f.type_id.as_str()) {
        Some(ty) => FilterKind::Video(ty),
        None => FilterKind::Audio(rbs_audio_filters::find(f.type_id.as_str())?),
    };
    Some((kind, &f.settings))
}

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

/// Volume and mute of every source of `collection`.
fn mix_of(collection: &SceneCollection) -> BTreeMap<SourceId, SourceAudio> {
    collection.sources().map(|s| (s.id, s.audio)).collect()
}

/// The preview scene's compositor and its picture for a UI (studio mode, ADR-0061).
struct ScenePreview {
    compositor: NodeId,
    layers: LayerSender,
    bound: BoundCompositor,
    /// The preview node and its counters.
    picture: (NodeId, Arc<PreviewNodeStats>),
}

/// `collection` with every following canvas's program scene set to what it shows now
/// (ADR-0093 decision 3); `last` remembers it for the `KeepLast` fallback. The binder
/// sees only program scenes, so it needs to know nothing of following.
fn with_followers(
    collection: &SceneCollection,
    last: &mut BTreeMap<CanvasId, SceneId>,
) -> SceneCollection {
    let shown = collection.followed_programs(last);
    if shown.is_empty() {
        return collection.clone();
    }
    let mut resolved = collection.clone();
    for (canvas, scene) in shown {
        // An override the collection refuses leaves the canvas on its own program.
        if resolved.set_program(canvas, Some(scene)).is_ok() {
            last.insert(canvas, scene);
        }
    }
    resolved
}

/// A further canvas that runs (ADR-0093).
struct ExtraRun {
    canvas: CanvasId,
    size: (u32, u32),
    view: ExtraView,
}

/// A further canvas's compositor and, for a UI, its picture.
struct ExtraView {
    compositor: NodeId,
    layers: LayerSender,
    bound: BoundCompositor,
    picture: Option<(NodeId, Arc<PreviewNodeStats>)>,
}

/// Adopts `bound`'s compositor on [`RENDER`] at `size`, with a picture of its own when
/// `desc` is given.
fn attach_compositor(
    engine: &mut Engine,
    graph: &mut GraphDescription,
    bound: BoundCompositor,
    size: (u32, u32),
    desc: Option<PreviewDesc>,
) -> Result<ExtraView, RecordError> {
    let (compositor, layers) = CompositorNode::new(
        CanvasDesc {
            width: size.0,
            height: size.1,
        },
        MAX_LAYERS as u32,
        Vec::new(),
    );
    engine.adopt::<GpuRuntime>(RENDER, bound.node, Box::new(compositor))?;
    let mut picture = None;
    if let Some(desc) = desc {
        let node = PreviewNode::new(desc);
        let stats = node.stats();
        let id = add_active(graph, Node::<GpuRuntime>::descriptor(&node), RENDER)?;
        graph.connect(bound.node, "canvas", id, "canvas")?;
        engine.adopt::<GpuRuntime>(RENDER, id, Box::new(node))?;
        picture = Some((id, stats));
    }
    Ok(ExtraView {
        compositor: bound.node,
        layers,
        bound,
        picture,
    })
}

/// One mixer input as the studio last sent it: its producer, the source it plays, its
/// meter and the source's volume and mute (ADR-0059).
#[derive(Clone)]
struct MixStrip {
    producer: NodeId,
    source: SourceId,
    meter: Option<Arc<LevelMeter>>,
    audio: SourceAudio,
}

/// The canvas's master volume and mute (the defaults for a canvas that is gone).
fn master_of(collection: &SceneCollection, canvas: CanvasId) -> MasterAudio {
    collection
        .canvas(canvas)
        .map(|c| c.master)
        .unwrap_or_default()
}

/// The mixer's parameters for `strips` (each at its source's volume and mute), the
/// master gain (its volume, or silence while muted) and the master meter.
fn mixer_params(strips: &[MixStrip], meter: &Arc<LevelMeter>, master: MasterAudio) -> MixerParams {
    let strips = strips
        .iter()
        .map(|s| {
            let strip = Strip::new(s.producer, s.audio.volume).with_muted(s.audio.muted);
            match &s.meter {
                Some(m) => strip.with_meter(Arc::clone(m)),
                None => strip,
            }
        })
        .collect();
    MixerParams::new(strips)
        .with_master(master.gain())
        .with_master_meter(Arc::clone(meter))
}
