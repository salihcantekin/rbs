//! The studio's control thread (ADR-0041 decision 7): owns the `Studio`, the scene
//! collection and the adapter registries. The UI only sends [`Command`]s and gets
//! [`Update`]s through a callback; it never calls the studio. The preview reaches the UI
//! through the studio's `PreviewEndpoint`, as it would from another process.
//!
//! Toolkit-free, so it is tested without a window (`tests/studio_host.rs`).
//!
//! The same thread serves the control API (ADR-0044) when one is attached
//! ([`StudioHost::attach_api`]): forwarded requests are applied one at a time, after any
//! UI command waiting, and state changes are published as events with a `seq` per
//! state key. With no API attached this costs a counter increment per change.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_api::request::{Empty, ProfileList, RecordStarted, SourceAdded, TargetList};
use rbs_api::types as api;
use rbs_api::{
    ApiError, ControlEndpoint, ControlTarget, ErrorKind, EventPayload, EventPublisher, Response,
    StateKey, Topic,
};
use rbs_audio::LevelMeter;
use rbs_codec::caps as codec_caps;
use rbs_core::capability::{CapabilitySet, CapabilityValue, Requirement};
use rbs_core::media::{ContainerId, Size};
use rbs_core::registry::{AdapterFactory, Registry};
use rbs_core::timing::FrameRate;
use rbs_engine::Engine;
use rbs_gpu::GpuContext;
use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AudioSettings, OutputSettings, RecordAdapters, Studio, StudioSettings, VideoSettings,
};
use rbs_scene::SceneCollection;
use rbs_scene::Undo;
use rbs_scene::model::{
    CanvasId, ItemContent, Layout, SceneId, SceneScope, SettingValue, Settings, SourceId,
};
use rbs_sources::types::{self, keys};

use crate::canvas_profile::CanvasProfile;
use crate::record_profile::{API_PROFILE_ID, RecordProfile};

/// The longest the control thread sleeps between two rounds of the engine's control
/// loop (the studio asks for at most about 100 ms).
const MAX_WAIT: Duration = Duration::from_millis(50);
/// The camera picture asked for (most webcams and capture cards offer it).
const CAMERA_SIZE: (i64, i64, i64) = (1280, 720, 30);

/// A kind of source the user can add. The UI names them with glossary terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A whole display.
    Screen,
    /// One window.
    Window,
    /// A camera or capture card.
    Camera,
    /// A microphone or line input.
    AudioInput,
    /// What the computer plays.
    SystemAudio,
    /// What one application plays.
    ApplicationAudio,
}

impl SourceKind {
    /// Every kind, in the order the add-source picker lists them.
    pub const ALL: [SourceKind; 6] = [
        SourceKind::Screen,
        SourceKind::Window,
        SourceKind::Camera,
        SourceKind::AudioInput,
        SourceKind::SystemAudio,
        SourceKind::ApplicationAudio,
    ];

    /// The source type of this kind.
    pub fn type_id(self) -> NodeTypeId {
        NodeTypeId::new_static(match self {
            SourceKind::Screen => types::DISPLAY_SOURCE,
            SourceKind::Window => types::WINDOW_SOURCE,
            SourceKind::Camera => types::CAMERA_SOURCE,
            SourceKind::AudioInput => types::AUDIO_INPUT_SOURCE,
            SourceKind::SystemAudio => types::AUDIO_OUTPUT_SOURCE,
            SourceKind::ApplicationAudio => types::APPLICATION_AUDIO_SOURCE,
        })
    }

    /// The kind of a source type.
    pub fn of(type_id: &NodeTypeId) -> Option<SourceKind> {
        SourceKind::ALL
            .into_iter()
            .find(|k| k.type_id() == *type_id)
    }

    /// The source type's name in the UI (glossary: "Capture" only in type names).
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Screen => "Screen capture",
            SourceKind::Window => "Window capture",
            SourceKind::Camera => "Camera",
            SourceKind::AudioInput => "Audio input",
            SourceKind::SystemAudio => "System audio",
            SourceKind::ApplicationAudio => "Application audio",
        }
    }

    /// True for kinds that only make sound.
    pub fn is_audio(self) -> bool {
        matches!(
            self,
            SourceKind::AudioInput | SourceKind::SystemAudio | SourceKind::ApplicationAudio
        )
    }
}

/// Something a source of a kind can capture: a display, window, camera or endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Name from the OS.
    pub name: String,
    /// The adapter that listed it.
    pub adapter: String,
    /// Its id for that adapter.
    pub target: String,
    /// The OS default of its kind (audio endpoints).
    pub is_default: bool,
    /// Native size in pixels when known (displays and windows).
    pub size: Option<(u32, u32)>,
}

/// How a source is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// It runs.
    Running,
    /// It is not shown or played, so it is not opened (P13): for example a video source
    /// where no preview runs.
    Idle,
    /// It is not running, and why.
    Failed(String),
}

/// One source of the program scene, as the UI shows it.
#[derive(Debug, Clone)]
pub struct SourceRow {
    /// Its id in the collection.
    pub id: SourceId,
    /// Its name, unique in the collection.
    pub name: String,
    /// Its kind; `None` for a type this build does not know.
    pub kind: Option<SourceKind>,
    /// How it is doing.
    pub status: SourceStatus,
    /// Its level meter while it plays (audio sources).
    pub meter: Option<Arc<LevelMeter>>,
}

impl PartialEq for SourceRow {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.kind == other.kind
            && self.status == other.status
            && match (&self.meter, &other.meter) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// What the UI asks the studio to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// List what a kind can capture ([`Update::Targets`]).
    ListTargets(SourceKind),
    /// Add a source of `kind` capturing `target` to the program scene, on top.
    AddSource {
        /// Its kind.
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// Delete a source from every scene (glossary: "Delete source").
    DeleteSource(SourceId),
    /// Undo the last delete.
    UndoDelete,
    /// Start recording the program ([`Update::Recording`] says how it went).
    StartRecording(RecordRequest),
    /// Stop the running recording; the file is complete when
    /// [`RecordingUpdate::Stopped`] arrives.
    StopRecording,
    /// Change the canvas's size and rate (Settings, Video). The studio restarts with
    /// it (ADR-0041: a running studio's canvas does not change); the program scene's
    /// items are placed again for the new size. Refused while recording. Always
    /// answered with [`Update::Canvas`]; the UI drops its preview connection first,
    /// since the old studio's preview ends.
    SetCanvas {
        /// The new size.
        size: Size,
        /// The new rate.
        fps: FrameRate,
    },
    /// The recording settings as saved: what `record.start` uses for control API
    /// clients and `settings.get` shows. Sent at start and after each change.
    SetProfile(RecordProfile),
}

/// A video encoder the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Its adapter id.
    pub id: String,
    /// Its name in the UI.
    pub name: String,
    /// What its quality settings can be, from its capabilities.
    pub options: EncoderOptions,
}

