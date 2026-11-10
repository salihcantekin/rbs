//! The recording: graph assembly, start, progress and finish.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, LevelMeter};
use rbs_codec::{
    AudioEncoderFactory, EncoderPreset, KeyframeInterval, RateControl, VideoEncoderFactory,
};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{CodecId, ContainerId};
use rbs_core::registry::{AdapterId, Registry};
use rbs_engine::{Engine, EngineError, Serviced};
use rbs_gpu::{GpuContext, YuvFormat};
use rbs_graph::description::{GraphDescription, GraphError, NodeId, PartitionId};
use rbs_graph::node::NodeDescriptor;
use rbs_mux::MuxerFactory;
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindError, BindProblem};
use rbs_scene::model::{CanvasId, SourceId};
use rbs_sources::types::{SourceClass, class_of};
use rbs_sources::{SourceError, SourceTypes};
use rbs_transport::{ReconnectPolicy, Secret, TransportFactory, TransportInput};

use crate::select::Chosen;
use crate::studio::{
    Chains, OutputId, OutputSettings, Prepared, Strictness, Studio, StudioSettings,
    VirtualCameraSettings, VirtualCameraState,
};

/// The render partition: capture sources, the compositor and the video export. The
/// scene binder places video sources here, so it is not configurable.
pub const RENDER: PartitionId = PartitionId::RENDER;
/// Default audio partition: audio inputs and the mixer, at block cadence.
pub const AUDIO: PartitionId = PartitionId(1);
/// Default video encoder partition, a quarter frame after the render tick.
pub const VIDEO_ENCODER: PartitionId = PartitionId(2);
/// Default audio encoder partition, one tick per queued block.
pub const AUDIO_ENCODER: PartitionId = PartitionId(3);
/// Default output partition (muxer and transport).
pub const OUTPUT: PartitionId = PartitionId(4);
/// Default video encoder partition of a studio's second output slot (ADR-0047).
pub const VIDEO_ENCODER_2: PartitionId = PartitionId(5);
/// Default audio encoder partition of a studio's second output slot.
pub const AUDIO_ENCODER_2: PartitionId = PartitionId(6);
/// Default output partition of a studio's second output slot.
pub const OUTPUT_2: PartitionId = PartitionId(7);

/// The ports a recording chooses adapters for.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// Video encoder.
    VideoEncoder,
    /// Audio encoder.
    AudioEncoder,
    /// Muxer.
    Muxer,
    /// Transport.
    Transport,
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Port::VideoEncoder => "video encoder",
            Port::AudioEncoder => "audio encoder",
            Port::Muxer => "muxer",
            Port::Transport => "transport",
        })
    }
}

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

/// Video encoding of the recording: every encoder setting that changes the
/// recording's quality ([`rbs_codec::VideoEncoderConfig`]).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSettings {
    /// Codec.
    pub codec: CodecId,
    /// Rate control: CBR (padded with filler data or not), VBR or constant quality,
    /// with its bitrates or level.
    pub rate_control: RateControl,
    /// Keyframe interval.
    pub keyframe_interval: KeyframeInterval,
    /// Most B-frames between references; encoders that declare a lower
    /// `codec.video.b_frames.max` use fewer.
    pub max_b_frames: u32,
    /// Codec profile name (`high`, `main`, `main10`); `None` lets the encoder pick.
    pub profile: Option<String>,
    /// Speed versus quality.
    pub preset: EncoderPreset,
    /// The export's pixel format (NV12, or P010 for 10-bit).
    pub format: YuvFormat,
}

impl Default for VideoSettings {
    /// H.264, CBR 12 Mbit/s (not padded), a keyframe every 2 s, no B-frames, the
    /// encoder's default profile, the balanced preset, NV12.
    fn default() -> Self {
        Self {
            codec: CodecId::H264,
            rate_control: RateControl::Cbr {
                bitrate_bps: 12_000_000,
            },
            keyframe_interval: KeyframeInterval::Duration(Duration::from_secs(2)),
            max_b_frames: 0,
            profile: None,
            preset: EncoderPreset::Balanced,
            format: YuvFormat::Nv12,
        }
    }
}

