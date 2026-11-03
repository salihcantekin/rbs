//! The studio's control thread (ADR-0041 decision 7): owns the `Studio`, the scene
//! collection and the adapter registries. The UI only sends [`Command`]s and gets
//! [`Update`]s through a callback; it never calls the studio. The preview reaches the UI
//! through the studio's `PreviewEndpoint`, as it would from another process.
//!
//! Toolkit-free, so it is tested without a window (`tests/studio_host.rs`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_audio::LevelMeter;
use rbs_codec::RateControl;
use rbs_core::capability::Requirement;
use rbs_core::media::{ContainerId, Size};
use rbs_core::registry::{AdapterFactory, Registry};
use rbs_core::timing::FrameRate;
use rbs_engine::Engine;
use rbs_gpu::GpuContext;
use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_graph::node::NodeTypeId;
use rbs_record::{AudioSettings, OutputSettings, RecordAdapters, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::Undo;
use rbs_scene::model::{
    CanvasId, ItemContent, Layout, SceneId, SceneScope, SettingValue, Settings, SourceId,
};
use rbs_sources::types::{self, keys};

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
}

/// A video encoder the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Its adapter id.
    pub id: String,
    /// Its name in the UI.
    pub name: String,
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
    /// Video bitrate (constant) in bit/s.
    pub video_bitrate_bps: u64,
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
        let preview = cfg!(any(target_os = "macos", windows)).then_some(PreviewDesc {
            width: 1280,
            height: 720,
            surfaces: 3,
        });
        Self {
            size: Size::new(1920, 1080),
            fps: FrameRate::FPS_30,
            preview,
        }
    }
}

/// The running control thread. Dropping it stops the studio and joins the thread.
pub struct StudioHost {
    tx: Option<Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl StudioHost {
    /// Starts the control thread. It makes the adapters (`adapters`, then probes them)
    /// and the GPU context (`gpu`) on that thread, starts the studio with an empty scene
    /// and reports through `notify`, which runs on the control thread and must not block.
    pub fn spawn(
        config: HostConfig,
        adapters: impl FnOnce() -> (RecordAdapters, Vec<String>) + Send + 'static,
        gpu: impl FnOnce() -> Result<Arc<GpuContext>, String> + Send + 'static,
        engine: impl FnOnce() -> Engine + Send + 'static,
        notify: impl Fn(Update) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
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
        if let Some(tx) = &self.tx {
            let _ = tx.send(c);
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
}

fn run(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl FnOnce() -> Engine,
    rx: Receiver<Command>,
    notify: &dyn Fn(Update),
) {
    let mut host = match start(config, adapters, gpu, engine) {
        Ok((host, update)) => {
            notify(update);
            host
        }
        Err(e) => {
            notify(Update::Failed(e));
            // Keep answering until the UI goes away, so it never waits on a dead thread.
            while rx.recv().is_ok() {}
            return;
        }
    };
    host.publish(notify);
    let mut failed = false;
    let mut wait = MAX_WAIT;
    loop {
        match rx.recv_timeout(wait) {
            Ok(c) => {
                if !failed {
                    host.command(c, notify);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if failed {
            wait = Duration::from_secs(3600);
            continue;
        }
        let serviced = host.studio.service();
        wait = serviced.wait.unwrap_or(MAX_WAIT).min(MAX_WAIT);
        if let Some(why) = host.studio.failure() {
            failed = true;
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
    engine: impl FnOnce() -> Engine,
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
    let studio = Studio::start(engine(), gpu, &collection, &adapters.sources, &settings)
        .map_err(|e| e.to_string())?;
    let installed = adapters.sources.installed();
    let kinds = SourceKind::ALL
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
        kinds,
        notes,
        encoders,
        cannot_record,
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
        },
        update,
    ))
}

impl Host {
    fn command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::ListTargets(kind) => {
                let targets = self
                    .adapters
                    .sources
                    .targets(&kind.type_id())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| Target {
                        name: t.name,
                        adapter: t.adapter.as_str().to_owned(),
                        target: t.target,
                        is_default: t.is_default,
                    })
                    .collect();
                notify(Update::Targets(kind, targets));
            }
            Command::AddSource { kind, target } => match self.add(kind, &target) {
                Ok(()) => self.apply(notify),
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
                let update = match self.start_recording(&r) {
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
        }
    }

    /// Starts recording to `r.path`; the file's folder is made first.
    fn start_recording(&mut self, r: &RecordRequest) -> Result<RecordingUpdate, String> {
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
        out.video.rate_control = RateControl::Cbr {
            bitrate_bps: r.video_bitrate_bps,
        };
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
        if started.is_ok() {
            self.recording = Some(r.path.clone());
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
        notify(Update::Recording(RecordingUpdate::Stopped {
            path,
            failure: failure.or(failed),
            frames,
            dropped,
            flush_lost,
        }));
    }

    /// Adds a source of `kind` for `target` to the program scene, on top.
    fn add(&mut self, kind: SourceKind, target: &Target) -> Result<(), String> {
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
                Layout::new([0.0, 0.0], [w as f32, h as f32])
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
        let (id, _) = self
            .collection
            .add_source(&name, kind.type_id(), settings)
            .map_err(|e| format!("{e:?}"))?;
        if let Err(e) = self
            .collection
            .add_item(self.scene, ItemContent::Source(id), layout, None)
        {
            let _ = self.collection.delete_source(id);
            return Err(format!("{e:?}"));
        }
        Ok(())
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