/// The quality settings a video encoder takes, from its probed capabilities
/// (`docs/specs/output-ports.md`), so the settings offer only what it can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncoderOptions {
    /// `codec.video.rate_control` values: `cbr`, `cbr_padded`, `vbr`,
    /// `constant_quality`.
    pub rate_controls: Vec<String>,
    /// H.264 profile names (`high`, `main`, `baseline`) from `codec.video.profiles`.
    pub h264_profiles: Vec<String>,
    /// `codec.video.b_frames.max`.
    pub b_frames_max: u32,
    /// `cbr_padded` is listed: CBR can be padded with filler data.
    pub cbr_padding: bool,
}

impl EncoderOptions {
    /// Reads them from a capability set.
    pub fn from_caps(caps: &CapabilitySet) -> Self {
        let set = |key| match caps.get(key) {
            Some(CapabilityValue::Set(v)) => v.to_vec(),
            _ => Vec::new(),
        };
        Self {
            rate_controls: set(&codec_caps::VIDEO_RATE_CONTROL),
            h264_profiles: set(&codec_caps::VIDEO_PROFILES)
                .iter()
                .filter_map(|p| p.strip_prefix("h264.").map(str::to_owned))
                .collect(),
            b_frames_max: match caps.get(&codec_caps::VIDEO_B_FRAMES_MAX) {
                Some(CapabilityValue::Int(n)) => u32::try_from(*n).unwrap_or(0),
                _ => 0,
            },
            cbr_padding: set(&codec_caps::VIDEO_RATE_CONTROL)
                .iter()
                .any(|r| r == "cbr_padded"),
        }
    }

    /// What any of `all` can do: the choice when the encoder is picked automatically.
    pub fn union<'a>(all: impl IntoIterator<Item = &'a EncoderOptions>) -> Self {
        let mut u = EncoderOptions::default();
        for o in all {
            for r in &o.rate_controls {
                if !u.rate_controls.contains(r) {
                    u.rate_controls.push(r.clone());
                }
            }
            for p in &o.h264_profiles {
                if !u.h264_profiles.contains(p) {
                    u.h264_profiles.push(p.clone());
                }
            }
            u.b_frames_max = u.b_frames_max.max(o.b_frames_max);
            u.cbr_padding |= o.cbr_padding;
        }
        u
    }
}

/// Where and how to record.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordRequest {
    /// The file, absolute. Its folder is created; an existing file is not replaced.
    pub path: PathBuf,
    /// The container.
    pub container: ContainerId,
    /// The video encoder's adapter id: only this one is used. `None`: the best one.
    pub encoder: Option<String>,
    /// Video encoding: rate control, keyframes, profile, preset, B-frames, padding.
    pub video: VideoSettings,
    /// Audio bitrate in bit/s.
    pub audio_bitrate_bps: u64,
}

/// What happened to the recording.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingUpdate {
    /// It runs.
    Started {
        /// The file.
        path: PathBuf,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is recorded.
        audio_encoder: Option<String>,
    },
    /// It could not start, in words; nothing records.
    NotStarted(String),
    /// It stopped, asked to or because it failed; the file is closed.
    Stopped {
        /// The file.
        path: PathBuf,
        /// Why it stopped on its own, if it did.
        failure: Option<String>,
        /// Video frames encoded.
        frames: u64,
        /// Video frames dropped at the export or the encoder.
        dropped: u64,
        /// Packets the video encoder still held after its final flush (normally 0).
        flush_lost: u64,
    },
}

/// What the studio tells the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The studio runs. `preview`: where the UI connects to the canvas preview (`None`
    /// where the platform has none). `kinds`: the kinds the adapters can add.
    Started {
        /// The preview endpoint.
        preview: Option<PreviewEndpoint>,
        /// Kinds the user can add.
        kinds: Vec<SourceKind>,
        /// Adapters that could not be registered or the GPU's name, for the log.
        notes: Vec<String>,
        /// Video encoders the user can pick, best first.
        encoders: Vec<EncoderInfo>,
        /// Why this system cannot record, or `None` when it can.
        cannot_record: Option<String>,
        /// The canvas's size.
        size: Size,
        /// The canvas's rate.
        fps: FrameRate,
        /// The main display's size in pixels, when known (for "Main display" in
        /// Settings).
        display: Option<(u32, u32)>,
    },
    /// The canvas the studio runs now, after [`Command::SetCanvas`]: changed, or
    /// unchanged when the change was refused (an [`Update::Error`] says why). Connect
    /// to `preview` again.
    Canvas {
        /// The preview endpoint.
        preview: Option<PreviewEndpoint>,
        /// The canvas's size.
        size: Size,
        /// The canvas's rate.
        fps: FrameRate,
    },
    /// The studio cannot run (no GPU, or a studio node failed for good).
    Failed(String),
    /// The program scene's sources changed or one changed state.
    Sources(Vec<SourceRow>),
    /// What a kind can capture, in OS order.
    Targets(SourceKind, Vec<Target>),
    /// A source was deleted; [`Command::UndoDelete`] brings it back.
    Deleted(String),
    /// A command did not work, in words for a toast.
    Error(String),
    /// The recording started, did not start or stopped.
    Recording(RecordingUpdate),
    /// A control API client changed the recording settings; the UI shows and saves
    /// them.
    Profile(RecordProfile),
}

/// What the control thread receives.
enum Msg {
    Ui(Command),
    AttachApi(Box<ControlEndpoint>),
    DetachApi,
    /// A control API request is waiting.
    ApiWake,
}

/// How the studio starts.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canvas size.
    pub size: Size,
    /// Canvas rate.
    pub fps: FrameRate,
    /// The preview's size and ring, or `None` for no preview.
    pub preview: Option<PreviewDesc>,
}

impl HostConfig {
    /// 1920 x 1080 at 30 frames per second with a 1280 x 720 preview where the platform
    /// has one (macOS, Windows).
    pub fn desktop() -> Self {
        Self::canvas(Size::new(1920, 1080), FrameRate::FPS_30)
    }