/// Audio encoding of the recording.
#[non_exhaustive]
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

/// The partitions a recording adds to the engine (render is always
/// [`PartitionId::RENDER`]). Each must be new to the engine.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partitions {
    /// Audio inputs and the mixer.
    pub audio: PartitionId,
    /// The video encoder.
    pub video_encoder: PartitionId,
    /// The audio encoder.
    pub audio_encoder: PartitionId,
    /// The output.
    pub output: PartitionId,
    /// The second output slot of a studio (ADR-0047): its encoders, when they are not
    /// shared with the first output, and its output. The fields above are the first.
    pub second: OutputPartitions,
}

impl Partitions {
    /// The partitions of output slot `slot` (0 or 1; 1 for anything above).
    pub(crate) fn slot(&self, slot: usize) -> OutputPartitions {
        if slot == 0 {
            OutputPartitions {
                video_encoder: self.video_encoder,
                audio_encoder: self.audio_encoder,
                output: self.output,
            }
        } else {
            self.second
        }
    }
}

impl Default for Partitions {
    fn default() -> Self {
        Self {
            audio: AUDIO,
            video_encoder: VIDEO_ENCODER,
            audio_encoder: AUDIO_ENCODER,
            output: OUTPUT,
            second: OutputPartitions::default(),
        }
    }
}

/// The partitions of one output slot: a video encoder, an audio encoder and an output.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputPartitions {
    /// The video encoder (cadence, a quarter frame after render).
    pub video_encoder: PartitionId,
    /// The audio encoder (doorbell).
    pub audio_encoder: PartitionId,
    /// The output (muxer and transport).
    pub output: PartitionId,
}

impl Default for OutputPartitions {
    /// The second slot's defaults: [`VIDEO_ENCODER_2`], [`AUDIO_ENCODER_2`], [`OUTPUT_2`].
    fn default() -> Self {
        Self {
            video_encoder: VIDEO_ENCODER_2,
            audio_encoder: AUDIO_ENCODER_2,
            output: OUTPUT_2,
        }
    }
}

/// What to record or stream and where.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSettings {
    /// The canvas recorded (its program scene).
    pub canvas: CanvasId,
    /// Destination URL, for example `file:///C:/Videos/rec.mkv` or `rtmp://host/app`.
    /// Never holds the secret.
    pub url: String,
    /// What the transport takes: a container's chunks, or packets
    /// ([`OutputSettings::input`]).
    pub input: TransportInput,
    /// Stream key or stream id ([`OutputSettings::secret`]).
    pub secret: Option<Secret>,
    /// What the transport does when the connection drops.
    pub reconnect: ReconnectPolicy,
    /// Opaque transport options ([`OutputSettings::transport_options`]).
    pub transport_options: Vec<(String, String)>,
    /// Video encoding.
    pub video: VideoSettings,
    /// Audio encoding; `None` records video only.
    pub audio: Option<AudioSettings>,
    /// The audio graph's format, block size and latency.
    pub audio_graph: AudioGraphConfig,
    /// Partition ids.
    pub partitions: Partitions,
    /// Sync offset in ns of each audio source: positive delays it, negative advances
    /// it ([`StudioSettings::audio_offsets`]).
    pub audio_offsets: BTreeMap<SourceId, i64>,
}

impl RecordSettings {
    /// `canvas` to `url` in `container` with the default video and audio settings, no
    /// secret, no reconnect.
    pub fn new(canvas: CanvasId, url: impl Into<String>, container: ContainerId) -> Self {
        Self::from_output(canvas, OutputSettings::new(url, container))
    }

    /// `canvas` as encoded packets to `url` (RTMP), with the default video and audio
    /// settings, no secret, no reconnect.
    pub fn packets(canvas: CanvasId, url: impl Into<String>) -> Self {
        Self::from_output(canvas, OutputSettings::packets(url))
    }

