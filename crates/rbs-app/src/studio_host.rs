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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_api::request::{Empty, ProfileList, RecordStarted, SourceAdded, StreamStarted, TargetList};
use rbs_api::types as api;
use rbs_api::{
    ApiError, ControlEndpoint, ControlTarget, ErrorKind, EventPayload, EventPublisher, Response,
    StateKey, Topic,
};
use rbs_audio::LevelMeter;
use rbs_capture::CaptureError;
use rbs_codec::caps as codec_caps;
use rbs_core::capability::{CapabilitySet, CapabilityValue, Requirement};
use rbs_core::media::{ContainerId, Size};
use rbs_core::registry::{AdapterFactory, Registry};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_engine::Engine;
use rbs_gpu::GpuContext;
use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AudioSettings, Link, OutputId, OutputSettings, Progress, RecordAdapters, Studio,
    StudioSettings, VideoSettings,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::{
    Border, CanvasId, Crop, ItemContent, ItemId, ItemStyle, Layout, SceneId, SceneItem, SceneScope,
    SettingValue, Settings, Shadow, Shape, SourceId,
};
use rbs_scene::{SceneError, Undo};
use rbs_sources::types::{self, keys};
use rbs_sources::{SourceError, SourceTarget};
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};

use crate::canvas_profile::CanvasProfile;
use crate::edit_history::{EditKind, History, HistoryError, StepInfo};
use crate::record_profile::{API_PROFILE_ID, RecordProfile};
use crate::remote_control::ApiTokens;
use crate::stream_key::{self, KeyStore, MemoryKeyStore};
use crate::stream_profile::{Protocol, StreamProfile, parse_server};
use crate::user_session::CollectionSaver;

/// The longest the control thread sleeps between two rounds of the engine's control
/// loop (the studio asks for at most about 100 ms).
const MAX_WAIT: Duration = Duration::from_millis(50);
/// How often the Stats panel gets new counters while an output runs (its redraw rate,
/// design system: `rbs.stats` 1 Hz).
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// Control-loop wait while a stopped recording's tail comes in.
const ENDING_WAIT: Duration = Duration::from_millis(5);
/// Hang guard for the tail when the app closes while recording (the studio itself
/// gives up after `rbs_record::END_TAIL_LIMIT` on the master clock).
const CLOSE_TAIL_GUARD: Duration = Duration::from_secs(1);
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
    /// Not a target yet: adding it shows the OS picker of `adapter` (systems that
    /// list no screens or windows, such as the Linux screen-cast portal), and the
    /// target the user chooses is added.
    pub picker: bool,
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
    /// Where its picture is on the canvas (video sources); `None` for sound only.
    pub layout: Option<Layout>,
    /// Its item in the program scene.
    pub item: ItemId,
    /// Width and height in pixels of its picture, as last seen while it ran (video
    /// sources, ADR-0057); `None` before it ran.
    pub picture: Option<(u32, u32)>,
}

impl PartialEq for SourceRow {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.kind == other.kind
            && self.status == other.status
            && self.layout == other.layout
            && self.item == other.item
            && self.picture == other.picture
            && match (&self.meter, &other.meter) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// A part of a source's picture the Region menu shows (ADR-0057).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropRegion {
    /// The whole picture: no crop, fitted to the canvas like a new source.
    Full,
    /// The centre, as large as the canvas (or the picture, if smaller), shown 1:1 when
    /// the picture is at least as large as the canvas.
    CanvasCentred,
    /// The left half.
    LeftHalf,
    /// The right half.
    RightHalf,
}

impl CropRegion {
    /// The crop that shows this part of a `w x h` picture on a `cw x ch` canvas.
    pub fn crop(self, (w, h): (u32, u32), (cw, ch): (u32, u32)) -> Crop {
        match self {
            CropRegion::Full => Crop::default(),
            CropRegion::CanvasCentred => {
                let (sw, sh) = (cw.min(w), ch.min(h));
                let (l, t) = ((w - sw) / 2, (h - sh) / 2);
                Crop::new(l, t, w - sw - l, h - sh - t)
            }
            CropRegion::LeftHalf => Crop::new(0, 0, w - w / 2, 0),
            CropRegion::RightHalf => Crop::new(w / 2, 0, 0, 0),
        }
    }
}

/// Why a crop was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CropRefusal {
    /// No such item, or its source is gone.
    NotFound(String),
    /// The request cannot be met (a nested scene, a crop that leaves nothing).
    Invalid(String),
    /// The scale cannot be kept: the source never showed a picture.
    NoPicture,
    /// The scene refused the edit.
    Failed(String),
}

impl CropRefusal {
    fn text(&self) -> String {
        match self {
            CropRefusal::NotFound(t) | CropRefusal::Invalid(t) | CropRefusal::Failed(t) => {
                t.clone()
            }
            CropRefusal::NoPicture => {
                "This source has no picture yet, so its crop cannot keep the scale.".into()
            }
        }
    }
}

const NO_AREA: &str = "The crop must leave at least one pixel of the picture in each direction.";

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
    /// The toast's Undo after a delete: undoes the newest edit if it is a delete, else
    /// nothing (another edit came after it).
    UndoDelete,
    /// Undo the newest scene edit (U4): an add, a delete, a layer move or a box; all
    /// the boxes of one drag are one edit. [`Update::History`] says what is next.
    Undo,
    /// Redo the edit undone last.
    Redo,
    /// Move a source one layer up (`up`, towards the top of the list, drawn over the
    /// one above) or down in the program scene. Nothing happens at the end of the list.
    MoveSource {
        /// The source.
        id: SourceId,
        /// Up, else down.
        up: bool,
    },
    /// Place a video source's picture: the box's top-left corner and size in canvas
    /// pixels. Rotation, crop and opacity stay. A size of zero or a value that is not
    /// finite is refused with [`Update::Error`]. Always answered with
    /// [`Update::BoxPlaced`] carrying `seq`, after any new rows, also when nothing
    /// changed, so the UI can send the next box of a drag (ADR-0048).
    SetSourceBox {
        /// The caller's number for this request.
        seq: u64,
        /// Boxes with the same number other than 0 that follow each other are one undo
        /// step (one drag on the picture panel); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// Top-left corner.
        position: [f32; 2],
        /// Width and height.
        size: [f32; 2],
    },
    /// Cut source pixels from the edges of a video source's picture (ADR-0057), on the
    /// layout the canvas shows. With `keep_scale`, the box changes so a source pixel
    /// keeps its size and what stays keeps its place. Always answered with
    /// [`Update::CropPlaced`] carrying `seq`, also when nothing changed; a refusal is
    /// also an [`Update::Error`].
    SetSourceCrop {
        /// The caller's number for this request.
        seq: u64,
        /// Crops with the same number other than 0 that follow each other are one undo
        /// step (one drag on the picture panel); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// Pixels cut from the left, top, right and bottom.
        crop: [u32; 4],
        /// Keep the scale (else the cropped picture fills the box).
        keep_scale: bool,
    },
    /// Show a part of a video source's picture filling as much of the canvas as fits
    /// (ADR-0057, Properties' Show buttons). One undo step.
    SetSourceRegion {
        /// The source.
        id: SourceId,
        /// Which part.
        region: CropRegion,
    },
    /// Shape and border of a video source's picture (ADR-0053), replacing the whole
    /// style of the layout the canvas shows. A style that is not valid is refused with
    /// [`Update::Error`].
    SetSourceStyle {
        /// Styles with the same number other than 0 that follow each other are one undo
        /// step (one slider drag); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// The new style.
        style: ItemStyle,
    },
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
    /// Go live: stream the program ([`Update::Stream`] says how it went). The stream key
    /// is read from the host's key store now, never earlier.
    StartStream(StreamRequest),
    /// End the running stream ([`StreamUpdate::Ended`] when it has).
    EndStream,
    /// The stream settings as saved: what `stream.start` uses for control API clients.
    /// Sent at start and after each change.
    SetStreamProfile(StreamProfile),
    /// Save the stream key in the key store (`None` removes it); answered with
    /// [`Update::StreamKey`].
    SaveStreamKey(Option<Secret>),
    /// The key store account of the output profile in use ([`stream_key::account`]):
    /// where Go live, `stream.start` and [`Command::SaveStreamKey`] read and write the
    /// key from now on. Sent when the user switches output profiles.
    SetKeyAccount(String),
    /// Remove the key saved for a deleted output profile's account (ADR-0051 decision
    /// 5). A failure is reported with [`Update::Error`].
    RemoveStreamKey(String),
    /// Run another scene collection (Scene collection menu): the studio restarts with
    /// it, as for a canvas change, and its first canvas's program scene is shown.
    /// Refused while recording or streaming. Changes are saved through `autosave` from
    /// now on. Always answered with [`Update::Canvas`] and then
    /// [`Update::CollectionSwitched`] carrying `seq`; on a refusal or failure the
    /// collection that ran keeps running.
    SwitchCollection {
        /// The caller's number for this request.
        seq: u64,
        /// The collection.
        collection: Box<SceneCollection>,
        /// Saves it after every change; `None`: changes are not saved.
        autosave: Option<CollectionSaver>,
    },
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

/// Where and how to stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    /// The server URL (`rtmp://host/app`, `srt://host:port`); never holds the key.
    pub url: String,
    /// Packets (RTMP) or the chunks of a container (SRT: MPEG-TS).
    pub input: TransportInput,
    /// The server needs a key (RTMP): without a saved one nothing opens.
    pub needs_key: bool,
    /// What the transport does when the connection drops.
    pub reconnect: ReconnectPolicy,
    /// Opaque transport options (SRT `latency_ms`).
    pub transport_options: Vec<(String, String)>,
    /// The video encoder's adapter id: only this one is used. `None`: the best one.
    pub encoder: Option<String>,
    /// Video encoding.
    pub video: VideoSettings,
    /// Audio bitrate in bit/s.
    pub audio_bitrate_bps: u64,
}