    /// A canvas of `size` at `fps`, with a preview of its aspect ratio where the
    /// platform has one (macOS, Windows).
    pub fn canvas(size: Size, fps: FrameRate) -> Self {
        let preview =
            cfg!(any(target_os = "macos", windows)).then(|| preview_desc(size, PREVIEW_SURFACES));
        Self { size, fps, preview }
    }
}

/// Surfaces in the preview's ring.
const PREVIEW_SURFACES: usize = 3;

/// The preview's surfaces for a canvas of `size`: its aspect ratio, the longer side
/// 1280 pixels (or the canvas's, if smaller), even sides.
fn preview_desc(size: Size, surfaces: usize) -> PreviewDesc {
    let (width, height) = crate::canvas_profile::preview_size(size.width, size.height);
    PreviewDesc {
        width,
        height,
        surfaces,
    }
}

/// The running control thread. Dropping it stops the studio and joins the thread.
pub struct StudioHost {
    tx: Option<Arc<Sender<Msg>>>,
    thread: Option<JoinHandle<()>>,
}

impl StudioHost {
    /// Starts the control thread. It makes the adapters (`adapters`, then probes them)
    /// and the GPU context (`gpu`) on that thread, starts the studio with an empty scene
    /// and reports through `notify`, which runs on the control thread and must not block.
    /// `engine` makes the studio's engine, again whenever the canvas changes.
    pub fn spawn(
        config: HostConfig,
        adapters: impl FnOnce() -> (RecordAdapters, Vec<String>) + Send + 'static,
        gpu: impl FnOnce() -> Result<Arc<GpuContext>, String> + Send + 'static,
        engine: impl Fn() -> Engine + Send + 'static,
        notify: impl Fn(Update) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(tx);
        let thread = std::thread::Builder::new()
            .name("rbs-studio".into())
            .spawn(move || run(config, adapters, gpu, engine, rx, &notify))?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Queues a command. Never blocks; a stopped studio ignores it.
    pub fn send(&self, c: Command) {
        self.post(Msg::Ui(c));
    }

    /// Serves the control API on the control thread from now on: `endpoint` comes from
    /// `rbs_api::ApiServer::start`, whose control waker is [`StudioHost::api_waker`].
    pub fn attach_api(&self, endpoint: ControlEndpoint) {
        self.post(Msg::AttachApi(Box::new(endpoint)));
    }

    /// Stops serving the control API (remote control turned off).
    pub fn detach_api(&self) {
        self.post(Msg::DetachApi);
    }

    /// Wakes the control thread when the API forwarded a request. Never blocks.
    /// It holds no strong reference to the channel, so dropping the host still closes
    /// it and ends the control thread while an `ApiServer` keeps the waker.
    pub fn api_waker(&self) -> impl Fn() + Send + Sync + 'static {
        let tx: Weak<Sender<Msg>> = self.tx.as_ref().map(Arc::downgrade).unwrap_or_default();
        move || {
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Msg::ApiWake);
            }
        }
    }

    fn post(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }
}