    /// `canvas` written as `out` says.
    pub fn from_output(canvas: CanvasId, out: OutputSettings) -> Self {
        Self {
            canvas,
            url: out.url,
            input: out.input,
            secret: out.secret,
            reconnect: out.reconnect,
            transport_options: out.transport_options,
            video: out.video,
            audio: out.audio,
            audio_graph: AudioGraphConfig::default(),
            partitions: Partitions::default(),
            audio_offsets: BTreeMap::new(),
        }
    }

    /// The studio part: canvas, audio (when audio is recorded), graph, partitions and
    /// audio offsets, no preview.
    pub fn studio(&self) -> StudioSettings {
        let mut s = StudioSettings::new(self.canvas);
        s.audio = self.audio.is_some();
        s.audio_graph = self.audio_graph;
        s.partitions = self.partitions;
        s.audio_offsets = self.audio_offsets.clone();
        s
    }

    /// The output part: URL, transport input, secret, reconnect, options, video and
    /// audio encoding.
    pub fn output(&self) -> OutputSettings {
        OutputSettings {
            url: self.url.clone(),
            input: self.input,
            secret: self.secret.clone(),
            reconnect: self.reconnect,
            transport_options: self.transport_options.clone(),
            video: self.video.clone(),
            audio: self.audio.clone(),
        }
    }
}

/// A node of the recording that did not start, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartFailure {
    /// The node's role (`"video encoder"`), or `"engine"` / a link.
    pub node: String,
    /// Why.
    pub reason: String,
}

/// Why a recording could not start.
#[non_exhaustive]
#[derive(Debug)]
pub enum RecordError {
    /// The collection has no canvas with this id.
    UnknownCanvas(CanvasId),
    /// The canvas has no program scene.
    NoProgram(CanvasId),
    /// The URL has no scheme.
    InvalidUrl(String),
    /// The binder refused the collection.
    Bind(BindError),
    /// The binder could not represent sources the recorded scene shows.
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
        port: Port,
        /// Adapters tried or rejected, with the reason.
        tried: Vec<(AdapterId, String)>,
    },
    /// The graph refused an edit (a bug in this crate).
    Graph(GraphError),
    /// The engine refused an edit (for example a partition id already in use).
    Engine(EngineError),
    /// Nodes the recording needs did not start; the engine was shut down. Shutdown
    /// errors are listed with node `"shutdown"`. ([`Studio::start_output`]: the
    /// output's nodes were removed and the studio runs on; errors of that removal are
    /// listed with node `"rollback"`.)
    Start(Vec<StartFailure>),
    /// The studio runs [`MAX_OUTPUTS`](crate::MAX_OUTPUTS) outputs already
    /// ([`Studio::start_output`]).
    TooManyOutputs,
    /// A partition the new output needs still finishes a stopped output (its swap or
    /// drain is in flight); [`Studio::service`] completes it, then a start works.
    OutputFinishing,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::UnknownCanvas(id) => write!(f, "no canvas {id:?}"),
            RecordError::NoProgram(id) => write!(f, "canvas {id:?} has no program scene"),
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
            RecordError::OutputFinishing => {
                f.write_str("an output stopped just now is still finishing; try again")
            }
            RecordError::TooManyOutputs => write!(
                f,
                "{} outputs run already, the most a studio runs at once",
                crate::MAX_OUTPUTS
            ),
            RecordError::Start(nodes) => {
                write!(f, "the recording did not start")?;
                for n in nodes {
                    write!(f, "; {}: {}", n.node, n.reason)?;
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

/// A recorded source as started.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceState {
    /// Its node in the graph.
    pub node: NodeId,
    /// The source's name.
    pub name: String,
    /// The adapter capturing it.
    pub adapter: AdapterId,
    /// The capture target or audio endpoint id.
    pub target: String,
    /// Why it is not running, while it is not (did not start, or failed and waits for
    /// a restart); cleared when it runs again.
    pub failed: Option<String>,
    /// The cause of that failure, when the adapter tells it (ADR-0077); `None` while the
    /// source runs or when the cause is unknown.
    pub fault: Option<rbs_core::fault::SourceFault>,
}

/// What [`Recording::start`] chose and started.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartReport {
    /// The video encoder.
    pub video_encoder: Chosen,
    /// The audio encoder, when audio is recorded.
    pub audio_encoder: Option<Chosen>,
    /// The muxer; `None` when the transport takes packets.
    pub muxer: Option<Chosen>,
    /// The transport.
    pub transport: Chosen,
    /// The recorded sources (those the output's plan schedules), in binder order.
    pub sources: Vec<SourceState>,
    /// Binder problems that do not stop the recording: elsewhere in the collection,
    /// or not fatal (a skipped filter, the video of a source with audio).
    pub problems: Vec<BindProblem>,
    /// The video encoder (and its export) was already running for another output and is
    /// shared (ADR-0047); the output started at its next keyframe.
    pub video_shared: bool,
    /// The audio encoder was already running for another output and is shared.
    pub audio_shared: bool,
}