impl StreamRequest {
    /// The request for the saved stream settings, with `encoder` as the video encoder
    /// (`None`: the best one). Says why when they cannot be used: no server, a server
    /// that is not valid, or settings out of range. The key is not checked here: the
    /// host reads it from the key store when the stream starts.
    pub fn from_profile(p: &StreamProfile, encoder: Option<String>) -> Result<Self, String> {
        if let Some(why) = p.problem() {
            return Err(why);
        }
        if p.server.is_empty() {
            return Err("Enter the server in Settings, Stream.".into());
        }
        let server = parse_server(&p.server)?;
        let e = &p.encoding;
        Ok(Self {
            url: p.server.clone(),
            input: server.protocol.input(),
            needs_key: server.protocol.needs_key(),
            reconnect: p.reconnect_policy(),
            transport_options: p.transport_options(),
            encoder,
            video: e.video_settings(),
            audio_bitrate_bps: e.audio_bps(),
        })
    }
}

/// What happened to the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamUpdate {
    /// It runs and connects in the background ([`StreamUpdate::Link`] follows).
    Started {
        /// The server URL.
        url: String,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is sent.
        audio_encoder: Option<String>,
    },
    /// It could not start, in words; nothing streams.
    NotStarted(String),
    /// The server needs a key and the key store has none; nothing streams.
    NoKey,
    /// The connection changed: connected, lost, another attempt.
    Link(Link),
    /// It ended, asked to or because it failed for good.
    Ended {
        /// Why it ended on its own, if it did.
        failure: Option<String>,
        /// Video frames encoded.
        frames: u64,
        /// Video frames dropped at the export or the encoder, and items the transport
        /// dropped while the connection was slow or down.
        dropped: u64,
        /// Times the stream connected (0: it never reached the server).
        connects: u64,
    },
}

/// What the studio tells the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The answer to [`Command::SetSourceBox`] number `seq`: `ok` when the box is the
    /// source's now (changed or already so), else it was refused (an
    /// [`Update::Error`] says why).
    BoxPlaced {
        /// The request's number.
        seq: u64,
        /// Taken.
        ok: bool,
    },
    /// The answer to [`Command::SetSourceCrop`] number `seq`, as [`Update::BoxPlaced`].
    CropPlaced {
        /// The request's number.
        seq: u64,
        /// Taken.
        ok: bool,
    },
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
    /// What [`Command::Undo`] and [`Command::Redo`] would apply now (`None`: nothing).
    /// Sent when it changes.
    History {
        /// The next undo.
        undo: Option<StepInfo>,
        /// The next redo.
        redo: Option<StepInfo>,
    },
    /// A command did not work, in words for a toast.
    Error(String),
    /// The recording started, did not start or stopped.
    Recording(RecordingUpdate),
    /// The stream started, did not start, changed its connection or ended.
    Stream(StreamUpdate),
    /// [`Command::SaveStreamKey`] done: `Ok(true)` a key is saved, `Ok(false)` none is,
    /// `Err` why the store refused.
    StreamKey(Result<bool, String>),
    /// A control API client changed the recording settings; the UI shows and saves
    /// them.
    Profile(RecordProfile),
    /// A control API client changed the stream settings (not the server or the key);
    /// the UI shows and saves them.
    StreamProfile(StreamProfile),
    /// A control API client (`rbs-cli token` over local IPC) made or revoked an access
    /// token: it is saved and in effect; the UI reads the token list again.
    TokensChanged,
    /// The running outputs' counters for the Stats panel, once a second while any
    /// output runs.
    Stats(Stats),
    /// [`Command::SwitchCollection`] with this `seq` is done: `Ok` the new collection
    /// runs, `Err` why the one before still runs.
    CollectionSwitched {
        /// The request's number.
        seq: u64,
        /// How it went.
        result: Result<(), String>,
    },
    /// After a caught panic: how many outputs the host did not track and stopped
    /// (tests only, ADR-0055).
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    Recovered {
        /// Untracked outputs stopped.
        stopped: usize,
    },
}

/// Counters of one running output, for the Stats panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputStats {
    /// Frames the video encoder took.
    pub frames: u64,
    /// Frames lost before or at the encoder: dropped at export, replaced in the queue
    /// into the encoder, or dropped by the encoder.
    pub dropped: u64,
    /// Packets the output took (all streams).
    pub packets: u64,
    /// The transport reaches its destination now (a stream is live).
    pub connected: bool,
    /// Connection attempts after a failure.
    pub reconnect_attempts: u64,
    /// Items the transport dropped while the connection was slow or down.
    pub transport_drops: u64,
}

impl OutputStats {
    fn of(p: &Progress) -> Self {
        Self {
            frames: p.frames_encoded,
            dropped: p.export_drops + p.encoder_skipped + p.encoder_drops,
            packets: p.packets_taken,
            connected: p.connected,
            reconnect_attempts: p.reconnect_attempts,
            transport_drops: p.transport_drops,
        }
    }
}

/// What the Stats panel shows: the recording's and the stream's counters (when they
/// run) and the engine's late ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// The running recording's counters.
    pub recording: Option<OutputStats>,
    /// The running stream's counters.
    pub stream: Option<OutputStats>,
    /// Render ticks skipped because the render worker was late.
    pub late_render_ticks: u64,
    /// Audio ticks skipped because the audio worker was late.
    pub late_audio_ticks: u64,
}

impl Stats {
    fn of(recording: Option<&Progress>, stream: Option<&Progress>) -> Self {
        let any = recording.or(stream);
        Self {
            recording: recording.map(OutputStats::of),
            stream: stream.map(OutputStats::of),
            late_render_ticks: any.map_or(0, |p| p.late_render_ticks),
            late_audio_ticks: any.map_or(0, |p| p.late_audio_ticks),
        }
    }
}

/// What the control thread receives.
enum Msg {
    Ui(Command),
    AttachApi(Box<Api>),
    DetachApi,
    /// A control API request is waiting.
    ApiWake,
    /// An OS picker failed (not cancelled).
    PickFailed(String),
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
    /// Where the stream key is kept: read when a stream starts, written when it is
    /// saved, on the control thread only. Memory by default.
    pub keys: Arc<dyn KeyStore>,
    /// The key store account of the output profile in use ([`stream_key::account`]);
    /// [`stream_key::ACCOUNT`] by default.
    pub key_account: String,
    /// The scene collection to run (ADR-0051). Its first canvas is the studio's; that
    /// canvas's program scene is the one shown and edited (a scene is added when it
    /// has none). `None`: a new collection with one canvas of `size` at `fps`.
    pub collection: Option<SceneCollection>,
    /// Saves the collection after every change (on the user data writer thread);
    /// `None`: changes are not saved.
    pub autosave: Option<CollectionSaver>,
    /// A step to panic at once (tests only, ADR-0055).
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fault: Option<Fault>,
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
        Self {
            size,
            fps,
            preview,
            keys: Arc::new(MemoryKeyStore::new()),
            key_account: stream_key::ACCOUNT.to_owned(),
            collection: None,
            autosave: None,
            #[cfg(feature = "fault-injection")]
            fault: None,
        }
    }
}

/// A panic the host raises once at a chosen step, for the crash-resilience tests
/// (ADR-0055). Only with the `fault-injection` feature, which release builds never use.
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// In a recording start, after the output started and before the host tracks it.
    AfterOutputStart,
    /// In a recording stop, after the output stopped and before the host forgets it.
    InRecordingStop,
}

/// Panics once if the host was told to fail at `$fault`.
macro_rules! inject {
    ($host:expr, $fault:expr) => {
        #[cfg(feature = "fault-injection")]
        if $host.fault == Some($fault) {
            $host.fault = None;
            panic!("injected fault: {:?}", $fault);
        }
    };
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
            .spawn({
                let inbox = Arc::downgrade(&tx);
                move || run(config, adapters, gpu, engine, rx, inbox, &notify)
            })?;
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
    /// With `tokens`, `tokens.*` requests are answered (local peers only, by scope);
    /// without, they fail.
    pub fn attach_api(&self, endpoint: ControlEndpoint, tokens: Option<ApiTokens>) {
        self.post(Msg::AttachApi(Box::new(Api { endpoint, tokens })));
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

/// The control API while it is attached.
struct Api {
    endpoint: ControlEndpoint,
    tokens: Option<ApiTokens>,
}

impl Api {
    /// Serves one request: `tokens.*` from the settings, everything else by `target`.
    fn serve_one(
        &self,
        target: &mut dyn ControlTarget,
        events: &mut EventPublisher,
        notify: &dyn Fn(Update),
    ) -> bool {
        let mut with_tokens = WithTokens {
            inner: target,
            tokens: self.tokens.as_ref(),
            notify,
        };
        self.endpoint.serve_one(&mut with_tokens, events)
    }
}

/// Answers `tokens.*` (they do not touch the studio, so they work also after it
/// failed), and passes everything else on.
struct WithTokens<'a> {
    inner: &'a mut dyn ControlTarget,
    tokens: Option<&'a ApiTokens>,
    notify: &'a dyn Fn(Update),
}

impl ControlTarget for WithTokens<'_> {
    fn handle(
        &mut self,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        if let Some(answer) = self.tokens.and_then(|t| t.answer(&command)) {
            let (response, changed) = answer?;
            if changed {
                (self.notify)(Update::TokensChanged);
            }
            return Ok(response);
        }
        self.inner.handle(command, events)
    }
}