impl Drop for StudioHost {
    fn drop(&mut self) {
        // Closing the channel ends the loop; the thread then shuts the studio down.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The studio and the scene it runs.
struct Host {
    studio: Studio,
    adapters: RecordAdapters,
    collection: SceneCollection,
    canvas: CanvasId,
    scene: SceneId,
    rows: Vec<SourceRow>,
    undo: Option<Undo>,
    waiting: bool,
    /// The running recording's file.
    recording: Option<PathBuf>,
    /// Makes an engine for a restart.
    engine: Box<dyn Fn() -> Engine>,
    gpu: Arc<GpuContext>,
    /// The preview's ring size, when there is a preview.
    preview_surfaces: Option<usize>,
    /// Why the studio could not start again after a canvas change.
    dead: Option<String>,
    /// Counts `seq` per state key and publishes events to control API clients.
    events: EventPublisher,
    /// The control API, while remote control is on.
    api: Option<ControlEndpoint>,
    /// The recording settings as the UI saved them.
    profile: RecordProfile,
    /// Kinds the adapters can add.
    kinds: Vec<SourceKind>,
    /// Video encoders, best first.
    encoders: Vec<EncoderInfo>,
    /// Why this system cannot record.
    cannot_record: Option<String>,
    /// The running recording, as the control API reports it.
    record_state: api::RecordState,
}

fn run(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    rx: Receiver<Msg>,
    notify: &dyn Fn(Update),
) {
    let mut host = match start(config, adapters, gpu, engine) {
        Ok((host, update)) => {
            notify(update);
            host
        }
        Err(e) => {
            notify(Update::Failed(e.clone()));
            // Keep answering until the UI goes away, so it never waits on a dead thread;
            // control API requests get the reason.
            let mut api: Option<ControlEndpoint> = None;
            while let Ok(m) = rx.recv() {
                match m {
                    Msg::AttachApi(e) => api = Some(*e),
                    Msg::DetachApi => api = None,
                    Msg::Ui(_) | Msg::ApiWake => {}
                }
                if let Some(api) = &api {
                    while api.serve_one(&mut Refuse(e.clone()), &mut EventPublisher::new()) {}
                }
            }
            return;
        }
    };
    host.publish(notify);
    let mut failed: Option<String> = None;
    let mut wait = MAX_WAIT;
    loop {
        match rx.recv_timeout(wait) {
            Ok(Msg::Ui(c)) => {
                if failed.is_none() {
                    host.command(c, notify);
                }
            }
            Ok(Msg::AttachApi(endpoint)) => {
                host.events.attach(endpoint.event_link());
                host.api = Some(*endpoint);
            }
            Ok(Msg::DetachApi) => {
                host.events.detach();
                host.api = None;
            }
            Ok(Msg::ApiWake) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // One API request per round: a UI command that arrived meanwhile is taken by the
        // next `recv_timeout` first, so the local user always goes first (decision 5).
        let served_api = match &failed {
            None => host.serve_api(notify),
            Some(why) => host
                .api
                .as_ref()
                .is_some_and(|api| api.serve_one(&mut Refuse(why.clone()), &mut host.events)),
        };
        if failed.is_none()
            && let Some(why) = host.dead.take()
        {
            host.stop_recording(Some(why.clone()), notify);
            failed = Some(why.clone());
            notify(Update::Failed(why));
        }
        if failed.is_some() {
            wait = if served_api {
                Duration::ZERO
            } else {
                Duration::from_secs(3600)
            };
            continue;
        }
        let serviced = host.studio.service();
        wait = serviced.wait.unwrap_or(MAX_WAIT).min(MAX_WAIT);
        if served_api {
            // More may wait; look again right after any UI command.
            wait = Duration::ZERO;
        }
        if let Some(why) = host.studio.failure() {
            // Clients see the recording end with the failure instead of a stale state.
            host.stop_recording(Some(why.clone()), notify);
            failed = Some(why.clone());
            notify(Update::Failed(why));
            continue;
        }
        // A replacement that waited for its predecessor opens now.
        let waiting = host.studio.waiting();
        if host.waiting && !waiting {
            host.apply(notify);
        }
        host.waiting = waiting;
        if host.recording.is_some()
            && let Some(why) = host.studio.recording_failure()
        {
            host.stop_recording(Some(why), notify);
        }
        host.publish(notify);
    }
    let _ = host.studio.shutdown();
}

fn start(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
) -> Result<(Host, Update), String> {
    let (mut adapters, mut notes) = adapters();
    adapters.probe();
    let gpu = gpu()?;
    notes.push(format!("gpu: {}", gpu.adapter_name()));
    let mut collection = SceneCollection::new();
    let (canvas, _) = collection
        .add_canvas("Main", config.size, config.fps)
        .map_err(|e| format!("canvas: {e:?}"))?;
    let (scene, _) = collection
        .add_scene("Scene", SceneScope::Shared)
        .map_err(|e| format!("scene: {e:?}"))?;
    collection
        .set_program(canvas, Some(scene))
        .map_err(|e| format!("program: {e:?}"))?;
    let mut settings = StudioSettings::new(canvas);
    settings.preview = config.preview;
    let studio = Studio::start(
        engine(),
        Arc::clone(&gpu),
        &collection,
        &adapters.sources,
        &settings,
    )
    .map_err(|e| e.to_string())?;
    let display = adapters
        .sources
        .targets(&SourceKind::Screen.type_id())
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.is_default)
        .or_else(|| {
            adapters
                .sources
                .targets(&SourceKind::Screen.type_id())
                .unwrap_or_default()
                .into_iter()
                .next()
        })
        .and_then(|t| t.size);
    let installed = adapters.sources.installed();
    let kinds: Vec<SourceKind> = SourceKind::ALL
        .into_iter()
        .filter(|k| installed.contains(&k.type_id()))
        .collect();
    let encoders: Vec<EncoderInfo> = adapters
        .video_encoders
        .select(&Requirement::new())
        .candidates
        .iter()
        .map(|f| EncoderInfo {
            id: f.id().as_str().to_owned(),
            name: f.display_name().to_owned(),
            options: adapters
                .video_encoders
                .capabilities(&f.id())
                .map(EncoderOptions::from_caps)
                .unwrap_or_default(),
        })
        .collect();
    let cannot_record = encoders.is_empty().then(|| {
        format!(
            "Recording is not available on {} yet: this system has no video encoder RBS can use.",
            os_name()
        )
    });
    let update = Update::Started {
        preview: studio.preview(),
        kinds: Vec::clone(&kinds),
        notes,
        encoders: encoders.clone(),
        cannot_record: cannot_record.clone(),
        size: config.size,
        fps: config.fps,
        display,
    };
    Ok((
        Host {
            studio,
            adapters,
            collection,
            canvas,
            scene,
            rows: Vec::new(),
            undo: None,
            waiting: false,
            recording: None,
            engine: Box::new(engine),
            gpu,
            preview_surfaces: config.preview.map(|p| p.surfaces),
            dead: None,
            events: EventPublisher::new(),
            api: None,
            profile: RecordProfile::new(),
            kinds,
            encoders,
            cannot_record,
            record_state: api::RecordState::Idle { last: None },
        },
        update,
    ))
}

impl Host {
    fn command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::ListTargets(kind) => {
                let targets = self.targets(kind);
                notify(Update::Targets(kind, targets));
            }
            Command::AddSource { kind, target } => match self.add(kind, &target) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e)),
            },
            Command::DeleteSource(id) => {
                let name = self.collection.source(id).map(|s| s.name.clone());
                match self.collection.delete_source(id) {
                    Ok(undo) => {
                        self.undo = Some(undo);
                        self.apply(notify);
                        notify(Update::Deleted(name.unwrap_or_default()));
                    }
                    Err(e) => notify(Update::Error(format!("{e:?}"))),
                }
            }
            Command::UndoDelete => {
                if let Some(undo) = self.undo.take() {
                    match self.collection.undo(undo) {
                        Ok(_) => self.apply(notify),
                        Err(e) => notify(Update::Error(format!("{e:?}"))),
                    }
                }
            }
            Command::StartRecording(r) => {
                let update = match self.start_recording(&r, Some(API_PROFILE_ID.into())) {
                    Ok(update) => update,
                    Err(why) => RecordingUpdate::NotStarted(why),
                };
                notify(Update::Recording(update));
                self.publish(notify);
            }
            Command::StopRecording => {
                self.stop_recording(None, notify);
                self.publish(notify);
            }
            Command::SetCanvas { size, fps } => {
                if let Err(why) = self.change_canvas(size, fps, notify) {
                    notify(Update::Error(why));
                }
            }
            Command::SetProfile(profile) => {
                if profile != self.profile {
                    self.profile = profile;
                    self.settings_changed();
                }
            }
        }
    }

    /// What a kind can capture, in OS order.
    fn targets(&self, kind: SourceKind) -> Vec<Target> {
        self.adapters
            .sources
            .targets(&kind.type_id())
            .unwrap_or_default()
            .into_iter()
            .map(|t| Target {
                name: t.name,
                adapter: t.adapter.as_str().to_owned(),
                target: t.target,
                is_default: t.is_default,
                size: t.size,
            })
            .collect()
    }

    /// Restarts the studio with a new canvas and tells the UI what runs now (always,
    /// also when refused).
    fn change_canvas(
        &mut self,
        size: Size,
        fps: FrameRate,
        notify: &dyn Fn(Update),
    ) -> Result<(), String> {
        let before = self.api_canvas();
        let result = self.set_canvas(size, fps);
        let (size, fps) = self
            .collection
            .canvas(self.canvas)
            .map_or((size, fps), |c| (c.size, c.frame_rate));
        notify(Update::Canvas {
            preview: self.studio.preview(),
            size,
            fps,
        });
        self.rows.clear();
        self.publish(notify);
        let now = self.api_canvas();
        if now != before {
            self.events
                .changed(StateKey::topic(Topic::CanvasChanged), || {
                    EventPayload::CanvasChanged(now)
                });
            self.settings_changed();
        }
        result
    }

    /// The canvas as the control API shows it.
    fn api_canvas(&self) -> api::Canvas {
        let (size, rate) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(0, 0), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
        let r = rate.per_second();
        api::Canvas {
            width: size.width,
            height: size.height,
            fps_num: r.num(),
            fps_den: r.den(),
        }
    }

    /// The settings the control thread owns, as the control API shows them.
    fn api_settings(&self) -> api::Settings {
        api::Settings {
            profiles: vec![self.profile.to_api()],
            canvas: self.api_canvas(),
        }
    }

    fn settings_changed(&mut self) {
        let settings = self.api_settings();
        self.events
            .changed(StateKey::topic(Topic::SettingsChanged), || {
                EventPayload::SettingsChanged(settings)
            });
    }

    fn set_record_state(&mut self, state: api::RecordState) {
        self.record_state = state.clone();
        self.events
            .changed(StateKey::topic(Topic::RecordState), || {
                EventPayload::RecordState(state)
            });
    }

    /// Serves one waiting control API request; true when one was served.
    fn serve_api(&mut self, notify: &dyn Fn(Update)) -> bool {
        let Some(api) = self.api.take() else {
            return false;
        };
        // The endpoint answers `events.subscribe` from the publisher it is given, and
        // the target publishes through `self.events`: the publisher moves out for the
        // call and `ApiTarget` swaps it back in while a command runs.
        let mut events = std::mem::take(&mut self.events);
        let served = api.serve_one(&mut ApiTarget { host: self, notify }, &mut events);
        self.events = events;
        self.api = Some(api);
        served
    }

    /// Restarts the studio with the canvas at `size` and `fps`, the program scene's
    /// items placed again for it. When the new canvas does not start, the old one
    /// starts again and the error says why; when that fails too, `dead` says why.
    fn set_canvas(&mut self, size: Size, fps: FrameRate) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("Stop the recording before changing the canvas.".into());
        }
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or("the canvas is gone")?;
        let (old_size, old_fps) = (canvas.size, canvas.frame_rate);
        if (old_size, old_fps) == (size, fps) {
            return Ok(());
        }
        let before = self.collection.clone();
        self.collection
            .set_canvas_format(self.canvas, size, fps)
            .map_err(|e| format!("canvas: {e:?}"))?;
        let items: Vec<_> = self
            .collection
            .scenes_for(self.canvas)
            .flat_map(|s| s.items.iter().map(|i| (i.id, i.layout)))
            .collect();
        for (id, layout) in items {
            let moved = rescale_layout(
                &layout,
                (old_size.width, old_size.height),
                (size.width, size.height),
            );
            let _ = self.collection.set_item_layout(id, None, moved);
        }
        // Old layouts would come back with an undo.
        self.undo = None;
        let _ = self.studio.shutdown();
        match self.restart() {
            Ok(()) => Ok(()),
            Err(why) => {
                self.collection = before;
                match self.restart() {
                    Ok(()) => Err(format!(
                        "The canvas {} x {} cannot be used: {why}",
                        size.width, size.height
                    )),
                    Err(again) => {
                        self.dead = Some(again);
                        Err(why)
                    }
                }
            }
        }
    }

    /// Starts a new studio for the collection as it is.
    fn restart(&mut self) -> Result<(), String> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or("the canvas is gone")?;
        let mut settings = StudioSettings::new(self.canvas);
        settings.preview = self.preview_surfaces.map(|n| preview_desc(canvas.size, n));
        let studio = Studio::start(
            (self.engine)(),
            Arc::clone(&self.gpu),
            &self.collection,
            &self.adapters.sources,
            &settings,
        )
        .map_err(|e| e.to_string())?;
        self.studio = studio;
        self.waiting = self.studio.waiting();
        Ok(())
    }

    /// Starts recording to `r.path`; the file's folder is made first.
    fn start_recording(
        &mut self,
        r: &RecordRequest,
        profile: Option<String>,
    ) -> Result<RecordingUpdate, String> {
        if self.recording.is_some() {
            return Err("A recording is already running.".into());
        }
        if !r.path.is_absolute() {
            return Err(format!("{} is not a full path.", r.path.display()));
        }
        if let Some(folder) = r.path.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|e| format!("Cannot make the folder {}: {e}", folder.display()))?;
        }
        if r.path.exists() {
            return Err(format!("{} already exists.", r.path.display()));
        }
        let mut out = OutputSettings::new(rbs_transport_file::file_url(&r.path), r.container);
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            a.bitrate_bps = r.audio_bitrate_bps;
            a
        });
        // A picked encoder is the only one tried: if it cannot open, the recording
        // fails rather than quietly using another one.
        let all = match &r.encoder {
            None => None,
            Some(id) => {
                let only = only_encoder(&self.adapters.video_encoders, id)?;
                Some(std::mem::replace(&mut self.adapters.video_encoders, only))
            }
        };
        let started = self
            .studio
            .start_recording(&self.adapters, &out)
            .map(|report| RecordingUpdate::Started {
                path: r.path.clone(),
                video_encoder: report.video_encoder.name.clone(),
                audio_encoder: report.audio_encoder.as_ref().map(|c| c.name.clone()),
            })
            .map_err(|e| e.to_string());
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        if let Ok(RecordingUpdate::Started {
            video_encoder,
            audio_encoder,
            ..
        }) = &started
        {
            self.recording = Some(r.path.clone());
            self.set_record_state(api::RecordState::Recording {
                profile,
                video_encoder: video_encoder.clone(),
                audio_encoder: audio_encoder.clone(),
            });
        } else if std::fs::metadata(&r.path).is_ok_and(|m| m.len() == 0) {
            // The output may have made the file before a later node failed.
            let _ = std::fs::remove_file(&r.path);
        }
        started
    }

    /// Stops the recording, if one runs, and says so.
    fn stop_recording(&mut self, failure: Option<String>, notify: &dyn Fn(Update)) {
        let Some(path) = self.recording.take() else {
            return;
        };
        let report = self.studio.stop_recording();
        let (frames, dropped, flush_lost, failed) = report.map_or((0, 0, 0, None), |r| {
            (
                r.progress.frames_encoded,
                r.progress.export_drops + r.progress.encoder_drops,
                r.flush_lost,
                r.failure,
            )
        });
        let failure = failure.or(failed);
        self.set_record_state(api::RecordState::Idle {
            last: Some(api::RecordingSummary {
                failure: failure.clone(),
                frames,
                dropped,
            }),
        });
        notify(Update::Recording(RecordingUpdate::Stopped {
            path,
            failure,
            frames,
            dropped,
            flush_lost,
        }));
    }

    /// Adds a source of `kind` for `target` to the program scene, on top.
    fn add(&mut self, kind: SourceKind, target: &Target) -> Result<SourceId, String> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or("the canvas is gone")?;
        let (w, h) = (canvas.size.width, canvas.size.height);
        let fps = whole_fps(canvas.frame_rate);
        let mut settings = Settings::new();
        settings.insert(
            keys::ADAPTER.into(),
            SettingValue::Text(target.adapter.clone()),
        );
        settings.insert(
            keys::TARGET.into(),
            SettingValue::Text(target.target.clone()),
        );
        let layout = match kind {
            SourceKind::Screen | SourceKind::Window => {
                settings.insert(keys::FPS.into(), SettingValue::Int(fps));
                // As large as fits with its own aspect ratio, centred: a 21:9 display
                // is not squeezed into a 16:9 canvas.
                match target.size {
                    Some((sw, sh)) => Layout::fit(w, h, sw, sh),
                    None => Layout::new([0.0, 0.0], [w as f32, h as f32]),
                }
            }
            SourceKind::Camera => {
                let (cw, ch, cfps) = CAMERA_SIZE;
                settings.insert(keys::WIDTH.into(), SettingValue::Int(cw));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(ch));
                settings.insert(keys::FPS.into(), SettingValue::Int(cfps));
                camera_layout(w, h)
            }
            // Audio has no picture; the item only places it in the scene.
            _ => Layout::new([0.0, 0.0], [1.0, 1.0]),
        };
        let name = unique_name(&self.collection, &target.name);
        self.add_named(kind, &name, settings, layout)
    }

    fn add_named(
        &mut self,
        kind: SourceKind,
        name: &str,
        settings: Settings,
        layout: Layout,
    ) -> Result<SourceId, String> {
        let (id, _) = self
            .collection
            .add_source(name, kind.type_id(), settings)
            .map_err(|e| format!("{e:?}"))?;
        if let Err(e) = self
            .collection
            .add_item(self.scene, ItemContent::Source(id), layout, None)
        {
            let _ = self.collection.delete_source(id);
            return Err(format!("{e:?}"));
        }
        Ok(id)
    }

    /// Runs the edited collection.
    fn apply(&mut self, notify: &dyn Fn(Update)) {
        match self.studio.update(&self.collection, &self.adapters.sources) {
            Ok(u) => {
                self.waiting = self.studio.waiting();
                // Source failures show in the list; other failures need words.
                for f in u.failed.iter().filter(|f| f.node != "source") {
                    notify(Update::Error(format!("{}: {}", f.node, f.reason)));
                }
            }
            Err(e) => notify(Update::Error(e.to_string())),
        }
        self.publish(notify);
    }

    /// Sends the rows when they changed.
    fn publish(&mut self, notify: &dyn Fn(Update)) {
        let rows = self.rows();
        if rows != self.rows {
            self.rows = rows.clone();
            let listed: Vec<api::SourceInfo> = rows.iter().map(api_source).collect();
            self.events
                .changed(StateKey::topic(Topic::SourceChanged), || {
                    EventPayload::SourceChanged(listed)
                });
            notify(Update::Sources(rows));
        }
    }

    /// The program scene's sources, top first.
    fn rows(&self) -> Vec<SourceRow> {
        let states = self.studio.sources();
        let Some(scene) = self.collection.scene(self.scene) else {
            return Vec::new();
        };
        scene
            .items
            .iter()
            .rev()
            .filter_map(|item| match item.content {
                ItemContent::Source(id) => self.collection.source(id),
                _ => None,
            })
            .map(|s| {
                let state = states.iter().find(|st| st.name == s.name);
                let status = match state {
                    None => SourceStatus::Idle,
                    Some(st) => match &st.failed {
                        Some(why) => SourceStatus::Failed(why.clone()),
                        None => SourceStatus::Running,
                    },
                };
                SourceRow {
                    id: s.id,
                    name: s.name.clone(),
                    kind: SourceKind::of(&s.type_id),
                    meter: matches!(status, SourceStatus::Running)
                        .then(|| self.studio.meter(s.id).cloned())
                        .flatten(),
                    status,
                }
            })
            .collect()
    }
}