/// Counters of one source.
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// Exported frames that never reached the video encoder: a newer frame replaced
    /// them in the queue into the encoder's partition before the encoder took them
    /// (the encoder fell behind or had not started yet), or they were still queued at
    /// stop.
    pub encoder_skipped: u64,
    /// Video frames dropped or refused at the encoder, and packets lost in its carry-over.
    pub encoder_drops: u64,
    /// Audio packets the audio encoder node lost (its carry-over was full).
    pub audio_packet_drops: u64,
    /// End-of-stream flushes of the audio encoder: 1 after a drained stop (ADR-0045).
    pub audio_flushes: u64,
    /// Audio packets or blocks lost at the end of the stream (ADR-0045): packets the
    /// output's queue refused on the last final tick, blocks after the flush.
    pub audio_tail_lost: u64,
    /// Audio blocks lost at the queue between the mixer and the audio encoder.
    pub audio_queue_drops: u64,
    /// Packets the output took, all streams, including those the muxer dropped.
    pub packets_taken: u64,
    /// Packets the muxer dropped as out of order.
    pub mux_drops: u64,
    /// Packets of other streams the output dropped because they end before the first
    /// video packet (aligned start).
    pub start_drops: u64,
    /// Largest output backlog (transport queue full).
    pub backlog_peak: u64,
    /// Items the transport dropped by its drop policy (a network transport whose
    /// queue filled while the connection was slow or down).
    pub transport_drops: u64,
    /// Times the transport reached its destination (a file opened, a stream
    /// published), reconnections included; 0 means nothing was ever delivered.
    pub connects: u64,
    /// Connection attempts after a failure or a lost connection.
    pub reconnect_attempts: u64,
    /// The transport is connected now.
    pub connected: bool,
    /// Render ticks skipped by a late render worker.
    pub late_render_ticks: u64,
    /// Audio ticks skipped by a late audio worker.
    pub late_audio_ticks: u64,
    /// Master peak in dBFS since the last [`Recording::progress`], when audio is recorded.
    pub master_peak_db: Option<f32>,
}

impl Progress {
    /// Packets muxed into the destination: taken minus dropped.
    pub fn packets_muxed(&self) -> u64 {
        self.packets_taken.saturating_sub(self.mux_drops)
    }
}

/// The result of [`Recording::finish`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct FinishReport {
    /// The counters at the end.
    pub progress: Progress,
    /// Why the recording failed while it ran, if it did.
    pub failure: Option<String>,
    /// Errors while the engine drained.
    pub shutdown_errors: Vec<String>,
    /// Packets the video encoder still held when it left without a final tick (a drain
    /// that timed out), lost. Frames still read back at the export when the recording
    /// stopped without [`Recording::end`] (or past [`crate::END_TAIL_LIMIT`]) are lost
    /// too and counted in [`Progress::export_drops`].
    pub flush_lost: u64,
    /// Output items not delivered when the output finished.
    pub finish_lost: u64,
}