/// A listed target as the UI shows it.
fn target_of(t: SourceTarget) -> Target {
    Target {
        name: t.name,
        adapter: t.adapter.as_str().to_owned(),
        target: t.target,
        is_default: t.is_default,
        size: t.size,
        picker: false,
    }
}

/// The entry that opens the OS picker for `kind` (glossary: screen, window).
fn picker_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Window => "Choose a window…",
        _ => "Choose a screen…",
    }
}

fn kind_noun(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Window => "window",
        _ => "screen",
    }
}

/// The studio and the scene it runs.
struct Host {
    /// This thread's own queue, for results from picker threads. Weak, so dropping
    /// the [`StudioHost`] still ends the thread.
    inbox: Weak<Sender<Msg>>,
    /// Saves the collection after a change ([`HostConfig::autosave`]).
    autosave: Option<CollectionSaver>,
    /// The collection as last handed to `autosave`.
    saved: Option<SceneCollection>,
    studio: Studio,
    adapters: RecordAdapters,
    collection: SceneCollection,
    canvas: CanvasId,
    scene: SceneId,
    rows: Vec<SourceRow>,
    /// The last picture size seen per video source (ADR-0057).
    sizes: BTreeMap<SourceId, (u32, u32)>,
    /// The user's undo history of scene edits, shared with control API clients.
    history: History<Undo>,
    /// The next undo and redo as last sent.
    history_shown: (Option<StepInfo>, Option<StepInfo>),
    waiting: bool,
    /// The running recording: its file and output (ADR-0047: it runs alongside a
    /// stream).
    recording: Option<(PathBuf, OutputId)>,
    /// Stop was asked for: the recording ended its video and stops once its tail is
    /// in (ADR-0045).
    ending: bool,
    /// The running stream: its URL, the connection last reported and its output.
    streaming: Option<(String, Link, OutputId)>,
    /// Where the stream key is kept.
    keys: Arc<dyn KeyStore>,
    /// The account of the output profile in use.
    key_account: String,
    /// Makes an engine for a restart.
    engine: Box<dyn Fn() -> Engine>,
    gpu: Arc<GpuContext>,
    /// The preview's ring size, when there is a preview.
    preview_surfaces: Option<usize>,
    /// Why the studio could not start again after a canvas change.
    dead: Option<String>,
    /// The studio is being stopped and started again (canvas change): a panic now
    /// leaves it stopped, so recovery treats it as dead (ADR-0055 decision 3).
    restarting: bool,
    /// An API request panicked in this round: the loop reconciles after it.
    caught: bool,
    /// The answer the UI waits for while a command runs, sent by
    /// [`Host::recover`] when the command panicked.
    unanswered: Option<Update>,
    #[cfg(feature = "fault-injection")]
    fault: Option<Fault>,
    /// Counts `seq` per state key and publishes events to control API clients.
    events: EventPublisher,
    /// The control API, while remote control is on.
    api: Option<Api>,
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
    /// The stream settings as the UI uses them; `None` until the UI sent them.
    stream_profile: Option<StreamProfile>,
    /// The running stream, as the control API reports it.
    stream_state: api::StreamState,
    /// When the Stats panel's counters go out next, on `clock`; `None` while no
    /// output runs.
    stats_due: Option<Timestamp>,
    /// The studio's master clock (the engine's), for the stats interval.
    clock: Arc<dyn Clock>,
}

fn run(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    rx: Receiver<Msg>,
    inbox: Weak<Sender<Msg>>,
    notify: &dyn Fn(Update),
) {
    // A control thread, not a real-time one: its panics get crash reports (ADR-0055).
    rbs_crash::report_panics_on_this_thread();
    let started = rbs_crash::contain(|| start(config, adapters, gpu, engine))
        .unwrap_or_else(|_| Err(rbs_crash::CAUGHT_TEXT.to_owned()));
    let mut host = match started {
        Ok((mut host, update)) => {
            notify(update);
            host.inbox = inbox;
            host
        }
        Err(e) => {
            notify(Update::Failed(e.clone()));
            // Keep answering until the UI goes away, so it never waits on a dead thread;
            // control API requests get the reason.
            let mut api: Option<Api> = None;
            while let Ok(m) = rx.recv() {
                match m {
                    Msg::AttachApi(a) => api = Some(*a),
                    Msg::DetachApi => api = None,
                    Msg::Ui(_) | Msg::ApiWake | Msg::PickFailed(_) => {}
                }
                if let Some(api) = &api {
                    let mut events = EventPublisher::new();
                    while api.serve_one(&mut Refuse(e.clone()), &mut events, notify) {}
                }
            }
            return;
        }
    };
    host.publish(notify);
    let mut failed: Option<String> = None;
    let mut wait = MAX_WAIT;
    let mut caught_in_row = 0;
    loop {
        let msg = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Each round is a panic boundary (ADR-0055 decision 3): a bug in one command or
        // one round fails that round, and the studio and its outputs keep running.
        match rbs_crash::contain(|| round(&mut host, &mut failed, msg, notify)) {
            Ok(next) => {
                caught_in_row = 0;
                wait = next;
                // An API request panicked: its client got an `internal` error.
                if std::mem::take(&mut host.caught) {
                    let _ = rbs_crash::contain(|| host.recover(&mut failed, 1, notify));
                }
            }
            Err(_) => {
                caught_in_row += 1;
                wait = MAX_WAIT;
                notify(Update::Error(rbs_crash::CAUGHT_TEXT.into()));
                let _ = rbs_crash::contain(|| host.recover(&mut failed, caught_in_row, notify));
            }
        }
    }
    // Closing while recording: keep the tail too, as Stop does.
    if let Some((_, id)) = host.recording.clone()
        && host.studio.end_output(id)
    {
        let started = std::time::Instant::now();
        while !host.studio.output_tail_done(id) && started.elapsed() < CLOSE_TAIL_GUARD {
            host.studio.service();
            // The control thread at shutdown, not the UI thread (ADR-0055 decision 6).
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(ENDING_WAIT);
        }
    }
    host.end_stream(None, notify);
    host.stop_recording(None, notify);
    let _ = host.studio.shutdown();
}

/// Caught panics in a row after which the studio counts as failed: its recording and
/// stream end cleanly instead of running on with a host that keeps panicking.
const CAUGHT_LIMIT: u32 = 3;

/// What the UI waits for after `c`, if `c` panics before answering.
fn answer_on_panic(c: &Command) -> Option<Update> {
    let why = || rbs_crash::CAUGHT_TEXT.to_owned();
    match c {
        Command::StartRecording(_) => Some(Update::Recording(RecordingUpdate::NotStarted(why()))),
        Command::StartStream(_) => Some(Update::Stream(StreamUpdate::NotStarted(why()))),
        _ => None,
    }
}

/// One round of the control loop: the message that woke it (`None`: the wait ended),
/// one API request, then the studio's service. Returns how long to wait for the next.
fn round(
    host: &mut Host,
    failed: &mut Option<String>,
    msg: Option<Msg>,
    notify: &dyn Fn(Update),
) -> Duration {
    match msg {
        Some(Msg::Ui(c)) => {
            if failed.is_none() {
                host.unanswered = answer_on_panic(&c);
                host.command(c, notify);
                host.unanswered = None;
            }
        }
        Some(Msg::AttachApi(api)) => {
            host.events.attach(api.endpoint.event_link());
            host.api = Some(*api);
        }
        Some(Msg::DetachApi) => {
            host.events.detach();
            host.api = None;
        }
        Some(Msg::PickFailed(why)) => notify(Update::Error(why)),
        Some(Msg::ApiWake) | None => {}
    }
    // One API request per round: a UI command that arrived meanwhile is taken by the
    // next `recv_timeout` first, so the local user always goes first (decision 5).
    let served_api = match &*failed {
        None => host.serve_api(notify),
        Some(why) => host
            .api
            .as_ref()
            .is_some_and(|api| api.serve_one(&mut Refuse(why.clone()), &mut host.events, notify)),
    };
    if failed.is_none()
        && let Some(why) = host.dead.take()
    {
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.stop_recording(Some(why.clone()), notify);
        host.end_stream(Some(why.clone()), notify);
        notify(Update::Failed(why));
    }
    if failed.is_some() {
        return if served_api {
            Duration::ZERO
        } else {
            Duration::from_secs(3600)
        };
    }
    let serviced = host.studio.service();
    let mut wait = serviced.wait.unwrap_or(MAX_WAIT).min(MAX_WAIT);
    if served_api {
        // More may wait; look again right after any UI command.
        wait = Duration::ZERO;
    }
    if let Some(why) = host.studio.failure() {
        // Clients see the recording end with the failure instead of a stale state.
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.stop_recording(Some(why.clone()), notify);
        host.end_stream(Some(why.clone()), notify);
        notify(Update::Failed(why));
        return wait;
    }
    // A replacement that waited for its predecessor opens now.
    let waiting = host.studio.waiting();
    if host.waiting && !waiting {
        host.apply(notify);
    }
    host.waiting = waiting;
    if let Some((_, id)) = &host.recording
        && let Some(why) = host.studio.output_failure(*id)
    {
        host.stop_recording(Some(why), notify);
    }
    if host.ending
        && let Some((_, id)) = &host.recording
    {
        if host.studio.output_tail_done(*id) {
            host.stop_recording(None, notify);
        } else {
            // The tail takes a few frames; look again soon.
            wait = wait.min(ENDING_WAIT);
        }
    }
    if let Some((_, _, id)) = &host.streaming {
        if let Some(why) = host.studio.output_failure(*id) {
            host.end_stream(Some(why), notify);
        } else {
            host.report_link(notify);
        }
    }
    let now = host.clock.now();
    if let Some(left) = host.report_stats(now, notify) {
        wait = wait.min(left);
    }
    host.publish(notify);
    wait
}