/// The control API's view of the host: one request at a time on the control thread.
struct ApiTarget<'a> {
    host: &'a mut Host,
    notify: &'a dyn Fn(Update),
}

impl ControlTarget for ApiTarget<'_> {
    fn handle(
        &mut self,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        // The real publisher comes back into the host for the command (see
        // `Host::serve_api`), so its changes publish with the right `seq`.
        std::mem::swap(&mut self.host.events, events);
        let result = self.run(command);
        std::mem::swap(&mut self.host.events, events);
        result
    }
}

fn failed(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Failed, message)
}

impl ApiTarget<'_> {
    fn run(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::StudioGet => Ok(Response::Studio(api::Snapshot {
                seq: host.events.seq_map([Topic::CanvasChanged]),
                data: api::StudioInfo {
                    canvas: host.api_canvas(),
                    kinds: host.kinds.iter().map(|k| api_kind(*k)).collect(),
                    encoders: host
                        .encoders
                        .iter()
                        .map(|e| api::EncoderInfo {
                            id: e.id.clone(),
                            name: e.name.clone(),
                            rate_controls: e.options.rate_controls.clone(),
                            h264_profiles: e.options.h264_profiles.clone(),
                            b_frames_max: e.options.b_frames_max,
                        })
                        .collect(),
                    cannot_record: host.cannot_record.clone(),
                },
            })),
            C::SourceList => Ok(Response::Sources(api::Snapshot {
                seq: host.events.seq_map([Topic::SourceChanged]),
                data: host.rows.iter().map(api_source).collect(),
            })),
            C::SourceTargets(p) => {
                let kind = app_kind(p.kind);
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets: host
                        .targets(kind)
                        .into_iter()
                        .map(|t| api::CaptureTarget {
                            name: t.name,
                            adapter: t.adapter,
                            target: t.target,
                            is_default: t.is_default,
                            size: t
                                .size
                                .map(|(width, height)| api::Dimensions { width, height }),
                        })
                        .collect(),
                }))
            }
            C::SourceAdd(p) => {
                let kind = app_kind(p.kind);
                let Some(mut target) = host
                    .targets(kind)
                    .into_iter()
                    .find(|t| t.adapter == p.adapter && t.target == p.target)
                else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("{} has no target {:?}", p.adapter, p.target),
                    ));
                };
                if let Some(name) = p.name.filter(|n| !n.trim().is_empty()) {
                    target.name = name;
                }
                let id = host.add(kind, &target).map_err(failed)?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceDelete(p) => {
                let id = SourceId::from_raw(p.source);
                let Some(name) = host.collection.source(id).map(|s| s.name.clone()) else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                };
                let undo = host
                    .collection
                    .delete_source(id)
                    .map_err(|e| failed(format!("{e:?}")))?;
                host.undo = Some(undo);
                host.apply(notify);
                notify(Update::Deleted(name));
                Ok(Response::Done(Empty {}))
            }
            C::EditUndo => {
                let Some(undo) = host.undo.take() else {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing to undo"));
                };
                host.collection
                    .undo(undo)
                    .map_err(|e| ApiError::new(ErrorKind::UndoConflict, format!("{e:?}")))?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::EditRedo => Err(failed("redo is not supported by this server yet")),
            C::OutputProfiles => Ok(Response::Profiles(ProfileList {
                profiles: vec![host.profile.to_api_listing()],
            })),
            C::RecordStart(p) => {
                check_profile(&p.profile)?;
                let path = host.profile.file_for(chrono::Local::now().naive_local());
                self.record(path, false)
            }
            C::RecordStartToFile(p) => {
                check_profile(&p.profile)?;
                let path = PathBuf::from(&p.path);
                if !path.is_absolute() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!("{} is not a full path", p.path),
                    ));
                }
                self.record(path, true)
            }
            C::RecordStop => {
                if host.recording.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
                }
                host.stop_recording(None, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::RecordStatus => Ok(Response::RecordStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::RecordState]),
                data: host.record_state.clone(),
            })),
            C::SettingsGet => Ok(Response::Settings(api::Snapshot {
                seq: host.events.seq_map([Topic::SettingsChanged]),
                data: host.api_settings(),
            })),
            C::SettingsSet(patch) => {
                let profile = if let Some(profiles) = &patch.profiles {
                    let [p] = profiles.as_slice() else {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "this server has exactly one recording profile",
                        ));
                    };
                    check_profile(&p.id)?;
                    let profile = RecordProfile::from_api(p);
                    if let Some(why) = profile.problem() {
                        return Err(ApiError::new(ErrorKind::InvalidParams, why));
                    }
                    if host.recording.is_some() && profile != host.profile {
                        return Err(ApiError::new(
                            ErrorKind::Conflict,
                            "stop the recording before changing its settings",
                        ));
                    }
                    Some(profile)
                } else {
                    None
                };
                // The canvas goes first: it is the part that can still fail, and then
                // nothing of the patch has been applied.
                if let Some(canvas) = &patch.canvas {
                    let c = canvas_profile(canvas)?;
                    let same = host
                        .collection
                        .canvas(host.canvas)
                        .is_some_and(|k| k.size == c.size() && k.frame_rate == c.frame_rate());
                    if !same {
                        if host.recording.is_some() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "stop the recording before changing the canvas",
                            ));
                        }
                        host.change_canvas(c.size(), c.frame_rate(), notify)
                            .map_err(failed)?;
                    }
                }
                if let Some(profile) = profile
                    && profile != host.profile
                {
                    host.profile = profile.clone();
                    notify(Update::Profile(profile));
                    host.settings_changed();
                }
                Ok(Response::Settings(api::Snapshot {
                    seq: host.events.seq_map([Topic::SettingsChanged]),
                    data: host.api_settings(),
                }))
            }
            // Answered by the endpoint; never reaches a target.
            C::EventsSubscribe(_) => Err(rbs_api::control::unsupported(&command)),
        }
    }

    /// Starts recording the program with the saved profile into `path`.
    fn record(&mut self, path: PathBuf, explicit: bool) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if explicit && !path.is_absolute() {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!("{} is not a full path", path.display()),
            ));
        }
        if explicit && path.exists() {
            return Err(ApiError::new(
                ErrorKind::FileExists,
                format!("{} already exists", path.display()),
            ));
        }
        if let Some(why) = &host.cannot_record {
            return Err(failed(why.clone()));
        }
        if host.recording.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a recording is already running",
            ));
        }
        let p = &host.profile;
        if let Some(why) = p.problem() {
            return Err(failed(why));
        }
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = p
            .encoder
            .clone()
            .filter(|id| host.encoders.iter().any(|e| e.id == *id));
        let request = RecordRequest {
            path,
            container: p.format.container(),
            encoder,
            video: p.video_settings(),
            audio_bitrate_bps: u64::from(p.audio_kbps) * 1000,
        };
        let profile = (!explicit).then(|| API_PROFILE_ID.to_owned());
        let result = host.start_recording(&request, profile);
        host.publish(notify);
        match result {
            Ok(update) => {
                let started = match &update {
                    RecordingUpdate::Started {
                        video_encoder,
                        audio_encoder,
                        ..
                    } => RecordStarted {
                        video_encoder: video_encoder.clone(),
                        audio_encoder: audio_encoder.clone(),
                    },
                    _ => return Err(failed("the recording did not start")),
                };
                // The UI shows the recording as if its own button started it.
                notify(Update::Recording(update));
                Ok(Response::RecordStarted(started))
            }
            Err(why) => Err(failed(why)),
        }
    }
}