/// Adds `desc` to `graph`, Active, in `partition`.
pub(crate) fn add_active(
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

pub(crate) fn sum(counters: &[&AtomicU64]) -> u64 {
    counters.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

pub(crate) fn peak_db(meter: &LevelMeter) -> f32 {
    let levels = meter.take();
    (0..levels.channels())
        .map(|ch| levels.peak_db(ch))
        .fold(f32::NEG_INFINITY, f32::max)
}

/// The source a problem names, if any (a filter problem names the filter's source).
fn problem_source(collection: &SceneCollection, p: &BindProblem) -> Option<SourceId> {
    let filter_owner = |id| {
        collection
            .sources()
            .find(|s| s.filters.iter().any(|f| f.id == id))
            .map(|s| s.id)
    };
    match p {
        BindProblem::UnknownSourceType(s, _)
        | BindProblem::NoVideoOutput(s)
        | BindProblem::AudioOnVideoSource(s) => Some(*s),
        BindProblem::UnknownFilterType(f, _)
        | BindProblem::NoVideoPins(f)
        | BindProblem::NoAudioPins(f)
        | BindProblem::KeyUnavailable(f) => filter_owner(*f),
        _ => None,
    }
}

/// Whether `p` stops a recording that shows `recorded`: a recorded source left out of
/// the graph. Without audio, audio-only sources are meant to be left out.
pub(crate) fn is_fatal(
    collection: &SceneCollection,
    recorded: &BTreeSet<SourceId>,
    audio: bool,
    p: &BindProblem,
) -> bool {
    let Some(source) = problem_source(collection, p) else {
        return false;
    };
    if !recorded.contains(&source) {
        return false;
    }
    match p {
        BindProblem::UnknownSourceType(..) => true,
        BindProblem::NoVideoOutput(_) => {
            let audio_only = collection
                .source(source)
                .and_then(|s| class_of(&s.type_id))
                .is_some_and(|c| matches!(c, SourceClass::Audio(_)));
            audio || !audio_only
        }
        _ => false,
    }
}

/// A running recording: a [`Studio`] without a preview and one recording started on
/// it. It owns the engine; [`Recording::finish`] stops it.
pub struct Recording {
    studio: Studio,
    output: OutputId,
    started: StartReport,
}

impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recording")
            .field("started", &self.started)
            .field("failure", &self.failure())
            .finish_non_exhaustive()
    }
}

impl Recording {
    /// Binds `collection`, attaches the encoders and the output to the canvas of
    /// `settings`, and starts `engine` on `gpu`. The recording adds
    /// [`PartitionId::RENDER`] and the partitions of `settings.partitions` (the audio
    /// ones only when audio is recorded); none of them may exist yet. Control thread;
    /// may block while devices open.
    ///
    /// Only what the canvas's program scene shows and plays is instantiated and run.
    /// A source that does not start is reported in [`StartReport::sources`] and the
    /// recording goes on without it; a compositor, mixer, export, encoder or output that
    /// does not start fails the call after the engine was shut down. The output may
    /// have opened (and, for a file, created or truncated) the destination by then.
    pub fn start(
        engine: Engine,
        gpu: Arc<GpuContext>,
        collection: &SceneCollection,
        adapters: &RecordAdapters,
        settings: &RecordSettings,
    ) -> Result<Self, RecordError> {
        let canvas = collection
            .canvas(settings.canvas)
            .ok_or(RecordError::UnknownCanvas(settings.canvas))?;
        if canvas.program.is_none() {
            return Err(RecordError::NoProgram(settings.canvas));
        }
        // Adapters first: nothing opens before every port has one.
        let prepared = Prepared::choose(
            adapters,
            &settings.output(),
            (canvas.size.width, canvas.size.height),
            canvas.frame_rate,
            settings.audio_graph,
            &Chains::default(),
        )?;
        let mut studio = Studio::start_with(
            engine,
            gpu,
            collection,
            &adapters.sources,
            &settings.studio(),
            Strictness::Strict,
            Some(prepared),
        )?;
        let started = studio
            .finish_first_attach()
            .and_then(|id| Some((id, studio.output(id)?.clone())));
        let Some((output, started)) = started else {
            let _ = studio.shutdown();
            return Err(RecordError::NotBound("recording"));
        };
        Ok(Self {
            studio,
            output,
            started,
        })
    }

    /// What was chosen and started, with each source's current state.
    pub fn started(&self) -> &StartReport {
        &self.started
    }

    /// The engine (its clock is the master clock of every timestamp in the file).
    pub fn engine(&self) -> &Engine {
        self.studio.engine()
    }