fn start(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
) -> Result<(Host, Update), String> {
    let keys = Arc::clone(&config.keys);
    #[cfg(feature = "fault-injection")]
    let config_fault = config.fault;
    let (mut adapters, mut notes) = adapters();
    adapters.probe();
    let gpu = gpu()?;
    notes.push(format!("gpu: {}", gpu.adapter_name()));
    let loaded = config.collection.clone();
    let mut collection = config.collection.unwrap_or_default();
    let (canvas, scene) = shown_scene(&mut collection, config.size, config.fps)?;
    // The collection's canvas, which may differ from `config.size` (only the preview's
    // aspect ratio is chosen from that).
    let (run_size, run_fps) = collection
        .canvas(canvas)
        .map_or((config.size, config.fps), |c| (c.size, c.frame_rate));
    let mut settings = StudioSettings::new(canvas);
    settings.preview = config.preview;
    let first = engine();
    let clock = first.clock();
    let studio = Studio::start(
        first,
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
        // An adapter that probes to nothing (no driver or device here) cannot encode.
        .filter(|f| {
            adapters
                .video_encoders
                .capabilities(&f.id())
                .is_some_and(|c| !c.is_empty())
        })
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
        size: run_size,
        fps: run_fps,
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
            sizes: BTreeMap::new(),
            history: History::new(),
            history_shown: (None, None),
            waiting: false,
            recording: None,
            ending: false,
            stats_due: None,
            clock,
            streaming: None,
            keys,
            key_account: config.key_account,
            engine: Box::new(engine),
            gpu,
            preview_surfaces: config.preview.map(|p| p.surfaces),
            dead: None,
            restarting: false,
            caught: false,
            unanswered: None,
            #[cfg(feature = "fault-injection")]
            fault: config_fault,
            autosave: config.autosave,
            // A collection that had to be completed is saved at the first publish.
            saved: loaded,
            events: EventPublisher::new(),
            api: None,
            profile: RecordProfile::new(),
            kinds,
            encoders,
            cannot_record,
            record_state: api::RecordState::Idle { last: None },
            stream_profile: None,
            stream_state: api::StreamState::Idle { last: None },
            inbox: Weak::new(),
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
            Command::AddSource { kind, target } if target.picker => self.pick(kind, &target),
            Command::AddSource { kind, target } => match self.add(kind, &target) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e)),
            },
            Command::DeleteSource(id) => match self.delete(id) {
                Ok(name) => {
                    self.apply(notify);
                    notify(Update::Deleted(name));
                }
                Err(e) => notify(Update::Error(e)),
            },
            Command::UndoDelete => {
                if self
                    .history
                    .next_undo()
                    .is_some_and(|s| s.kind == EditKind::Delete)
                {
                    self.step(true, notify);
                }
            }
            Command::Undo => self.step(true, notify),
            Command::Redo => self.step(false, notify),
            Command::MoveSource { id, up } => match self.move_source(id, up) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::SetSourceBox {
                seq,
                gesture,
                id,
                position,
                size,
            } => {
                let ok = match self.set_source_box(id, position, size, gesture) {
                    Ok(true) => {
                        self.apply(notify);
                        true
                    }
                    Ok(false) => true,
                    Err(e) => {
                        notify(Update::Error(e));
                        false
                    }
                };
                notify(Update::BoxPlaced { seq, ok });
            }
            Command::SetSourceCrop {
                seq,
                gesture,
                id,
                crop,
                keep_scale,
            } => {
                let [l, t, r, b] = crop;
                let edit = self
                    .item_of(id)
                    .map_err(CropRefusal::NotFound)
                    .map(|(_, item)| item.id)
                    .and_then(|item| {
                        self.set_item_crop(item, Crop::new(l, t, r, b), keep_scale, gesture)
                    });
                let ok = match edit {
                    Ok(true) => {
                        self.apply(notify);
                        true
                    }
                    Ok(false) => true,
                    Err(e) => {
                        notify(Update::Error(e.text()));
                        false
                    }
                };
                notify(Update::CropPlaced { seq, ok });
            }
            Command::SetSourceRegion { id, region } => match self.set_source_region(id, region) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text())),
            },
            Command::SetSourceStyle { gesture, id, style } => {
                let edit = self
                    .item_of(id)
                    .map(|(_, item)| item.id)
                    .and_then(|item| self.set_item_style(item, style, gesture));
                match edit {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e)),
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
                // Ends the video now; the loop stops the recording once the frames
                // still read back are in (ADR-0045).
                if let Some((_, id)) = &self.recording
                    && self.studio.end_output(*id)
                {
                    self.ending = true;
                } else {
                    self.stop_recording(None, notify);
                }
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
            Command::StartStream(r) => {
                let update = match self.start_stream(&r) {
                    Ok(update) => update,
                    Err(why) => StreamUpdate::NotStarted(why),
                };
                notify(Update::Stream(update));
                self.publish(notify);
            }
            Command::EndStream => {
                self.end_stream(None, notify);
                self.publish(notify);
            }
            Command::SetStreamProfile(profile) => {
                if self.stream_profile.as_ref() != Some(&profile) {
                    self.stream_profile = Some(profile);
                    self.settings_changed();
                }
            }
            Command::SaveStreamKey(key) => {
                let result = match &key {
                    Some(k) => self.keys.write(&self.key_account, k).map(|()| true),
                    None => self.keys.remove(&self.key_account).map(|()| false),
                };
                notify(Update::StreamKey(result.map_err(|e| {
                    format!("{} refused the stream key: {e}", self.keys.place())
                })));
            }
            Command::SetKeyAccount(account) => self.key_account = account,
            Command::RemoveStreamKey(account) => {
                if let Err(e) = self.keys.remove(&account) {
                    notify(Update::Error(format!(
                        "The deleted output profile's stream key could not be removed from {}: {e}",
                        self.keys.place()
                    )));
                }
            }
            Command::SwitchCollection {
                seq,
                collection,
                autosave,
            } => {
                let result = self.switch_collection(*collection, autosave, notify);
                notify(Update::CollectionSwitched { seq, result });
            }
        }
    }

    /// What a kind can capture, in OS order, then one entry per OS picker.
    fn targets(&self, kind: SourceKind) -> Vec<Target> {
        let mut list: Vec<Target> = self
            .adapters
            .sources
            .targets(&kind.type_id())
            .unwrap_or_default()
            .into_iter()
            .map(target_of)
            .collect();
        let pickers = self
            .adapters
            .sources
            .pickers(&kind.type_id())
            .unwrap_or_default();
        list.extend(pickers.iter().map(|p| Target {
            name: picker_name(kind).to_owned(),
            adapter: p.adapter().as_str().to_owned(),
            target: String::new(),
            is_default: false,
            size: None,
            picker: true,
        }));
        list
    }

    /// Shows the OS picker of `entry.adapter` on a thread of its own (it blocks while
    /// the dialog is up, and this thread keeps the studio and any recording running),
    /// then adds the chosen target like a listed one. Closing the dialog adds nothing.
    fn pick(&self, kind: SourceKind, entry: &Target) {
        let Some(picker) = self
            .adapters
            .sources
            .pickers(&kind.type_id())
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.adapter().as_str() == entry.adapter)
        else {
            self.post(Msg::PickFailed(format!(
                "{} is not available any more.",
                entry.name
            )));
            return;
        };
        let inbox = Weak::clone(&self.inbox);
        let spawned = std::thread::Builder::new()
            .name("rbs-picker".into())
            .spawn(move || {
                let msg = match picker.pick() {
                    Ok(t) => Msg::Ui(Command::AddSource {
                        kind,
                        target: target_of(t),
                    }),
                    Err(SourceError::Capture(CaptureError::Cancelled)) => return,
                    Err(e) => {
                        Msg::PickFailed(format!("The {} could not be chosen: {e}", kind_noun(kind)))
                    }
                };
                if let Some(tx) = inbox.upgrade() {
                    let _ = tx.send(msg);
                }
            });
        if let Err(e) = spawned {
            self.post(Msg::PickFailed(format!("The picker could not start: {e}")));
        }
    }

    fn post(&self, m: Msg) {
        if let Some(tx) = self.inbox.upgrade() {
            let _ = tx.send(m);
        }
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
        self.canvas_changed(before, notify);
        result
    }

    /// Runs `next` instead of the collection that runs now (the studio restarts);
    /// on a failure the one before runs again. Always sends [`Update::Canvas`].
    fn switch_collection(
        &mut self,
        next: SceneCollection,
        autosave: Option<CollectionSaver>,
        notify: &dyn Fn(Update),
    ) -> Result<(), String> {
        if self.recording.is_some() {
            let why = "Stop the recording before switching scene collections.".to_owned();
            self.canvas_changed(self.api_canvas(), notify);
            return Err(why);
        }
        if self.streaming.is_some() {
            let why = "End the stream before switching scene collections.".to_owned();
            self.canvas_changed(self.api_canvas(), notify);
            return Err(why);
        }
        let before = self.api_canvas();
        let loaded = next.clone();
        let mut next = next;
        let (size, fps) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(1920, 1080), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
        let (canvas, scene) = match shown_scene(&mut next, size, fps) {
            Ok(shown) => shown,
            Err(why) => {
                self.canvas_changed(before, notify);
                return Err(format!("The scene collection cannot be used: {why}"));
            }
        };
        // Whatever the old collection's last change was, it is saved before this one.
        self.publish_save();
        let old = (
            std::mem::replace(&mut self.collection, next),
            std::mem::replace(&mut self.canvas, canvas),
            std::mem::replace(&mut self.scene, scene),
        );
        let _ = self.studio.shutdown();
        let result = match self.restart() {
            Ok(()) => {
                self.autosave = autosave;
                self.saved = Some(loaded);
                self.history.clear();
                Ok(())
            }
            Err(why) => {
                (self.collection, self.canvas, self.scene) = old;
                if let Err(again) = self.restart() {
                    self.dead = Some(again);
                }
                Err(format!("The scene collection cannot run: {why}"))
            }
        };
        self.canvas_changed(before, notify);
        result
    }

    /// After a restart: tells the UI the canvas (connect to the preview again), shows
    /// the rows anew and publishes a canvas change to control API clients.
    fn canvas_changed(&mut self, before: api::Canvas, notify: &dyn Fn(Update)) {
        let (size, fps) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(0, 0), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
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
    }

    /// Hands the collection to autosave when it changed since it was last handed.
    fn publish_save(&mut self) {
        if let Some(save) = &self.autosave
            && self.saved.as_ref() != Some(&self.collection)
        {
            save.save(&self.collection);
            self.saved = Some(self.collection.clone());
        }
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
            stream: self.stream_profile.as_ref().map(StreamProfile::to_api),
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

    fn set_stream_state(&mut self, state: api::StreamState) {
        if state == self.stream_state {
            return;
        }
        self.stream_state = state.clone();
        self.events
            .changed(StateKey::topic(Topic::StreamState), || {
                EventPayload::StreamState(state)
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
        let served = api.serve_one(&mut ApiTarget { host: self, notify }, &mut events, notify);
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
        if self.streaming.is_some() {
            return Err("End the stream before changing the canvas.".into());
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
        self.restarting = true;
        let _ = self.studio.shutdown();
        let restarted = self.restart();
        self.restarting = false;
        match restarted {
            Ok(()) => {
                // Old layouts would come back with an undo. (When the restart fails
                // the collection goes back as it was, and so the history stays.)
                self.history.clear();
                Ok(())
            }
            Err(why) => {
                self.collection = before;
                self.restarting = true;
                let again = self.restart();
                self.restarting = false;
                match again {
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

    /// Brings the host's bookkeeping back in line with the studio after a caught panic
    /// (ADR-0055 decision 3), `in_row` being how many rounds in a row panicked:
    /// - the command that panicked gets the answer the UI waits for, if any;
    /// - a panic during a canvas restart leaves the studio stopped: it is dead;
    /// - an output the studio runs but the host does not track (a start that panicked
    ///   half-way) is stopped, so no file grows that Stop cannot end;
    /// - a tracked recording or stream that no longer runs (a stop that panicked
    ///   half-way) is ended again;
    /// - after [`CAUGHT_LIMIT`] panics in a row the studio counts as failed, and its
    ///   outputs end, each step under its own boundary.
    ///
    /// Every step runs under its own boundary, so one that panics again does not skip
    /// the others.
    fn recover(&mut self, failed: &mut Option<String>, in_row: u32, notify: &dyn Fn(Update)) {
        const WHY: &str = "An internal error ended it (a report was saved).";
        if let Some(answer) = self.unanswered.take() {
            notify(answer);
        }
        if std::mem::take(&mut self.restarting) {
            self.dead.get_or_insert_with(|| {
                "An internal error stopped the studio while it restarted.".into()
            });
        }
        let running = self.studio.outputs();
        let recording = self.recording.as_ref().map(|(_, id)| *id);
        let streaming = self.streaming.as_ref().map(|(_, _, id)| *id);
        let mut stopped = 0;
        for id in &running {
            if Some(*id) != recording && Some(*id) != streaming {
                let _ = rbs_crash::contain(|| self.studio.stop_output(*id));
                stopped += 1;
            }
        }
        #[cfg(feature = "fault-injection")]
        notify(Update::Recovered { stopped });
        let _ = stopped;
        if recording.is_some_and(|id| !running.contains(&id)) {
            let _ = rbs_crash::contain(|| self.stop_recording(Some(WHY.into()), notify));
        }
        if streaming.is_some_and(|id| !running.contains(&id)) {
            let _ = rbs_crash::contain(|| self.end_stream(Some(WHY.into()), notify));
        }
        if in_row >= CAUGHT_LIMIT && failed.is_none() {
            let why = String::from(
                "RBS stopped the studio after repeated internal errors (reports were saved).",
            );
            *failed = Some(why.clone());
            let _ = rbs_crash::contain(|| self.stop_recording(Some(why.clone()), notify));
            let _ = rbs_crash::contain(|| self.end_stream(Some(why.clone()), notify));
            for id in self.studio.outputs() {
                let _ = rbs_crash::contain(|| self.studio.stop_output(id));
            }
            self.recording = None;
            self.streaming = None;
            notify(Update::Failed(why));
        }
        let _ = rbs_crash::contain(|| self.publish(notify));
    }

    /// Starts a new studio for the collection as it is.
    fn restart(&mut self) -> Result<(), String> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or("the canvas is gone")?;
        let mut settings = StudioSettings::new(self.canvas);
        settings.preview = self.preview_surfaces.map(|n| preview_desc(canvas.size, n));
        let engine = (self.engine)();
        let clock = engine.clock();
        let studio = Studio::start(
            engine,
            Arc::clone(&self.gpu),
            &self.collection,
            &self.adapters.sources,
            &settings,
        )
        .map_err(|e| e.to_string())?;
        self.studio = studio;
        self.clock = clock;
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
        let started = self.start_output(&out, r.encoder.as_deref());
        inject!(self, Fault::AfterOutputStart);
        let started = started.map(|(id, video_encoder, audio_encoder)| {
            (
                id,
                RecordingUpdate::Started {
                    path: r.path.clone(),
                    video_encoder,
                    audio_encoder,
                },
            )
        });
        if let Ok((
            id,
            RecordingUpdate::Started {
                video_encoder,
                audio_encoder,
                ..
            },
        )) = &started
        {
            self.recording = Some((r.path.clone(), *id));
            self.set_record_state(api::RecordState::Recording {
                profile,
                video_encoder: video_encoder.clone(),
                audio_encoder: audio_encoder.clone(),
            });
        } else if std::fs::metadata(&r.path).is_ok_and(|m| m.len() == 0) {
            // The output may have made the file before a later node failed.
            let _ = std::fs::remove_file(&r.path);
        }
        started.map(|(_, update)| update)
    }

    /// Stops the recording, if one runs, and says so.
    fn stop_recording(&mut self, failure: Option<String>, notify: &dyn Fn(Update)) {
        self.ending = false;
        // Tracked until the end: when a stop panics half-way, `Host::recover` finds the
        // recording still tracked and ends it again (ADR-0055 decision 3).
        let Some((path, id)) = self.recording.clone() else {
            return;
        };
        let report = self.studio.stop_output(id);
        inject!(self, Fault::InRecordingStop);
        let (frames, dropped, flush_lost, failed) = report.map_or((0, 0, 0, None), |r| {
            (
                r.progress.frames_encoded,
                r.progress.export_drops + r.progress.encoder_skipped + r.progress.encoder_drops,
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
        self.recording = None;
        notify(Update::Recording(RecordingUpdate::Stopped {
            path,
            failure,
            frames,
            dropped,
            flush_lost,
        }));
    }

    /// Goes live: reads the key (RTMP: required) from the key store, then attaches the
    /// stream's encoders and output to the running studio. The connection is made in
    /// the background; [`Host::report_link`] tells the UI how it goes.
    fn start_stream(&mut self, r: &StreamRequest) -> Result<StreamUpdate, String> {
        if self.streaming.is_some() {
            return Err("A stream is already running.".into());
        }
        if self.cannot_record.is_some() {
            return Err(format!(
                "Streaming is not available on {} yet: this system has no video encoder RBS can use.",
                os_name()
            ));
        }
        let key = self.keys.read(&self.key_account).map_err(|e| {
            format!(
                "The stream key could not be read from {}: {e}",
                self.keys.place()
            )
        })?;
        if r.needs_key && key.is_none() {
            return Ok(StreamUpdate::NoKey);
        }
        let mut out = match r.input {
            TransportInput::Container(c) => OutputSettings::new(r.url.clone(), c),
            _ => OutputSettings::packets(r.url.clone()),
        };
        // Never in a crash report (ADR-0055 decision 4).
        if let Some(key) = &key {
            rbs_crash::add_secret(key.expose());
        }
        rbs_crash::add_secret(&r.url);
        out.secret = key;
        out.reconnect = r.reconnect;
        out.transport_options = r.transport_options.clone();
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            a.bitrate_bps = r.audio_bitrate_bps;
            a
        });
        let (id, video_encoder, audio_encoder) = self.start_output(&out, r.encoder.as_deref())?;
        self.streaming = Some((r.url.clone(), Link::default(), id));
        let protocol = match parse_server(&r.url).map(|s| s.protocol) {
            Ok(Protocol::Srt) => api::StreamProtocol::Srt,
            // Only RTMP and SRT servers pass the settings' check.
            _ => api::StreamProtocol::Rtmp,
        };
        self.set_stream_state(api::StreamState::Live {
            protocol,
            connected: false,
            connects: 0,
            reconnect_attempts: 0,
            video_encoder: video_encoder.clone(),
            audio_encoder: audio_encoder.clone(),
        });
        Ok(StreamUpdate::Started {
            url: r.url.clone(),
            video_encoder,
            audio_encoder,
        })
    }

    /// Attaches `out` to the studio with the picked encoder only, or the best one (a
    /// running one when the settings match, ADR-0047); returns the output and the
    /// encoders' names.
    fn start_output(
        &mut self,
        out: &OutputSettings,
        encoder: Option<&str>,
    ) -> Result<(OutputId, String, Option<String>), String> {
        // A picked encoder is the only one tried: if it cannot open, the output fails
        // rather than quietly using another one.
        let all = match encoder {
            None => None,
            Some(id) => {
                let only = only_encoder(&self.adapters.video_encoders, id)?;
                Some(std::mem::replace(&mut self.adapters.video_encoders, only))
            }
        };
        let started = self
            .studio
            .start_output(&self.adapters, out)
            .map_err(|e| e.to_string())
            .and_then(|id| {
                let report = self.studio.output(id).ok_or("the output did not start")?;
                Ok((
                    id,
                    report.video_encoder.name.clone(),
                    report.audio_encoder.as_ref().map(|c| c.name.clone()),
                ))
            });
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        started
    }

    /// Sends the running outputs' counters to the Stats panel once every
    /// [`STATS_INTERVAL`], the first at once, and empty counters once when the last
    /// output stopped; nothing while no output runs (no timer: ADR-0013). Returns the
    /// time until the next report.
    fn report_stats(&mut self, now: Timestamp, notify: &dyn Fn(Update)) -> Option<Duration> {
        let recording = self.recording.as_ref().map(|(_, id)| *id);
        let stream = self.streaming.as_ref().map(|(_, _, id)| *id);
        if recording.is_none() && stream.is_none() {
            // The last output stopped: the panel empties once.
            if self.stats_due.take().is_some() {
                notify(Update::Stats(Stats::default()));
            }
            return None;
        }
        let due = *self.stats_due.get_or_insert(now);
        if now < due {
            return Some(due.saturating_since(now));
        }
        self.stats_due = Some(now + STATS_INTERVAL);
        let counters = |id: Option<OutputId>| id.and_then(|id| self.studio.output_counters(id));
        notify(Update::Stats(Stats::of(
            counters(recording).as_ref(),
            counters(stream).as_ref(),
        )));
        Some(STATS_INTERVAL)
    }

    /// Tells the UI when the stream's connection changed.
    fn report_link(&mut self, notify: &dyn Fn(Update)) {
        let Some((_, last, id)) = &mut self.streaming else {
            return;
        };
        // Only what the UI shows: a change of transport drops alone sends nothing.
        let shown = |l: &Link| (l.connected, l.connects, l.reconnect_attempts);
        if let Some(link) = self.studio.link(*id)
            && shown(&link) != shown(last)
        {
            *last = link;
            notify(Update::Stream(StreamUpdate::Link(link)));
            if let api::StreamState::Live {
                protocol,
                video_encoder,
                audio_encoder,
                ..
            } = &self.stream_state
            {
                let state = api::StreamState::Live {
                    protocol: *protocol,
                    connected: link.connected,
                    connects: link.connects,
                    reconnect_attempts: link.reconnect_attempts,
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                };
                self.set_stream_state(state);
            }
        }
    }

    /// Ends the stream, if one runs, and says so.
    fn end_stream(&mut self, failure: Option<String>, notify: &dyn Fn(Update)) {
        // Tracked until the end, as in `stop_recording`.
        let Some((_, _, id)) = self.streaming.clone() else {
            return;
        };
        let report = self.studio.stop_output(id);
        let (frames, dropped, connects, failed) = report.map_or((0, 0, 0, None), |r| {
            let p = &r.progress;
            (
                p.frames_encoded,
                p.export_drops + p.encoder_skipped + p.encoder_drops + p.transport_drops,
                p.connects,
                r.failure,
            )
        });
        let failure = failure.or(failed);
        self.streaming = None;
        self.set_stream_state(api::StreamState::Idle {
            last: Some(api::StreamSummary {
                failure: failure.clone(),
                frames,
                dropped,
                connects,
            }),
        });
        notify(Update::Stream(StreamUpdate::Ended {
            failure,
            frames,
            dropped,
            connects,
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
        let (id, source) = self
            .collection
            .add_source(name, kind.type_id(), settings)
            .map_err(|e| format!("{e:?}"))?;
        match self
            .collection
            .add_item(self.scene, ItemContent::Source(id), layout, None)
        {
            Ok((_, item)) => {
                self.history
                    .record(EditKind::Add, name, 0, vec![source, item]);
                Ok(id)
            }
            Err(e) => {
                let _ = self.collection.undo(source);
                Err(format!("{e:?}"))
            }
        }
    }

    /// Deletes a source from every scene; returns its name.
    fn delete(&mut self, id: SourceId) -> Result<String, String> {
        let name = self
            .collection
            .source(id)
            .map(|s| s.name.clone())
            .ok_or("This source is not in the scene any more.")?;
        let undo = self
            .collection
            .delete_source(id)
            .map_err(|e| format!("{e:?}"))?;
        self.history.record(EditKind::Delete, &name, 0, vec![undo]);
        Ok(name)
    }

    /// Undoes (`back`) or redoes one step of the history and runs the result. A
    /// refused step is said in [`Update::Error`]; with nothing to do, nothing happens.
    fn step(&mut self, back: bool, notify: &dyn Fn(Update)) {
        // Nothing to undo is not a problem: the buttons are disabled then, and a key
        // press says nothing.
        match self.undo_or_redo(back) {
            Ok(()) => self.apply(notify),
            Err((true, _)) => {}
            Err((false, why)) => {
                notify(Update::Error(why));
                // A refused step left the scene as it was, but a lost one may not
                // have: run what is there.
                self.apply(notify);
            }
        }
    }

    /// One step of the history; the error says whether there was nothing to do
    /// (`true`) and why it failed.
    fn undo_or_redo(&mut self, back: bool) -> Result<(), (bool, String)> {
        let collection = &mut self.collection;
        let apply = |r: Undo| collection.undo(r);
        let done = if back {
            self.history.undo(apply)
        } else {
            self.history.redo(apply)
        };
        match done {
            Ok(_) => Ok(()),
            Err(HistoryError::Empty) => Err((
                true,
                if back {
                    "nothing to undo"
                } else {
                    "nothing to redo"
                }
                .to_owned(),
            )),
            Err(HistoryError::Refused(e)) => Err((
                false,
                format!("The scene changed in a way this step cannot be applied to ({e:?})."),
            )),
            Err(HistoryError::Lost(e)) => Err((
                false,
                format!(
                    "The scene changed in a way this step cannot be applied to ({e:?}), and part of it may have been applied; the undo history was cleared."
                ),
            )),
        }
    }

    /// The program scene's item that places `id`, and its index (0 is the bottom).
    fn item_of(&self, id: SourceId) -> Result<(usize, &SceneItem), String> {
        let gone = || "This source is not in the scene any more.".to_owned();
        let scene = self.collection.scene(self.scene).ok_or_else(gone)?;
        scene
            .items
            .iter()
            .enumerate()
            .find(|(_, i)| i.content == ItemContent::Source(id))
            .ok_or_else(gone)
    }

    /// True when the item shows a picture (a source that is not sound only).
    fn has_picture(&self, item: &SceneItem) -> bool {
        match item.content {
            ItemContent::Source(id) => self
                .collection
                .source(id)
                .and_then(|s| SourceKind::of(&s.type_id))
                .is_some_and(|k| !k.is_audio()),
            _ => true,
        }
    }

    /// Over (`up`) or under the next source with a picture; sound-only items do not
    /// count, because they draw nothing. `Ok(false)` when there is none that way.
    fn move_source(&mut self, id: SourceId, up: bool) -> Result<bool, String> {
        let (index, item) = self.item_of(id)?;
        let item = item.id;
        let Some(scene) = self.collection.scene(self.scene) else {
            return Ok(false);
        };
        let mut others = scene
            .items
            .iter()
            .enumerate()
            .filter(|(i, it)| *i != index && self.has_picture(it))
            .map(|(i, _)| i);
        let to = if up {
            others.find(|i| *i > index)
        } else {
            others.rfind(|i| *i < index)
        };
        let Some(to) = to else { return Ok(false) };
        let undo = self
            .collection
            .move_item(item, to)
            .map_err(|e| edit_error(&e))?;
        let name = self.name_of(id);
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    fn name_of(&self, id: SourceId) -> String {
        self.collection
            .source(id)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    /// A new box for a source's picture; `Ok(false)` when it is already there.
    fn set_source_box(
        &mut self,
        id: SourceId,
        position: [f32; 2],
        size: [f32; 2],
        gesture: u64,
    ) -> Result<bool, String> {
        let valid = position.iter().chain(size.iter()).all(|v| v.is_finite())
            && size.iter().all(|v| *v != 0.0);
        if !valid {
            return Err("Position and size must be numbers, and the size cannot be 0.".into());
        }
        let (_, item) = self.item_of(id)?;
        // The layout the canvas shows: its own if the item has one, else the default.
        let mut layout = *item.layout_on(self.canvas);
        let own = item
            .overrides
            .contains_key(&self.canvas)
            .then_some(self.canvas);
        if layout.position == position && layout.size == size {
            return Ok(false);
        }
        layout.position = position;
        layout.size = size;
        let item = item.id;
        let undo = self
            .collection
            .set_item_layout(item, own, layout)
            .map_err(|e| edit_error(&e))?;
        let name = self.name_of(id);
        self.history
            .record(EditKind::Place, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A new style for an item's layout on the studio's canvas (its own layout there if
    /// it has one, else the default); `Ok(false)` when it already has it.
    fn set_item_style(
        &mut self,
        item: ItemId,
        style: ItemStyle,
        gesture: u64,
    ) -> Result<bool, String> {
        if !style.is_valid() {
            return Err(STYLE_ERROR.into());
        }
        let gone = || "This source is not in the scene any more.".to_owned();
        let found = self
            .collection
            .scenes()
            .find_map(|s| s.items.iter().find(|i| i.id == item).cloned());
        let found = found.ok_or_else(gone)?;
        let mut layout = *found.layout_on(self.canvas);
        if layout.style == style {
            return Ok(false);
        }
        let own = found
            .overrides
            .contains_key(&self.canvas)
            .then_some(self.canvas);
        layout.style = style;
        let undo = self
            .collection
            .set_item_layout(item, own, layout)
            .map_err(|e| edit_error(&e))?;
        let name = match found.content {
            ItemContent::Source(id) => self.name_of(id),
            _ => String::new(),
        };
        self.history
            .record(EditKind::Style, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// An item placing a source, anywhere in the collection, with its layout on the
    /// studio's canvas and whether that is its own override there.
    fn crop_target(
        &self,
        item: ItemId,
    ) -> Result<(SourceId, Layout, Option<CanvasId>), CropRefusal> {
        let found = self
            .collection
            .scenes()
            .find_map(|s| s.items.iter().find(|i| i.id == item))
            .ok_or_else(|| CropRefusal::NotFound(format!("no item {}", item.get())))?;
        let ItemContent::Source(source) = found.content else {
            return Err(CropRefusal::Invalid(
                "crop is for source items, not nested scenes".into(),
            ));
        };
        let own = found
            .overrides
            .contains_key(&self.canvas)
            .then_some(self.canvas);
        Ok((source, *found.layout_on(self.canvas), own))
    }

    /// Writes `layout` as one undo step of `kind`; `Ok(false)` when it is unchanged.
    fn write_crop(
        &mut self,
        item: ItemId,
        source: SourceId,
        own: Option<CanvasId>,
        old: Layout,
        layout: Layout,
        gesture: u64,
    ) -> Result<bool, CropRefusal> {
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_item_layout(item, own, layout)
            .map_err(|e| CropRefusal::Failed(edit_error(&e)))?;
        let name = self.name_of(source);
        self.history
            .record(EditKind::Crop, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A new crop for an item's layout on the studio's canvas (ADR-0057); `Ok(false)`
    /// when it already has it.
    fn set_item_crop(
        &mut self,
        item: ItemId,
        crop: Crop,
        keep_scale: bool,
        gesture: u64,
    ) -> Result<bool, CropRefusal> {
        let (source, old, own) = self.crop_target(item)?;
        let size = self.sizes.get(&source).copied();
        let layout = if keep_scale {
            let size = size.ok_or(CropRefusal::NoPicture)?;
            if crop == old.crop {
                return Ok(false);
            }
            old.recrop(size, crop)
                .ok_or_else(|| CropRefusal::Invalid(NO_AREA.into()))?
        } else {
            if size.is_some_and(|(w, h)| !crop.fits(w, h)) {
                return Err(CropRefusal::Invalid(NO_AREA.into()));
            }
            old.with_crop(crop)
        };
        self.write_crop(item, source, own, old, layout, gesture)
    }

    /// Shows `region` of a source's picture (Properties' Region menu).
    fn set_source_region(&mut self, id: SourceId, region: CropRegion) -> Result<bool, CropRefusal> {
        let item = self.item_of(id).map_err(CropRefusal::NotFound)?.1.id;
        let (source, old, own) = self.crop_target(item)?;
        let size = self
            .sizes
            .get(&source)
            .copied()
            .ok_or(CropRefusal::NoPicture)?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .map(|c| (c.size.width, c.size.height))
            .ok_or_else(|| CropRefusal::Failed("the canvas is gone".into()))?;
        let crop = region.crop(size, canvas);
        let layout = {
            if !crop.fits(size.0, size.1) {
                return Err(CropRefusal::Invalid(NO_AREA.into()));
            }
            let w = size.0 - crop.left - crop.right;
            let h = size.1 - crop.top - crop.bottom;
            let fit = Layout::fit(canvas.0, canvas.1, w, h);
            let mut layout = old.with_crop(crop);
            layout.position = fit.position;
            // A flipped picture stays flipped.
            layout.size = [
                fit.size[0].copysign(old.size[0]),
                fit.size[1].copysign(old.size[1]),
            ];
            if layout.size[0] < 0.0 {
                layout.position[0] -= layout.size[0];
            }
            if layout.size[1] < 0.0 {
                layout.position[1] -= layout.size[1];
            }
            layout
        };
        self.write_crop(item, source, own, old, layout, 0)
    }

    /// Remembers the picture size of every running video source (ADR-0057 decision 2)
    /// and forgets sources that are gone.
    fn refresh_sizes(&mut self) {
        let collection = &self.collection;
        self.sizes.retain(|id, _| collection.source(*id).is_some());
        for source in self.collection.sources() {
            if let Some(size) = self.studio.picture_size(source.id) {
                self.sizes.insert(source.id, size);
            }
        }
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

    /// Sends the rows and the undo history when they changed, and saves the collection
    /// when it changed (every edit, undo, canvas change or control API change passes
    /// here; the writer thread keeps only the latest snapshot, so a drag costs one
    /// write behind it, never a wait here).
    fn publish(&mut self, notify: &dyn Fn(Update)) {
        self.publish_save();
        self.refresh_sizes();
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
        // After the rows, so the UI sees the edit before what undoes it.
        let shown = (
            self.history.next_undo().cloned(),
            self.history.next_redo().cloned(),
        );
        if shown != self.history_shown {
            self.history_shown = shown.clone();
            notify(Update::History {
                undo: shown.0,
                redo: shown.1,
            });
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
                ItemContent::Source(id) => self.collection.source(id).map(|s| (s, item)),
                _ => None,
            })
            .map(|(s, item)| {
                let kind = SourceKind::of(&s.type_id);
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
                    item: item.id,
                    name: s.name.clone(),
                    kind,
                    layout: kind
                        .is_some_and(|k| !k.is_audio())
                        .then(|| *item.layout_on(self.canvas)),
                    meter: matches!(status, SourceStatus::Running)
                        .then(|| self.studio.meter(s.id).cloned())
                        .flatten(),
                    picture: self.sizes.get(&s.id).copied(),
                    status,
                }
            })
            .collect()
    }
}

/// Why a style is refused, with what to do.
const STYLE_ERROR: &str = "Corner radius and border width must be numbers, not below 0, and a border must be wider than 0.";

/// A refused scene edit in words, with what to do.
fn edit_error(e: &SceneError) -> String {
    match e {
        SceneError::StyleOnScene(_) => {
            "Shape and border are for sources, not for scenes placed in a scene.".to_owned()
        }
        SceneError::InvalidLayout => {
            "Position and size must be numbers, and the size cannot be 0.".to_owned()
        }
        SceneError::UnknownItem(_) | SceneError::UnknownScene(_) | SceneError::UnknownSource(_) => {
            "This source is not in the scene any more.".to_owned()
        }
        other => format!("The scene could not be changed ({other:?}). Try again."),
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
        // A panic fails this request only (ADR-0055 decision 3); the control loop
        // reconciles the host after the round.
        let result = rbs_crash::contain(|| self.run(command));
        std::mem::swap(&mut self.host.events, events);
        result.unwrap_or_else(|_| {
            self.host.caught = true;
            Err(ApiError::new(ErrorKind::Internal, rbs_crash::CAUGHT_TEXT))
        })
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
                host.delete(id).map_err(failed)?;
                host.apply(notify);
                notify(Update::Deleted(name));
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetCrop(p) => {
                let [l, t, r, b] = p.crop;
                let item = ItemId::from_raw(p.item);
                match host.set_item_crop(item, Crop::new(l, t, r, b), p.keep_scale, 0) {
                    Ok(changed) => {
                        if changed {
                            host.apply(notify);
                        }
                        Ok(Response::Done(Empty {}))
                    }
                    Err(e) => Err(ApiError::new(
                        match e {
                            CropRefusal::NotFound(_) => ErrorKind::NotFound,
                            CropRefusal::Invalid(_) => ErrorKind::InvalidParams,
                            CropRefusal::NoPicture | CropRefusal::Failed(_) => ErrorKind::Failed,
                        },
                        e.text(),
                    )),
                }
            }
            C::ItemSetStyle(p) => {
                let style = scene_style(&p.style)
                    .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
                let item = ItemId::from_raw(p.item);
                let found = host
                    .collection
                    .scenes()
                    .flat_map(|s| s.items.iter())
                    .find(|i| i.id == item)
                    .map(|i| i.content);
                match found {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "shape and border are for source items, not nested scenes",
                        ));
                    }
                    Some(_) => {}
                }
                if host.set_item_style(item, style, 0).map_err(failed)? {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::EditUndo | C::EditRedo => {
                let done = host.undo_or_redo(matches!(command, C::EditUndo));
                host.apply(notify);
                done.map_err(|(empty, why)| {
                    let kind = if empty {
                        ErrorKind::Conflict
                    } else {
                        ErrorKind::UndoConflict
                    };
                    ApiError::new(kind, why)
                })?;
                Ok(Response::Done(Empty {}))
            }
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
            C::StreamStart => self.stream(),
            C::StreamStop => {
                if host.streaming.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing streams"));
                }
                host.end_stream(None, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::StreamStatus => Ok(Response::StreamStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::StreamState]),
                data: host.stream_state.clone(),
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
                let stream = match &patch.stream {
                    Some(patch) => {
                        let Some(current) = &host.stream_profile else {
                            return Err(failed("the studio is still starting; try again"));
                        };
                        let stream = current
                            .with_api(patch)
                            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
                        if host.streaming.is_some() && stream != *current {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing its settings",
                            ));
                        }
                        Some(stream)
                    }
                    None => None,
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
                        if host.streaming.is_some() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing the canvas",
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
                if let Some(stream) = stream
                    && host.stream_profile.as_ref() != Some(&stream)
                {
                    host.stream_profile = Some(stream.clone());
                    notify(Update::StreamProfile(stream));
                    host.settings_changed();
                }
                Ok(Response::Settings(api::Snapshot {
                    seq: host.events.seq_map([Topic::SettingsChanged]),
                    data: host.api_settings(),
                }))
            }
            // Answered by the endpoint; never reaches a target.
            // `tokens.*` are answered before (`WithTokens`) when the API was attached
            // with the settings.
            C::EventsSubscribe(_) | C::TokensList | C::TokensCreate(_) | C::TokensRevoke(_) => {
                Err(rbs_api::control::unsupported(&command))
            }
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

impl ApiTarget<'_> {
    /// Goes live with the saved stream settings and the key in the key store.
    fn stream(&mut self) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if host.streaming.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a stream is already running",
            ));
        }
        let Some(profile) = &host.stream_profile else {
            return Err(failed("the studio is still starting; try again"));
        };
        if profile.server.is_empty() {
            return Err(failed("no server is saved: enter it in Settings, Stream"));
        }
        // The saved server is a setting (`admin`): a `control` caller never gets it back,
        // also not inside an error text.
        let url = profile.server.clone();
        let hide = |why: String| failed(why.replace(&url, "the saved server"));
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = profile
            .encoding
            .encoder
            .clone()
            .filter(|id| host.encoders.iter().any(|e| e.id == *id));
        let request = StreamRequest::from_profile(profile, encoder).map_err(|_| {
            failed("the saved stream settings cannot be used: see Settings, Stream")
        })?;
        let result = host.start_stream(&request);
        host.publish(notify);
        match result {
            Ok(StreamUpdate::Started {
                url,
                video_encoder,
                audio_encoder,
            }) => {
                let started = StreamStarted {
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                };
                // The UI shows the stream as if its own button started it.
                notify(Update::Stream(StreamUpdate::Started {
                    url,
                    video_encoder,
                    audio_encoder,
                }));
                Ok(Response::StreamStarted(started))
            }
            Ok(StreamUpdate::NoKey) => Err(failed(
                "no stream key is saved: enter it in Settings, Stream",
            )),
            Ok(_) => Err(failed("the stream did not start")),
            Err(why) => Err(hide(why)),
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

/// The control API's form of a style (ADR-0053).
fn api_style(s: &ItemStyle) -> api::ItemStyle {
    let (shape, radius) = match s.shape {
        Shape::Rectangle { radius } => (api::ShapeKind::Rectangle, radius),
        Shape::Circle => (api::ShapeKind::Circle, 0.0),
        Shape::Ellipse => (api::ShapeKind::Ellipse, 0.0),
        _ => (api::ShapeKind::Rectangle, 0.0),
    };
    api::ItemStyle {
        shape,
        radius,
        border: s.border.map(|b| api::StyleBorder {
            width: b.width,
            color: api::format_color(b.color),
        }),
        shadow: s.shadow.map(|sh| api::StyleShadow {
            offset: sh.offset,
            blur: sh.blur,
            color: api::format_color(sh.color),
        }),
    }
}

/// The scene's form of an API style; the error says what is wrong.
fn scene_style(s: &api::ItemStyle) -> Result<ItemStyle, String> {
    let shape = match s.shape {
        api::ShapeKind::Rectangle => Shape::Rectangle { radius: s.radius },
        _ if s.radius != 0.0 => return Err("radius is only for shape \"rectangle\"".into()),
        api::ShapeKind::Circle => Shape::Circle,
        api::ShapeKind::Ellipse => Shape::Ellipse,
    };
    let color = |c: &str| {
        api::parse_color(c).ok_or_else(|| format!("colour {c:?} is not #rrggbb or #rrggbbaa"))
    };
    let border = match &s.border {
        Some(b) => Some(Border::new(b.width, color(&b.color)?)),
        None => None,
    };
    let shadow = match &s.shadow {
        Some(sh) => Some(Shadow::new(sh.offset, sh.blur, color(&sh.color)?)),
        None => None,
    };
    let style = ItemStyle::new(shape)
        .with_border(border)
        .with_shadow(shadow);
    if style.is_valid() {
        Ok(style)
    } else {
        Err(STYLE_ERROR.into())
    }
}

fn api_source(row: &SourceRow) -> api::SourceInfo {
    api::SourceInfo {
        item: Some(row.item.get()),
        layout: row.layout.map(|l| api::ItemLayout {
            position: l.position,
            size: l.size,
            rotation: l.rotation,
            crop: [l.crop.left, l.crop.top, l.crop.right, l.crop.bottom],
            opacity: l.opacity,
            style: api_style(&l.style),
        }),
        picture_size: row.picture.map(|(w, h)| [w, h]),
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
        out.style = scale_style(&layout.style, nw / w.abs());
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
    out.style = scale_style(&layout.style, s);
    out
}

/// `style` with its lengths multiplied by `factor`, the factor its box was scaled by
/// (ADR-0053 decision 12). A factor that is not finite and positive leaves it as it is.
fn scale_style(style: &ItemStyle, factor: f32) -> ItemStyle {
    if !(factor.is_finite() && factor > 0.0) {
        return *style;
    }
    let mut out = *style;
    if let Shape::Rectangle { radius } = &mut out.shape {
        *radius *= factor;
    }
    if let Some(b) = &mut out.border {
        b.width *= factor;
    }
    if let Some(sh) = &mut out.shadow {
        sh.offset = sh.offset.map(|v| v * factor);
        sh.blur *= factor;
    }
    out
}

/// The canvas the studio runs (the collection's first, or a new one of `size` at
/// `fps`) and the scene it shows (that canvas's program, else the first scene it may
/// show, else a new shared scene), set on air.
fn shown_scene(
    collection: &mut SceneCollection,
    size: Size,
    fps: FrameRate,
) -> Result<(CanvasId, SceneId), String> {
    let first = collection.canvases().next().map(|c| c.id);
    let canvas = match first {
        Some(c) => c,
        None => {
            collection
                .add_canvas("Main", size, fps)
                .map_err(|e| format!("canvas: {e:?}"))?
                .0
        }
    };
    let program = collection.canvas(canvas).and_then(|c| c.program);
    let allowed = collection.scenes_for(canvas).next().map(|s| s.id);
    let scene = match program.or(allowed) {
        Some(s) => s,
        None => {
            collection
                .add_scene("Scene", SceneScope::Shared)
                .map_err(|e| format!("scene: {e:?}"))?
                .0
        }
    };
    if program != Some(scene) {
        collection
            .set_program(canvas, Some(scene))
            .map_err(|e| format!("program: {e:?}"))?;
    }
    Ok((canvas, scene))
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
    fn style_lengths_scale_with_the_box_in_both_branches() {
        let style = ItemStyle::new(Shape::Rectangle { radius: 20.0 })
            .with_border(Some(Border::new(4.0, [255; 4])))
            .with_shadow(Some(Shadow::new([2.0, 3.0], 10.0, [0, 0, 0, 255])));
        // Fitted: 1920 wide to 1280 wide.
        let full = Layout::new([0.0, 0.0], [1920.0, 1080.0]).with_style(style);
        let got = rescale_layout(&full, (1920, 1080), (1280, 720)).style;
        let f = 1280.0 / 1920.0;
        assert_eq!(got.shape, Shape::Rectangle { radius: 20.0 * f });
        assert_eq!(got.border.map(|b| b.width), Some(4.0 * f));
        assert_eq!(
            got.shadow.map(|sh| (sh.offset, sh.blur)),
            Some(([2.0 * f, 3.0 * f], 10.0 * f))
        );
        // In a corner: by the box's own factor.
        let cam = camera_layout(1920, 1080).with_style(style);
        let got = rescale_layout(&cam, (1920, 1080), (3440, 1440));
        let s = got.size[0] / cam.size[0];
        assert!((s - 1440.0 / 1080.0).abs() < 1e-4);
        assert_eq!(got.style.border.map(|b| b.width), Some(4.0 * s));
        // Circles have no length to scale.
        let circle =
            Layout::new([0.0, 0.0], [1920.0, 1080.0]).with_style(ItemStyle::new(Shape::Circle));
        let got = rescale_layout(&circle, (1920, 1080), (1280, 720));
        assert_eq!(got.style, ItemStyle::new(Shape::Circle));
    }

    #[test]
    fn previews_keep_the_canvas_aspect() {
        let d = preview_desc(Size::new(3440, 1440), 3);
        assert_eq!((d.width, d.height, d.surfaces), (1280, 534, 3));
    }
}