fn check_profile(id: &str) -> Result<(), ApiError> {
    if id == API_PROFILE_ID {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no recording profile {id:?}"),
        ))
    }
}

fn canvas_profile(c: &api::Canvas) -> Result<CanvasProfile, ApiError> {
    if c.fps_den != 1 {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "the canvas rate must be a whole number of frames per second",
        ));
    }
    let profile = CanvasProfile {
        width: c.width,
        height: c.height,
        fps: c.fps_num,
        ..CanvasProfile::new()
    };
    match profile.problem() {
        Some(why) => Err(ApiError::new(ErrorKind::InvalidParams, why)),
        None => Ok(profile),
    }
}

/// Answers every control API request with why the studio cannot run.
struct Refuse(String);

impl ControlTarget for Refuse {
    fn handle(
        &mut self,
        _command: rbs_api::Command,
        _events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        Err(failed(format!("the studio stopped: {}", self.0)))
    }
}

fn api_kind(k: SourceKind) -> api::SourceKind {
    match k {
        SourceKind::Screen => api::SourceKind::Screen,
        SourceKind::Window => api::SourceKind::Window,
        SourceKind::Camera => api::SourceKind::Camera,
        SourceKind::AudioInput => api::SourceKind::AudioInput,
        SourceKind::SystemAudio => api::SourceKind::SystemAudio,
        SourceKind::ApplicationAudio => api::SourceKind::ApplicationAudio,
    }
}