    /// The studio the recording runs on.
    pub fn studio(&self) -> &Studio {
        &self.studio
    }

    /// Starts sending the program to a virtual camera next to the recording
    /// ([`Studio::start_virtual_camera`]; `rbs-cli record --vcam`).
    ///
    /// # Errors
    ///
    /// As [`Studio::start_virtual_camera`]: the adapter's refusal (`NotInstalled`, ...),
    /// `InvalidConfig` while it already sends, an engine refusal as `Os`.
    pub fn start_virtual_camera(
        &mut self,
        factory: &dyn rbs_vcam::VirtualCameraFactory,
        settings: VirtualCameraSettings,
    ) -> Result<(), rbs_vcam::VcamError> {
        self.studio.start_virtual_camera(factory, settings)
    }

    /// Stops sending to the virtual camera ([`Studio::stop_virtual_camera`]); false when
    /// it was not sending. [`Recording::finish`] stops it too.
    pub fn stop_virtual_camera(&mut self) -> bool {
        self.studio.stop_virtual_camera()
    }

    /// The virtual camera while it sends ([`Studio::virtual_camera`]).
    pub fn virtual_camera(&self) -> Option<VirtualCameraState> {
        self.studio.virtual_camera()
    }

    /// The name of a node of the recording (a source name or a role such as
    /// `"video encoder"`), for messages about engine events.
    pub fn node_name(&self, node: NodeId) -> Option<&str> {
        self.studio.node_name(node)
    }

    /// Runs the engine's control loop once (restarts, swaps) and notes failures: a
    /// source that fails is marked in [`StartReport::sources`] until it runs again; an
    /// essential node that fails for good, or an output failure, ends the recording
    /// ([`Recording::failure`]). Call it again after `wait` (at most about 100 ms).
    pub fn service(&mut self) -> Serviced {
        let serviced = self.studio.service();
        let now = self.studio.sources();
        for s in &mut self.started.sources {
            if let Some(n) = now.iter().find(|n| n.node == s.node) {
                s.failed.clone_from(&n.failed);
            }
        }
        serviced
    }

    /// Why the recording cannot go on, if it cannot: the output's first permanent
    /// failure, or an essential node that failed for good.
    pub fn failure(&self) -> Option<String> {
        self.studio
            .output_failure(self.output)
            .or_else(|| self.studio.failure())
    }

    /// The counters now. Peak levels are those since the previous call.
    pub fn progress(&self) -> Progress {
        self.studio
            .output_progress(self.output)
            .unwrap_or_else(|| self.studio.progress())
    }

    /// Mixer inputs that had no strip or an invalid block (both should stay 0).
    pub fn mixer_anomalies(&self) -> u64 {
        self.studio.mixer_anomalies()
    }

    /// Ends the video at the master clock's now and keeps the frames still read back
    /// (ADR-0045, [`Studio::end_output`]). Keep calling [`Recording::service`] until
    /// [`Recording::tail_done`], then [`Recording::finish`].
    pub fn end(&mut self) {
        self.studio.end_output(self.output);
    }

    /// True once [`Recording::finish`] loses no video frame up to the end
    /// ([`Studio::output_tail_done`]): false before [`Recording::end`].
    pub fn tail_done(&self) -> bool {
        self.studio.output_tail_done(self.output)
    }

    /// Stops the recording: drains its encoders and output (the output writes the
    /// container's trailer), then shuts the engine down, and reports. Without
    /// [`Recording::end`] first, frames still read back for the video are lost.
    pub fn finish(mut self) -> FinishReport {
        let studio_failure = self.studio.failure();
        let (reports, errors) = self.studio.shutdown();
        let report = reports.into_iter().next().map(|(_, r)| r);
        let mut report = report.unwrap_or_else(|| FinishReport {
            progress: Progress::default(),
            failure: None,
            shutdown_errors: Vec::new(),
            flush_lost: 0,
            finish_lost: 0,
        });
        report.shutdown_errors.extend(errors);
        report.failure = report.failure.or(studio_failure);
        report
    }
}