fn app_kind(k: api::SourceKind) -> SourceKind {
    match k {
        api::SourceKind::Screen => SourceKind::Screen,
        api::SourceKind::Window => SourceKind::Window,
        api::SourceKind::Camera => SourceKind::Camera,
        api::SourceKind::AudioInput => SourceKind::AudioInput,
        api::SourceKind::SystemAudio => SourceKind::SystemAudio,
        api::SourceKind::ApplicationAudio => SourceKind::ApplicationAudio,
    }
}

fn api_source(row: &SourceRow) -> api::SourceInfo {
    api::SourceInfo {
        id: row.id.get(),
        name: row.name.clone(),
        kind: row.kind.map(api_kind),
        status: match &row.status {
            SourceStatus::Running => api::SourceStatus::Running,
            SourceStatus::Idle => api::SourceStatus::Idle,
            SourceStatus::Failed(why) => api::SourceStatus::Failed {
                reason: why.clone(),
            },
        },
    }
}

/// A registry with only the encoder `id` of `all`, probed.
fn only_encoder<F: ?Sized + AdapterFactory>(
    all: &Registry<F>,
    id: &str,
) -> Result<Registry<F>, String> {
    let factory = all
        .select(&Requirement::new())
        .candidates
        .into_iter()
        .find(|f| f.id().as_str() == id)
        .ok_or_else(|| format!("The encoder {id} is not available on this system."))?;
    let mut only = Registry::new();
    only.register(factory).map_err(|e| e.to_string())?;
    only.probe_all();
    Ok(only)
}

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// `base`, or `base 2`, `base 3`... whichever no source has yet.
fn unique_name(collection: &SceneCollection, base: &str) -> String {
    let base = if base.trim().is_empty() {
        "Source"
    } else {
        base.trim()
    };
    let taken = |n: &str| collection.sources().any(|s| s.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_owned())
}

fn whole_fps(rate: FrameRate) -> i64 {
    let r = rate.per_second();
    i64::from(r.num().div_ceil(r.den().max(1)))
}

/// The camera's rectangle: a 16:9 picture 4/15 of the canvas wide, a margin of 1/40
/// of the width from the bottom-right corner (as `rbs-cli record` places it).
pub fn camera_layout(width: u32, height: u32) -> Layout {
    let w = width as f32 * 4.0 / 15.0;
    let h = w * 9.0 / 16.0;
    let margin = width as f32 / 40.0;
    Layout::new(
        [width as f32 - w - margin, height as f32 - h - margin],
        [w, h],
    )
}

/// `layout` on a canvas of `from` placed again on a canvas of `to`. A picture that
/// fills the canvas's width or height, centred (as a screen is added), is fitted to the
/// new canvas with its own aspect ratio. Anything else keeps its size relative to the
/// canvas (the smaller of the two scale factors, so its shape stays) and its margin to
/// the nearest edges: a camera in a corner stays in that corner.
pub fn rescale_layout(layout: &Layout, from: (u32, u32), to: (u32, u32)) -> Layout {
    let (fw, fh) = (from.0.max(1) as f32, from.1.max(1) as f32);
    let (tw, th) = (to.0 as f32, to.1 as f32);
    let [x, y] = layout.position;
    let [w, h] = layout.size;
    let near = |a: f32, b: f32| (a - b).abs() < 1.0;
    let centred = near(x + w / 2.0, fw / 2.0) && near(y + h / 2.0, fh / 2.0);
    let mut out = *layout;
    if centred && (near(w.abs(), fw) || near(h.abs(), fh)) && w != 0.0 && h != 0.0 {
        // Fit: the limiting side takes the canvas's size.
        let (nw, nh) = if w.abs() * th >= h.abs() * tw {
            (tw, tw * h.abs() / w.abs())
        } else {
            (th * w.abs() / h.abs(), th)
        };
        out.size = [nw * w.signum(), nh * h.signum()];
        out.position = [(tw - nw) / 2.0, (th - nh) / 2.0];
        return out;
    }
    let s = (tw / fw).min(th / fh);
    let axis = |pos: f32, len: f32, old: f32, new: f32| {
        let size = len * s;
        let centre = pos + len / 2.0;
        let pos = if near(centre, old / 2.0) {
            (new - size) / 2.0
        } else if centre < old / 2.0 {
            pos * s
        } else {
            new - (old - pos - len) * s - size
        };
        (pos, size)
    };
    let (nx, nw) = axis(x, w, fw, tw);
    let (ny, nh) = axis(y, h, fh, th);
    out.position = [nx, ny];
    out.size = [nw, nh];
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fitted_screen_is_fitted_again() {
        let l = Layout::fit(1920, 1080, 3440, 1440);
        let got = rescale_layout(&l, (1920, 1080), (3440, 1440));
        assert_eq!(got, Layout::new([0.0, 0.0], [3440.0, 1440.0]));
        let full = Layout::new([0.0, 0.0], [1920.0, 1080.0]);
        let got = rescale_layout(&full, (1920, 1080), (3440, 1440));
        assert_eq!(got, Layout::fit(3440, 1440, 1920, 1080));
        let got = rescale_layout(&full, (1920, 1080), (1280, 720));
        assert_eq!(got, Layout::new([0.0, 0.0], [1280.0, 720.0]));
    }

    #[test]
    fn a_camera_stays_in_its_corner() {
        let got = rescale_layout(&camera_layout(1920, 1080), (1920, 1080), (3840, 2160));
        assert_eq!(got, camera_layout(3840, 2160));
        let got = rescale_layout(&camera_layout(1920, 1080), (1920, 1080), (3440, 1440));
        let s = 1440.0 / 1080.0;
        let cam = camera_layout(1920, 1080);
        let right = 1920.0 - cam.position[0] - cam.size[0];
        assert!((got.size[0] - cam.size[0] * s).abs() < 0.01);
        assert!((3440.0 - got.position[0] - got.size[0] - right * s).abs() < 0.01);
        assert!((1440.0 - got.position[1] - got.size[1] - right * s).abs() < 0.01);
    }

    #[test]
    fn previews_keep_the_canvas_aspect() {
        let d = preview_desc(Size::new(3440, 1440), 3);
        assert_eq!((d.width, d.height, d.surfaces), (1280, 534, 3));
    }
}
