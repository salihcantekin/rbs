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

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_api::request::{
    AlignAxis, Axis, BoxPlaced, ChromeSpec, Empty, FitKind, GlobalSourceList, ItemAdded,
    ItemRemoved, LookInfo, LookList, ProfileList, RecordStarted, SceneAdded, SceneSourceList,
    SourceAdded, SourceChromeInfo, SourceRegionInfo, SourceSettingsInfo, StreamStarted, TargetList,
};
use rbs_api::types as api;
use rbs_api::{
    ApiError, ControlEndpoint, ControlTarget, ErrorKind, EventPayload, EventPublisher, Response,
    StateKey, Topic,
};
use rbs_audio::LevelMeter;
use rbs_capture::CaptureError;
use rbs_codec::caps as codec_caps;
use rbs_core::capability::{CapabilitySet, CapabilityValue, Requirement};
use rbs_core::fault::SourceFault;
use rbs_core::media::{CodecId, ContainerId, Size};
use rbs_core::pause::HoldPicture;
use rbs_core::registry::{AdapterFactory, Registry};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_engine::Engine;
use rbs_gpu::GpuContext;
use rbs_gpu::share::{MAX_PREVIEW_SURFACES, PreviewDesc, PreviewEndpoint, ring_needed};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AudioSettings, ExtraCanvas, IsoAudioStart, Link, OutputId, OutputSettings, PictureId, Progress,
    RecordAdapters, RecordError, ScrollControl, ScrollRefused, Studio, StudioSettings, TextDue,
    TransitionOnce, TransitionPhase, VideoSettings, VirtualCameraSettings,
};
use rbs_scene::model::{
    Border, CanvasId, Crop, DisplayChrome, FilterId, ItemContent, ItemId, ItemStyle, Layout,
    MAX_SCROLL_SPEED, MIN_SCROLL_SPEED, MasterAudio, RecordSeparately, Scene, SceneId, SceneItem,
    SceneScope, Scroll, ScrollDirection, ScrollRepeat, ScrollStart, SettingValue, Settings, Shadow,
    Shape, Source, SourceAudio, SourceId, SourceMonitor, SourceRegion, VideoAs,
};
use rbs_scene::{
    AutoLayout, CenterAxis, ChromeFill, FitMode, Removed, SceneError, SceneLevel, Undo,
    place_chrome, recrop_keeping, visible_size,
};
use rbs_scene::{COMPANION_OF_HINT, STANDBY_HINT, SceneCollection};
use rbs_sources::BrowserTree;
use rbs_sources::permissions::{PermissionInfo, PermissionKind, PermissionStatus};
use rbs_sources::types::{self, keys};
use rbs_sources::{SourceError, SourceTarget};
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};
use rbs_vcam::{InstallState, VcamStats, VirtualCameraFactory};

mod add_ons;
mod attributes;
pub use attributes::{AttributeTargetView, AttributesView};
mod calibration;
mod device_sync;
mod failures;
mod seams;
use failures::{answer_on_panic, refused};
pub use seams::{
    Applied, BrowserTrees, Chapter, ChapterWriter, EngineRuntime, EngineSlot, FreeSpaceReader,
    GPU_FALLBACK, HostBrowserTree, MainDisplayProbe, display_from_default_screen, page_reason,
};
pub mod filters;
mod interact;
pub use interact::{InteractState, MAX_INTERACT};
mod control_out;
mod log_summary;
mod loudness;
mod monitor;
mod network;
mod obs_import_api;
mod on_air;
pub mod protection;
pub mod replay;
mod scene_cache;
mod stinger;
pub mod sync_measure;
mod text_live;
mod text_measure;
use control_out::ControlOut;
mod api_answers;
mod api_convert;
mod control_api;
use api_answers::*;
use api_convert::*;
pub use command::*;
use control_api::*;
use recording_markers::*;
pub use rows::*;
pub use updates::*;
mod command;
mod items;
mod outputs;
mod projector;
mod recording;
mod recording_markers;
mod rows;
mod scenes;
mod source_settings;
mod sources;
mod stream;
mod text_resize;
mod updates;

use crate::api_tokens::ApiTokens;
use crate::canvas_profile::CanvasProfile;
use crate::edit_history::{EditKind, History, HistoryError, StepInfo};
use crate::monitor_settings::MonitorSettings;
use crate::recent_targets::{Recent, RecentTargets};
use crate::record_profile::{
    API_PROFILE_ID, LimitPolicy, RecordProfile, SeparateFiles, output_size_matches,
};
use crate::stream_destinations::{
    Canvas as DestinationCanvas, DestinationEncoding, Destinations, MAIN_FIRST, Resolved,
    VERTICAL_FIRST,
};
use crate::stream_key::{self, KeyStore, MemoryKeyStore};
use crate::stream_profile::{Protocol, StreamProfile, parse_server};
use crate::user_session::CollectionSaver;
use crate::vertical_outputs::VerticalFlags;

/// The longest the control thread sleeps between two rounds of the engine's control
/// loop (the studio asks for at most about 100 ms).
const MAX_WAIT: Duration = Duration::from_millis(50);
/// The longest the control thread sleeps while no output runs, ends or waits (perf
/// audit G-05). Idle with no deadline blocks on the next message instead (F-08).
/// This cap remains for a round that still has to look, and the unit test of [`round_cap`].
const IDLE_WAIT: Duration = Duration::from_millis(250);
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
    /// A picture: a file or a built-in sample (ADR-0074). Not in [`SourceKind::ALL`]:
    /// it has no target to list; the Library adds it.
    Image,
    /// A solid colour or a gradient (ADR-0074). Not in [`SourceKind::ALL`].
    Color,
    /// A local video or animated picture (ADR-0108). Not in [`SourceKind::ALL`]:
    /// added with a path like [`SourceKind::Image`].
    Media,
    /// A web page from the browser helper (ADR-0109). Not in [`SourceKind::ALL`].
    Browser,
    /// A text, drawn by the registered text renderer (ADR-0129). Not in
    /// [`SourceKind::ALL`]: it has no target; Add source adds "Text" at once.
    Text,
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
            SourceKind::Image => types::IMAGE_SOURCE,
            SourceKind::Color => types::COLOR_SOURCE,
            SourceKind::Media => types::MEDIA_SOURCE,
            SourceKind::Browser => types::BROWSER_SOURCE,
            SourceKind::Text => types::TEXT_SOURCE,
        })
    }

    /// The kind of a source type.
    pub fn of(type_id: &NodeTypeId) -> Option<SourceKind> {
        SourceKind::ALL
            .into_iter()
            .chain([
                SourceKind::Image,
                SourceKind::Color,
                SourceKind::Media,
                SourceKind::Browser,
                SourceKind::Text,
            ])
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
            SourceKind::Image => "Image",
            SourceKind::Color => "Colour",
            SourceKind::Media => "Media file",
            SourceKind::Browser => "Browser",
            SourceKind::Text => "Text",
        }
    }

    /// The `kind` parameter of `source.add` / the control API.
    pub fn api_id(self) -> &'static str {
        match self {
            SourceKind::Screen => "screen",
            SourceKind::Window => "window",
            SourceKind::Camera => "camera",
            SourceKind::AudioInput => "audio_input",
            SourceKind::SystemAudio => "system_audio",
            SourceKind::ApplicationAudio => "application_audio",
            SourceKind::Image => "image",
            SourceKind::Color => "color",
            SourceKind::Media => "media",
            SourceKind::Browser => "browser",
            SourceKind::Text => "text",
        }
    }

    /// True for kinds that only make sound.
    pub fn is_audio(self) -> bool {
        matches!(
            self,
            SourceKind::AudioInput | SourceKind::SystemAudio | SourceKind::ApplicationAudio
        )
    }

    /// True when Add source opens a file or URL dialog instead of a device list.
    pub fn picks_path(self) -> bool {
        matches!(self, SourceKind::Media | SourceKind::Image)
    }
}

/// The one listed window a window source's hints name: none while its stored target
/// is still listed, none when no or several windows have the same application
/// (case-insensitive) and title (exact).
fn match_by_hints(
    listed: Vec<Target>,
    adapter: &str,
    stored: &str,
    app: &str,
    title: &str,
) -> Option<Target> {
    if listed
        .iter()
        .any(|t| t.adapter == adapter && t.target == stored)
    {
        return None;
    }
    let app = app.to_lowercase();
    let mut same = listed.into_iter().filter(|t| {
        t.adapter == adapter
            && t.name == title
            && t.app.as_deref().is_some_and(|a| a.to_lowercase() == app)
    });
    let found = same.next()?;
    same.next().is_none().then_some(found)
}

/// What a reload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reload {
    /// The source was re-pointed at the window its hints name: the collection changed.
    Retargeted,
    /// The source node was restarted; `true` when it was running to restart.
    Restarted(bool),
}

/// The hint keys of a source (ADR-0077): the application and the title its target had.
const HINT_APP: &str = "target_app";
const HINT_TITLE: &str = "target_title";

/// The hints a source gets for `target`: its title always, its application when known.
fn hints_of(target: &Target) -> BTreeMap<String, String> {
    let mut hints = BTreeMap::new();
    if !target.name.is_empty() {
        hints.insert(HINT_TITLE.to_owned(), target.name.clone());
    }
    if let Some(app) = target.app.as_ref().filter(|a| !a.is_empty()) {
        hints.insert(HINT_APP.to_owned(), app.clone());
    }
    hints
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
    /// The application that owns a window, when the OS tells it.
    pub app: Option<String>,
    /// Where the user picked it recently for this source type: 0 is the newest
    /// ([`crate::recent_targets`]); `None` when it is not among the recent targets.
    pub recent: Option<usize>,
    /// Not a target yet: adding it shows the OS picker of `adapter` (systems that
    /// list no screens or windows, such as the Linux screen-cast portal), and the
    /// target the user chooses is added.
    pub picker: bool,
    /// Name of the camera's own microphone, when the audio system has it and it is not
    /// a source yet (ADR-0096 E); `None` otherwise and for everything but cameras.
    pub mic: Option<String>,
    /// The sound that goes with a display or window can be added too (the system can
    /// leave RBS's own sound out of a display's sound); false when it cannot.
    pub sound: bool,
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
    /// It cannot deliver because its target is gone, refused or busy (ADR-0077); `detail`
    /// is the adapter's own text. The UI words the cause by `fault` and the source's kind.
    Unavailable {
        /// The typed cause.
        fault: rbs_core::fault::SourceFault,
        /// The adapter's text, for tooltips and logs.
        detail: String,
    },
}

/// A new kept-profile id: 128 random bits as 32 lowercase hex digits (ADR-0132
/// decision 10).
/// One `browser.input` event on the runner's wire, or why it is not valid.
fn browser_input_event(
    e: &rbs_api::request::BrowserInputEvent,
) -> Result<rbs_browser_ipc::input::InputEvent, String> {
    use rbs_api::request::{BrowserInputEvent as E, BrowserModifiers, BrowserMouseButton};
    use rbs_browser_ipc::input::{self as wire, InputEvent, Key};
    let bits = |m: &BrowserModifiers| {
        [
            (m.shift, wire::SHIFT),
            (m.ctrl, wire::CTRL),
            (m.alt, wire::ALT),
            (m.meta, wire::META),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, bit)| acc | bit)
    };
    Ok(match e {
        E::Move { x, y, modifiers } => InputEvent::Move {
            x: *x,
            y: *y,
            modifiers: bits(modifiers),
        },
        E::Leave => InputEvent::Leave { modifiers: 0 },
        E::Button {
            x,
            y,
            button,
            down,
            clicks,
            modifiers,
        } => {
            if !(1..=3).contains(clicks) {
                return Err("\"clicks\" must be 1, 2 or 3".into());
            }
            InputEvent::Button {
                x: *x,
                y: *y,
                button: match button {
                    BrowserMouseButton::Left => wire::Button::Left,
                    BrowserMouseButton::Middle => wire::Button::Middle,
                    BrowserMouseButton::Right => wire::Button::Right,
                },
                down: *down,
                clicks: *clicks,
                modifiers: bits(modifiers),
            }
        }
        E::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => InputEvent::Wheel {
            x: *x,
            y: *y,
            dx: *dx,
            dy: *dy,
            modifiers: bits(modifiers),
        },
        E::Key {
            key,
            down,
            modifiers,
        } => InputEvent::Key {
            key: Key::parse(key).ok_or_else(|| format!("{key:?} is not a key name"))?,
            down: *down,
            modifiers: bits(modifiers),
        },
        E::Text { text } => {
            if text.chars().count() > rbs_api::request::BrowserInput::MAX_TEXT {
                return Err(format!(
                    "\"text\" is longer than {} characters",
                    rbs_api::request::BrowserInput::MAX_TEXT
                ));
            }
            InputEvent::Text(text.clone())
        }
    })
}

fn new_profile_id() -> Result<String, Refusal> {
    crate::browser_profiles::new_id().map_err(Refusal::Invalid)
}

/// A new browser page's size on a `w` x `h` canvas: the canvas, at most 1920x1080,
/// even ("Use canvas size" on the Page tab sets it).
pub fn attributes_browser_size(w: u32, h: u32) -> (i64, i64) {
    attributes::browser_size(w, h)
}

/// A part of a source's picture the Region menu shows (ADR-0058).
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

/// What a transform command does to an item's layout (ADR-0084).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformOp {
    /// Mirror left to right as seen on the canvas.
    FlipHorizontal,
    /// Mirror top to bottom as seen on the canvas.
    FlipVertical,
    /// Turn clockwise by this many degrees (negative turns the other way).
    Turn(f32),
    /// Set the rotation to this many degrees clockwise, wrapped into `0..360`.
    SetAngle(f32),
    /// Not turned.
    ResetRotation,
    /// Place on the canvas (needs the picture size).
    Fit(FitMode),
    /// Move to the canvas centre on these axes.
    Center(CenterAxis),
    /// Not turned, not flipped, native size (needs the picture size).
    Reset,
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
    /// The canvas cannot be edited now (the vertical canvas shows another scene, or
    /// the source is global there).
    Conflict(String),
}

impl CropRefusal {
    fn text(&self) -> String {
        match self {
            CropRefusal::NotFound(t)
            | CropRefusal::Invalid(t)
            | CropRefusal::Failed(t)
            | CropRefusal::Conflict(t) => t.clone(),
            CropRefusal::NoPicture => {
                "This source has no picture yet, so its crop cannot keep the scale.".into()
            }
        }
    }
}

/// Whether the app shows the preview scene's own picture in studio mode (ADR-0061):
/// macOS, where each picture is its own Core Animation layer, and Windows, where both
/// pictures share the window's one DirectComposition target.
const SCENE_PICTURE: bool = cfg!(any(target_os = "macos", windows));

const STUDIO_MODE_OFF: &str = "Studio mode is off: there is no preview scene.";
const NO_AREA: &str = "The crop must leave at least one pixel of the picture in each direction.";

/// The separate files of the main recording (ADR-0086).
#[derive(Debug, Clone)]
struct IsoFiles {
    /// The tracks, as `record.status` lists them.
    tracks: Vec<api::RecordTrack>,
    /// Marked sources left out, as `record.status` lists them.
    skipped: Vec<api::SkippedTrack>,
    /// The manifest written at start, and its folder; markers are added to it at stop
    /// (ADR-0089 decision 5).
    manifest: rbs_manifest::RecordingManifest,
    /// The recording's folder.
    folder: PathBuf,
    /// The image tracks, whose pictures go into the manifest at stop (ADR-0117
    /// decision 6).
    images: Vec<rbs_record::IsoImageTrack>,
    /// The start gate's decision for the picture files, used once when they open.
    gate: Option<PictureGate>,
    /// Video files skipped because their encoder would have fallen to a software one
    /// (ADR-0087 decision 5): `software_fallback` in the start answer's estimate.
    software_stopped: Vec<SourceId>,
    /// The pause window the sound files follow; the master is started with it
    /// (ADR-0124).
    pause: rbs_record::SharedPause,
}

/// How a `record.start` asks for its separate video files (control API 1.54,
/// ADR-0087 decisions 5 and 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartGate {
    /// Neither `acknowledge_heavy` nor `on_hardware_limit` (older clients, the
    /// desktop buttons, hotkeys): never refused because of separate video files.
    Older,
    /// With either parameter.
    Asked {
        /// The caller accepts a `heavy` plan.
        acknowledge_heavy: bool,
        /// What to do with files past their encoder's capacity.
        policy: LimitPolicy,
    },
}

/// Why a recording did not start.
#[derive(Debug)]
enum StartError {
    /// In words.
    Message(String),
    /// The start gate refused it (control API 1.54): `failed` with `data`.
    Refused(String, Box<api::StartRefusal>),
}

impl From<String> for StartError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl StartError {
    fn into_message(self) -> String {
        match self {
            Self::Message(m) | Self::Refused(m, _) => m,
        }
    }

    fn into_api(self) -> ApiError {
        match self {
            Self::Message(m) => failed(m),
            Self::Refused(m, r) => r.into_error(m),
        }
    }
}

/// The start gate's decision for the separate video files, applied once the master
/// runs (pictures resolve only then).
#[derive(Debug, Clone)]
struct PictureGate {
    /// The only sources that may open a video file (a request with the 1.54
    /// parameters); `None`: every marked source in source id order, a later one taking
    /// the slot of one without a picture (1.52's behaviour).
    keep: Option<Vec<SourceId>>,
    /// Most video files to open.
    count: usize,
    /// No encoder can take a separate video file: the gate's skips are `open_failed`.
    no_encoder: bool,
    /// `on_hardware_limit: "ask"`: a file that cannot open refuses the start.
    ask: bool,
    /// The plan the gate judged, for a refusal found while files open.
    judged: rbs_record::estimate::Estimate,
}

/// Separate video files a take may open while the encoder session limit of this
/// computer is not known (neither measured nor declared): the owner's choice of
/// 2026-11-14 in ADR-0087's Review. With a known limit the estimate decides
/// ([`rbs_record::estimate::separate_video_limit`]). Marked sources past the limit are
/// skipped, in source id order, and listed with the reason `limit`. Image and sound
/// files are not counted.
pub const SEPARATE_VIDEO_LIMIT: usize = rbs_record::estimate::UNKNOWN_SESSIONS_VIDEO_LIMIT;

/// Each separate file's media when the recording stopped, by file name.
type SeparateMedia = Vec<(String, Option<rbs_record::MediaSpan>)>;

/// The canvas an output records or streams (ADR-0093 decision 6).
pub use calibration::{CalibrationSetup, ExportRigMaker};
pub use rbs_api::types::OutputCanvas;
pub use sync_measure::{SyncMeasurePhase, SyncMeasureRefusal, SyncMeasureView};

/// `c`'s place in the host's per-canvas state.
/// A refused pause or resume in words, for its `conflict` (control API 1.61).
fn pause_refusal(e: rbs_record::PauseRefused) -> String {
    use rbs_record::PauseRefused as R;
    match e {
        R::KeyframeUnsupported => "the recording shares its video encoder with another \
            output, and that encoder cannot make a keyframe on request, so the file \
            could not go on cleanly after the pause"
            .to_owned(),
        R::NotPausable => "this recording cannot be paused".to_owned(),
        R::SeparateFiles => "separate files run that do not belong to this recording, so they \
             would go on while it is paused"
            .to_owned(),
        e => e.to_string(),
    }
}

/// How long a marker waits before the manifest is rewritten with it (ADR-0089
/// decision 5).
const MARKER_FLUSH_NS: u64 = 5_000_000_000;

/// `Ok` when an entry changed; else the first refusal, or `conflict` with `none`.
fn changed_or_first_refusal(
    outputs: &[api::OutputEntry],
    refusals: Vec<ApiError>,
    none: &str,
) -> Result<(), ApiError> {
    if outputs.iter().any(|o| {
        matches!(
            o.outcome,
            api::OutputOutcome::Paused | api::OutputOutcome::Resumed | api::OutputOutcome::Held
        )
    }) {
        return Ok(());
    }
    Err(refusals
        .into_iter()
        .next()
        .unwrap_or_else(|| ApiError::new(ErrorKind::Conflict, none)))
}

fn hold_refusal(e: rbs_record::HoldRefused) -> String {
    use rbs_record::HoldRefused as R;
    match e {
        R::NotHoldable => "this stream cannot be held".to_owned(),
        R::PictureUnsupported => {
            "the hold picture cannot be made for this stream's video format".to_owned()
        }
        e => e.to_string(),
    }
}

/// The engine's hold picture for the control API's.
fn hold_picture(p: rbs_api::request::HoldPicture) -> HoldPicture {
    match p {
        rbs_api::request::HoldPicture::Freeze => HoldPicture::Freeze,
        rbs_api::request::HoldPicture::Black => HoldPicture::Black,
        rbs_api::request::HoldPicture::Scene => HoldPicture::Scene,
    }
}

/// The control API's hold picture for the engine's (`freeze` for one it does not
/// know).
fn api_hold_picture(p: HoldPicture) -> rbs_api::request::HoldPicture {
    match p {
        HoldPicture::Black => rbs_api::request::HoldPicture::Black,
        HoldPicture::Scene => rbs_api::request::HoldPicture::Scene,
        _ => rbs_api::request::HoldPicture::Freeze,
    }
}

/// The studio's hold sound for the control API's.
fn hold_sound(s: rbs_api::request::HoldSound) -> rbs_record::HoldSound {
    match s {
        rbs_api::request::HoldSound::Silence => rbs_record::HoldSound::Silence,
        rbs_api::request::HoldSound::Scene => rbs_record::HoldSound::Scene,
    }
}

/// The control API's hold sound for the studio's.
fn api_hold_sound(s: rbs_record::HoldSound) -> rbs_api::request::HoldSound {
    match s {
        rbs_record::HoldSound::Scene => rbs_api::request::HoldSound::Scene,
        _ => rbs_api::request::HoldSound::Silence,
    }
}

/// A refused hold as a control API error (protocol 1.67): a scene not in the
/// collection is `not_found`, one the canvas cannot show `invalid_params`, the rest
/// `conflict`.
fn hold_error(e: rbs_record::HoldRefused) -> ApiError {
    use rbs_record::HoldRefused as R;
    let kind = match e {
        R::UnknownScene => ErrorKind::NotFound,
        R::SceneNotShowable => ErrorKind::InvalidParams,
        _ => ErrorKind::Conflict,
    };
    let message = match e {
        R::OtherSceneHeld => {
            "the stream is already on hold with another scene or sound: resume it first".to_owned()
        }
        e => hold_refusal(e),
    };
    ApiError::new(kind, message)
}

fn slot(c: OutputCanvas) -> usize {
    match c {
        OutputCanvas::Main => 0,
        OutputCanvas::Vertical => 1,
    }
}

/// `u` as the update of `c`'s recording.
fn recording_update(c: OutputCanvas, u: RecordingUpdate) -> Update {
    match c {
        OutputCanvas::Main => Update::Recording(u),
        OutputCanvas::Vertical => Update::VerticalRecording(u),
    }
}

/// `u` as the update of `c`'s stream.
fn stream_update(c: OutputCanvas, u: StreamUpdate) -> Update {
    match c {
        OutputCanvas::Main => Update::Stream(u),
        OutputCanvas::Vertical => Update::VerticalStream(u),
    }
}

/// The state key of `c`'s outputs on a per-canvas topic (protocol 1.29).
fn canvas_key(topic: Topic, c: OutputCanvas) -> StateKey {
    StateKey {
        topic,
        object: Some(c.object()),
    }
}

/// The recording and stream of one canvas, as the host tracks them.
#[derive(Debug)]
struct CanvasOutputs {
    /// The running recording: its file and output (ADR-0047: it runs alongside a
    /// stream).
    recording: Option<(PathBuf, OutputId)>,
    /// Stop was asked for: the recording ended its video and stops once its tail is
    /// in (ADR-0045).
    ending: bool,
    /// The running recording was started with the stream ("Record when the stream
    /// starts"): ending the stream stops it too. Cleared when the recording stops or
    /// the stream ends.
    auto_recording: bool,
    /// The running stream: its URL, the connection last reported and its output. With
    /// several destinations (ADR-0127) this is one destination that still runs, the
    /// stream's anchor: its output carries the stream's hold and counters.
    streaming: Option<(String, Link, OutputId)>,
    /// The destinations of the running stream, the anchor's among them; empty while
    /// nothing streams.
    destinations: Vec<LiveDestination>,
    /// The rows last sent with [`Update::LiveDestinations`], without their time on air.
    destinations_shown: Vec<api::LiveDestination>,
    /// Recording protection ended the recording (ADR-0090 decision 3): the reason for
    /// its summary once the tail is in.
    protection_stop: Option<String>,
    /// The recording, as the control API reports it.
    record_state: api::RecordState,
    /// The stream, as the control API reports it.
    stream_state: api::StreamState,
    /// The running recording's markers (ADR-0089); `None` while nothing records.
    markers: Option<RecordingMarkers>,
    /// The stream's hold as the host keeps it (ADR-0124 decision 14).
    hold: HoldPlan,
    /// The stats summary in the log (ADR-0158), on the main canvas only.
    log_summary: log_summary::LogSummary,
}

/// A destination Go live is about to start: its request and key account.
struct Planned {
    /// What it becomes once started.
    live: LiveDestination,
    /// Where and how to stream to it.
    request: StreamRequest,
    /// Its key's account in the key store.
    account: String,
    /// It is its canvas's first destination.
    first: bool,
}

impl Planned {
    fn of(d: &Resolved, request: StreamRequest, first: bool) -> Self {
        Self {
            live: LiveDestination {
                id: d.destination.id,
                name: d.destination.name.clone(),
                service: d.destination.service.clone(),
                url: request.url.clone(),
                own: d.destination.encoding.is_own(),
                output: None,
                state: DestinationState::Connecting,
                link: Link::default(),
                error: None,
                on_air_since: None,
            },
            request,
            account: d.account.clone(),
            first,
        }
    }

    /// It started as `output`.
    fn started(self, output: OutputId) -> LiveDestination {
        LiveDestination {
            output: Some(output),
            ..self.live
        }
    }

    /// It did not start, because of `why`.
    fn failed(self, why: String) -> LiveDestination {
        LiveDestination {
            state: DestinationState::Failed,
            error: Some(why),
            ..self.live
        }
    }
}

/// The studio output of a stream to `r`'s server with `key`, from `canvas`.
fn stream_output(
    r: &StreamRequest,
    key: Option<Secret>,
    canvas: Option<CanvasId>,
    adapters: &RecordAdapters,
) -> OutputSettings {
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
    out.canvas = canvas;
    out.reconnect = r.reconnect;
    out.transport_options = r.transport_options.clone();
    out.video = Some(r.video.clone());
    out.audio = adapters.audio_encoders.ids().next().map(|_| {
        let mut a = AudioSettings::default();
        a.bitrate_bps = r.audio_bitrate_bps;
        a
    });
    // The size this stream starts with. A later settings change waits for the next
    // start (ADR-0102). `None` stays the canvas.
    out.size = r.size;
    // Every stream can be held (ADR-0124 decision 12).
    out.holdable = true;
    out
}

/// Where one destination of a running stream stands (ADR-0127 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationState {
    /// Started; not connected yet.
    Connecting,
    /// Connected and sending.
    Live,
    /// The connection dropped; the transport tries again.
    Reconnecting,
    /// Refused at start, gave up reconnecting, or its output failed.
    Failed,
    /// Stopped by the user while the others run.
    Stopped,
}

/// One destination of a running stream, as the host tracks it.
#[derive(Debug, Clone)]
struct LiveDestination {
    /// The destination's id in the profile.
    id: u32,
    /// Its name, for notices.
    name: String,
    /// Its service preset's id, or `custom` (control API only).
    service: String,
    /// Its server URL (never with the key).
    url: String,
    /// It has its own encoder chain.
    own: bool,
    /// Its output while it runs.
    output: Option<OutputId>,
    /// Where it stands.
    state: DestinationState,
    /// The connection last read.
    link: Link,
    /// Why it failed, the URL and key never in it.
    error: Option<String>,
    /// When it first connected (master clock).
    on_air_since: Option<Timestamp>,
}

impl LiveDestination {
    /// Its row in the stream's state (control API 1.77), as of `now`.
    fn api(&self, now: Timestamp) -> api::LiveDestination {
        api::LiveDestination {
            id: self.id,
            name: self.name.clone(),
            service: self.service.clone(),
            state: match self.state {
                DestinationState::Connecting => api::DestinationState::Connecting,
                DestinationState::Live => api::DestinationState::Live,
                DestinationState::Reconnecting => api::DestinationState::Reconnecting,
                DestinationState::Failed => api::DestinationState::Failed,
                DestinationState::Stopped => api::DestinationState::Stopped,
            },
            connects: self.link.connects,
            reconnect_attempts: self.link.reconnect_attempts,
            dropped: self.link.transport_drops,
            on_air_ms: self.on_air_since.map_or(0, |t| {
                u64::try_from(now.saturating_since(t).as_millis()).unwrap_or(u64::MAX)
            }),
            error: self.error.clone(),
        }
    }

    /// The name to show: its own, or "Destination <id>".
    fn label(&self) -> String {
        if self.name.is_empty() {
            format!("Destination {}", self.id)
        } else {
            self.name.clone()
        }
    }
}

/// What the host remembers about a stream's hold (ADR-0124 decision 14).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HoldPlan {
    /// A hold asked for that waits until the recording sharing the stream's encoder
    /// is paused: the studio holds a shared chain only then, so the hold picture
    /// never reaches the file.
    pending: Option<HoldAsk>,
    /// The hold paused the recording: Resume stream resumes it, and never a
    /// recording the user paused.
    paused_recording: bool,
}

/// A hold as asked for, with the stream settings' defaults filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HoldAsk {
    picture: HoldPicture,
    /// The scene, with [`HoldPicture::Scene`] only (phase 2b).
    scene: Option<SceneId>,
    sound: rbs_record::HoldSound,
}

impl Default for CanvasOutputs {
    fn default() -> Self {
        Self {
            recording: None,
            ending: false,
            auto_recording: false,
            streaming: None,
            destinations: Vec::new(),
            destinations_shown: Vec::new(),
            protection_stop: None,
            record_state: api::RecordState::Idle { last: None },
            stream_state: api::StreamState::idle(),
            markers: None,
            hold: HoldPlan::default(),
            log_summary: Default::default(),
        }
    }
}

impl CanvasOutputs {
    /// A recording or a stream runs.
    fn busy(&self) -> bool {
        self.recording.is_some() || self.streaming.is_some()
    }
}

/// Why Go live and `stream.start` are refused while no destination of the canvas is
/// on (ADR-0148 decision 8).
pub const STREAMING_OFF: &str =
    "Streaming is off. Add a destination or turn one on in Settings, Stream.";

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
    /// Encoded picture size (ADR-0102). `None`: this output's canvas.
    pub size: Option<(u32, u32)>,
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
        Ok(Self {
            url: p.server.clone(),
            input: server.protocol.input(),
            needs_key: server.protocol.needs_key(),
            reconnect: p.reconnect_policy(),
            transport_options: p.transport_options(),
            ..Self::from_settings(p, encoder)?
        })
    }

    /// The stream's shared settings (encoding, output size) without a server: the
    /// base each destination's request is built on when the canvas's first slot does
    /// not exist (ADR-0148 decision 6). Its own URL is empty and never used.
    pub fn from_settings(p: &StreamProfile, encoder: Option<String>) -> Result<Self, String> {
        if let Some(why) = p.problem() {
            return Err(why);
        }
        let e = &p.encoding;
        Ok(Self {
            url: String::new(),
            input: Protocol::Rtmp.input(),
            needs_key: false,
            reconnect: p.reconnect_policy(),
            transport_options: Vec::new(),
            encoder,
            video: e.video_settings(),
            audio_bitrate_bps: e.audio_bps(),
            size: p.output_size.map(|s| s.pair()),
        })
    }
}

/// A scene the canvas may show, as the Scenes panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneRow {
    /// Its id.
    pub id: SceneId,
    /// Its name.
    pub name: String,
    /// The scene keeps its own levels (ADR-0091).
    pub own_audio: bool,
    /// The transition into this scene (ADR-0135); `None`: the collection's.
    pub transition_in: Option<rbs_scene::SceneTransition>,
}

/// A source of the collection, for "Add existing source".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// Its id.
    pub id: SourceId,
    /// Its name.
    pub name: String,
    /// Its kind; `None` for a type this build does not know.
    pub kind: Option<SourceKind>,
    /// A global source of the studio's canvas (ADR-0065).
    pub global: bool,
    /// Per-source slide override (ADR-0100).
    pub glide: Option<rbs_scene::SourceGlide>,
    /// Which feeds are marked to record as files of their own (ADR-0086).
    pub record_separately: RecordSeparately,
}

/// The scenes and sources of the collection, as the Scenes panel and the source
/// pickers show them (ADR-0059).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneList {
    /// The scenes the canvas may show, in panel order.
    pub scenes: Vec<SceneRow>,
    /// The scenes a stream of each canvas (main, vertical) may hold with (ADR-0124
    /// phase 2b), in panel order; the vertical one empty while it does not exist.
    pub hold_scenes: [Vec<SceneRow>; 2],
    /// The one on air.
    pub program: SceneId,
    /// The preview scene in studio mode (ADR-0061); `None` outside it.
    pub preview: Option<SceneId>,
    /// The canvas's master volume and mute (ADR-0081).
    pub master: MasterAudio,
    /// Collection morph defaults (ADR-0100).
    pub glide: rbs_scene::CollectionGlide,
    /// The collection's transition (ADR-0128).
    pub transition: rbs_scene::CollectionTransition,
    /// The stinger's found cut point in ms (ADR-0140 decision 3); `None` until found.
    pub stinger_cut_found_ms: Option<u32>,
    /// The length of the stinger's play range in ms, once its file was looked at.
    pub stinger_length_ms: Option<u32>,
    /// The stinger's file is being looked at.
    pub stinger_looking: bool,
    /// The stinger's file has transparency no adapter here keeps (decision 9).
    pub stinger_alpha_unsupported: bool,
    /// Every source of the collection, by name.
    pub sources: Vec<SourceEntry>,
}

/// Why a scene, item or source edit was refused (ADR-0059): words for the UI, and the
/// control API's error kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The object does not exist (any more).
    NotFound(String),
    /// The request is not valid (an empty name, a volume out of range).
    Invalid(String),
    /// It conflicts with the state (a taken name, the last scene).
    Conflict(String),
    /// Something else went wrong.
    Failed(String),
}

impl Refusal {
    /// In words.
    pub fn text(&self) -> &str {
        match self {
            Self::NotFound(s) | Self::Invalid(s) | Self::Conflict(s) | Self::Failed(s) => s,
        }
    }

    fn of(e: &SceneError) -> Self {
        match e {
            SceneError::UnknownSource(_)
            | SceneError::UnknownScene(_)
            | SceneError::UnknownItem(_)
            | SceneError::UnknownFilter(_) => Self::NotFound(edit_error(e)),
            SceneError::DuplicateName(_) | SceneError::StingerPlaced(_) => {
                Self::Conflict(edit_error(e))
            }
            SceneError::EmptyName
            | SceneError::InvalidVolume
            | SceneError::InvalidSyncOffset
            | SceneError::InvalidShowFor
            | SceneError::InvalidActivity
            | SceneError::InvalidTransition
            | SceneError::ChromeNotDisplay
            | SceneError::IndexOutOfRange(_) => Self::Invalid(edit_error(e)),
            _ => Self::Failed(edit_error(e)),
        }
    }
}

/// Smallest and largest side of a vertical canvas the control API accepts: even (the
/// 4:2:0 formats of the encoders need it) and within what the hardware encoders take.
const VERTICAL_MIN_SIDE: u32 = 64;
const VERTICAL_MAX_SIDE: u32 = 8192;

/// What [`Command::AddVerticalCanvas`] makes (ADR-0093 decision 8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalCanvas {
    /// The picture size, for example 1080x1920 or 720x1280. It runs at the main canvas's
    /// frame rate.
    pub size: Size,
    /// Shows the main canvas's program scene and switches with it (the default for a
    /// vertical canvas); else it keeps the scene on air now and is switched on its own.
    pub follow: bool,
    /// How to arrange the program scene's sources for the new shape; `None`: they keep
    /// the layout of the main canvas.
    pub layout: Option<AutoLayout>,
}

/// The second canvas as the host runs it ([`Update::Vertical`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerticalState {
    /// Its canvas.
    pub canvas: CanvasId,
    /// Its picture size.
    pub size: Size,
    /// It shows the main canvas's program scene.
    pub follow: bool,
}

/// What became of a projector (ADR-0125 decisions 3 and 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectorState {
    /// Open: its picture runs or is being prepared.
    Open,
    /// Kept with no picture until its canvas comes back (the vertical canvas was
    /// removed, or the collection has none).
    Suspended,
    /// Closed by the studio, with the reason to tell the user: a limit was reached or
    /// its picture failed. The UI closes the window.
    Closed(String),
}

/// At most this many projectors are open at a time (ADR-0125 decision 4).
pub const MAX_PROJECTORS: usize = 4;
/// Surface memory all open projector pictures may hold together, counted at
/// [`MAX_PREVIEW_SURFACES`] surfaces of 4 bytes per pixel each (ADR-0125 decision 4).
pub const MAX_PROJECTOR_SURFACE_BYTES: u64 = 512 * 1024 * 1024;

/// The surface memory a projector of a `size` canvas counts toward
/// [`MAX_PROJECTOR_SURFACE_BYTES`].
pub fn projector_surface_bytes(size: Size) -> u64 {
    u64::from(size.width) * u64::from(size.height) * 4 * MAX_PREVIEW_SURFACES as u64
}

/// Why a projector of `size` may not open next to projectors counting `open` bytes
/// in `count` projectors; `None` when it may.
pub fn projector_refusal(count: usize, open: u64, size: Size) -> Option<&'static str> {
    if count >= MAX_PROJECTORS || open + projector_surface_bytes(size) > MAX_PROJECTOR_SURFACE_BYTES
    {
        return Some("Too many projectors are open. Close one first.");
    }
    None
}

/// One projector as the host keeps it (ADR-0125 decision 3).
struct ProjectorRun {
    id: u32,
    canvas: OutputCanvas,
    /// Its picture in the running studio; `None` while suspended or not yet added.
    picture: Option<PictureId>,
    /// Counted toward the limits (its canvas's size) while it has a picture.
    size: Size,
    /// What was last sent to the UI.
    sent: Option<(Option<PreviewEndpoint>, ProjectorState)>,
}

/// The main display's framebuffer, for the Video settings row (ADR-0102 decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainDisplay {
    /// Physical pixels. Not multiplied by [`Self::scale`].
    pub size: (u32, u32),
    /// OS pixels per logical point, when the adapter read it. `None` when it did not
    /// (the label then has no percent).
    pub scale: Option<rbs_capture::DisplayScale>,
}

/// What the Mixer and Settings › Audio show of the loudness meter ([`Update::Loudness`]).
#[derive(Clone, Debug)]
pub struct LoudnessView {
    /// Show loudness is on (`loudness.set_enabled`, saved in `loudness.toml`).
    pub shown: bool,
    /// The running meter's values; `None` while it is off or the studio has no audio.
    pub readout: Option<Arc<rbs_audio::LoudnessReadout>>,
    /// The target, LUFS.
    pub target: f32,
    /// The true-peak limit, dBTP.
    pub true_peak_limit: f32,
}

impl PartialEq for LoudnessView {
    fn eq(&self, o: &Self) -> bool {
        let same = match (&self.readout, &o.readout) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same && self.shown == o.shown
            && self.target.to_bits() == o.target.to_bits()
            && self.true_peak_limit.to_bits() == o.true_peak_limit.to_bits()
    }
}

/// A live stream's hold state for the window (ADR-0124), as of the moment it was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHold {
    /// It is on hold now (or the hold waits for a sharing recording's pause).
    pub held: bool,
    /// A hold scene fell back (ADR-0124 phase 2b), while it does.
    pub note: Option<HoldNote>,
}

/// How a scene hold fell back, for the Stream card's notice (ADR-0124 phase 2b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldNote {
    /// The scene could not open or went away: the last frame and silence go out.
    Picture,
    /// The scene's sound failed: its picture goes out with silence.
    Sound,
}

/// A running recording's pause state for the window (ADR-0124), as of the moment it
/// was sent: the window moves its clocks on from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordPause {
    /// It is paused now.
    pub paused: bool,
    /// What the files hold: the time recorded, pauses left out.
    pub recorded_ns: u64,
}

/// A marker just added, for the window's notice (ADR-0089 decision 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerNote {
    /// Its label ("Marker 3").
    pub label: String,
    /// Its place in the recording, from the start, after the lead.
    pub t_ns: u64,
    /// Each recording's canvas and the marker's index there, to undo it.
    pub placed: Vec<(OutputCanvas, u32)>,
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
    /// A replay clip writer finished.
    ReplayWritten(Result<std::path::PathBuf, String>),
    /// A thumbnail worker finished one target.
    Thumbnail {
        generation: u64,
        adapter: String,
        target: String,
        picture: Option<rbs_capture::Thumbnail>,
    },
    /// A region-preview worker finished.
    RegionPreview {
        generation: u64,
        id: SourceId,
        picture: Option<rbs_capture::RegionGrab>,
    },
    /// The worker of calibration run `id` reports (ADR-0087 step D1).
    Calibration {
        id: u64,
        msg: calibration::WorkerMsg,
    },
    /// An OS permission prompt was answered (ADR-0141).
    PermissionAnswered(PermissionKind),
    /// The stinger's file look finished (ADR-0140 decision 3).
    StingerLook {
        key: stinger::LookKey,
        result: Result<rbs_sources::stinger::StingerLook, rbs_sources::stinger::LookError>,
    },
    /// A text source's box was measured off the control thread.
    TextMeasured {
        id: SourceId,
        text: String,
        size: [u32; 2],
    },
    /// The install thread of catalog entry `index` reports (ADR-0145).
    AddOn {
        index: usize,
        msg: add_ons::AddOnMsg,
    },
}

/// How the studio starts.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canvas size.
    pub size: Size,
    /// Canvas rate.
    pub fps: FrameRate,
    /// The preview's size, or `None` for no preview. The studio sets the ring
    /// (`surfaces`) from the canvas frame rate when it starts and whenever the
    /// canvas changes (ADR-0034, ADR-0036); a count stored here is replaced.
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
    /// The platform's virtual camera, `None` where there is none: the studio sends
    /// to it on [`Command::StartVirtualCamera`].
    pub virtual_camera: Option<CameraFactory>,
    /// The config folder where the recent targets ([`crate::recent_targets`]) and
    /// `calibration.toml` are kept; `None`: they are kept in memory only.
    pub config_dir: Option<PathBuf>,
    /// A transition also puts the old program scene in preview (ADR-0098); false by
    /// default.
    pub swap_after_transition: bool,
    /// How long a paused or cued media file keeps its decoder (`pause_release_s`,
    /// ADR-0108 decision 10); 30 s by default.
    pub media_pause_release: Duration,
    /// The monitor output's device and volume (ADR-0097; `monitor.toml`). The device
    /// is opened only while a source is monitored or soloed.
    pub monitor: MonitorSettings,
    /// The loudness meter's saved settings (ADR-0092; `loudness.toml`): Show loudness,
    /// the target and the true-peak limit.
    pub loudness: crate::loudness_settings::LoudnessSettings,
    /// Reads free disk space and memory for recording protection (ADR-0090); `None`:
    /// disk and memory are reported unavailable and only lag is watched.
    pub system_probe: Option<protection::SystemProbeFactory>,
    /// The last calibration of this computer (`calibration.toml`, ADR-0087 decision
    /// 4), read with `app.toml` before the control thread starts; empty by default.
    pub calibration: crate::calibration_file::CalibrationFile,
    /// Reads a recording folder's free space for the estimate's disk probe (ADR-0087
    /// decision 1), on the probe's own short-lived thread; `None`: free space is
    /// unknown.
    pub read_free_space: Option<FreeSpaceReader>,
    /// Writes markers into a finished Matroska master as chapters (ADR-0159 decision 4);
    /// `None`: the markers stay in the manifest only.
    pub chapters: Option<Arc<dyn ChapterWriter>>,
    /// The OS name and version in the calibration key; always pass it (ADR-0159).
    pub os_version: Option<String>,
    /// The main display for the Video settings row at start (ADR-0102 decision 8);
    /// `None`: unknown. Off macOS, roots pass [`display_from_default_screen`].
    pub main_display: Option<MainDisplayProbe>,
    /// The calibration run's clock, pacer, file system and wall time (ADR-0087 step
    /// D1); `None`: the real ones. Tests inject a test clock and a fake sink.
    pub calibration_setup: Option<calibration::CalibrationSetup>,
    /// The add-on catalog, where add-ons are kept and how they are fetched (ADR-0145;
    /// `rbs_app::add_ons::system_setup` in the app). `None`: `addon.list`
    /// shows the built-in catalog as not installed and installs are refused.
    pub add_ons: Option<crate::add_ons::AddOnsSetup>,
    /// Makes the browser tree at each start (ADR-0159 decision 4); `None`: no helper,
    /// so browser sources are Unavailable with `helper_missing`.
    pub browser: Option<Arc<dyn BrowserTrees>>,
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
        let preview = cfg!(any(target_os = "macos", windows)).then(|| preview_desc(size, fps));
        Self {
            size,
            fps,
            preview,
            keys: Arc::new(MemoryKeyStore::new()),
            key_account: stream_key::ACCOUNT.to_owned(),
            collection: None,
            autosave: None,
            virtual_camera: None,
            config_dir: None,
            swap_after_transition: false,
            media_pause_release: rbs_record::MEDIA_PAUSE_RELEASE,
            monitor: MonitorSettings::default(),
            loudness: crate::loudness_settings::LoudnessSettings::default(),
            system_probe: None,
            calibration: crate::calibration_file::CalibrationFile::default(),
            read_free_space: None,
            chapters: None,
            os_version: None,
            main_display: None,
            calibration_setup: None,
            add_ons: None,
            browser: None,
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
    /// In a stream start, after the output started and before the host tracks it.
    AfterStreamStart,
    /// In a stream end, after the output stopped and before the host forgets it.
    InStreamEnd,
    /// While a canvas change is restarting the studio, before that restart is over.
    DuringCanvasRestart,
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
use inject;

// How long the platform's preview layer holds a replaced frame: the `hold`
// `PreviewLayer::surfaces_needed` uses at its default retire setting. The layer is
// created later, in the UI, so the studio applies that default here.
#[cfg(windows)]
fn preview_hold(frame_interval: Duration) -> Duration {
    frame_interval.saturating_add(rbs_gpu::share::DEFAULT_RETIRE_POLL)
}

#[cfg(target_os = "macos")]
fn preview_hold(_frame_interval: Duration) -> Duration {
    rbs_gpu::share::DEFAULT_RETIRE_AFTER
}

#[cfg(not(any(windows, target_os = "macos")))]
fn preview_hold(_frame_interval: Duration) -> Duration {
    // No layer on this platform (the host publishes no preview). The macOS default,
    // so the ring arithmetic still has one answer in tests.
    Duration::from_millis(34)
}

/// Surfaces in the preview ring for a canvas at `fps`, capped at
/// [`MAX_PREVIEW_SURFACES`]. Same count as `PreviewLayer::surfaces_needed` with the
/// platform's default hold ([`ring_needed`], ADR-0034, ADR-0036).
fn preview_surfaces(fps: FrameRate) -> usize {
    let interval = fps.frame_duration();
    ring_needed(interval, preview_hold(interval)).min(MAX_PREVIEW_SURFACES)
}

/// The preview for a canvas of `size` at `fps`: its aspect ratio, the longer side
/// 1280 pixels (or the canvas's, if smaller), even sides, and [`preview_surfaces`].
fn preview_desc(size: Size, fps: FrameRate) -> PreviewDesc {
    let (width, height) = crate::canvas_profile::preview_size(size.width, size.height);
    PreviewDesc {
        width,
        height,
        surfaces: preview_surfaces(fps),
    }
}

/// The running control thread. Dropping it stops the studio and joins the thread.
pub struct StudioHost {
    tx: Option<Arc<ControlOut>>,
    thread: Option<JoinHandle<()>>,
    /// Meters the control API samples for `audio.levels`.
    levels: Arc<crate::levels::LevelFeed>,
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
        let tx = ControlOut::new(tx);
        let levels = crate::levels::LevelFeed::new();
        let thread = std::thread::Builder::new()
            .name("rbs-studio".into())
            .spawn({
                let inbox = Arc::downgrade(&tx);
                let levels = Arc::clone(&levels);
                move || {
                    run(
                        config,
                        adapters,
                        gpu,
                        engine,
                        ControlIo { rx, inbox, levels },
                        &notify,
                    )
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
            levels,
        })
    }

    /// The meters `audio.levels` samples while remote control is on.
    pub fn level_feed(&self) -> Arc<crate::levels::LevelFeed> {
        Arc::clone(&self.levels)
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
        let tx: std::sync::Weak<ControlOut> =
            self.tx.as_ref().map(Arc::downgrade).unwrap_or_default();
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
        crate::ui_thread::assert_off_ui("stopping the studio host"); // Joins (ADR-0156).
        // The last `ControlOut` disconnects and unparks the parked control thread, here
        // or on a worker that still holds it.
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
    fn handle_as(
        &mut self,
        caller: &rbs_api::Caller,
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
        self.inner.handle_as(caller, command, events)
    }

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

/// What the Source settings window shows for a source (ADR-0077).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSettingsView {
    /// The source.
    pub id: SourceId,
    /// Its name.
    pub name: String,
    /// Its kind.
    pub kind: SourceKind,
    /// The modes its camera offers (cameras only, empty when unknown).
    pub modes: Vec<rbs_sources::SourceMode>,
    /// The stored size, when set.
    pub size: Option<(u32, u32)>,
    /// The stored frame rate, when set.
    pub fps: Option<u32>,
    /// Draw the cursor (screens and windows).
    pub cursor: bool,
    /// Capture region (ADR-0103).
    pub cap_has: bool,
    /// Left edge of the capture region.
    pub cap_l: u32,
    /// Top edge of the capture region.
    pub cap_t: u32,
    /// Right edge of the capture region.
    pub cap_r: u32,
    /// Bottom edge of the capture region.
    pub cap_b: u32,
    /// Display chrome (ADR-0104); screen capture only.
    pub chrome_on: bool,
    /// Top chrome inset.
    pub chrome_t: u32,
    /// Right chrome inset.
    pub chrome_r: u32,
    /// Bottom chrome inset.
    pub chrome_b: u32,
    /// Left chrome inset.
    pub chrome_l: u32,
    /// Chrome fill: 0 cover, 1 fit.
    pub chrome_fill: u8,
    /// The display reported a work area for Reset chrome.
    pub has_work_rect: bool,
    /// Delivered picture size when known (for region drag scale).
    pub pic_w: u32,
    /// Delivered picture height when known.
    pub pic_h: u32,
}

/// A listed target as the UI shows it.
fn target_of(t: SourceTarget) -> Target {
    Target {
        name: t.name,
        adapter: t.adapter.as_str().to_owned(),
        target: t.target,
        is_default: t.is_default,
        size: t.size,
        app: t.app,
        recent: None,
        picker: false,
        mic: None,
        sound: true,
    }
}

/// Where `target` is in the recent list (0 is the newest); pickers are never recent.
fn recent_rank(recent: &[Recent], target: &Target) -> Option<usize> {
    if target.picker {
        return None;
    }
    recent
        .iter()
        .position(|r| r.adapter == target.adapter && r.target == target.target)
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
    inbox: std::sync::Weak<ControlOut>,
    /// What the root passed instead of adapters (ADR-0159 decision 4).
    seams: seams::HostSeams,
    /// The stinger's file look (ADR-0140 decision 3).
    stinger: stinger::StingerFile,
    /// Saves the collection after a change ([`HostConfig::autosave`]).
    autosave: Option<CollectionSaver>,
    /// The recent targets per source type and the folder they are saved in.
    recent: RecentTargets,
    config_dir: Option<PathBuf>,
    /// The last calibration of this computer (ADR-0087 decision 4).
    calibration: crate::calibration_file::CalibrationFile,
    /// The calibration run, *Test this computer* (ADR-0087 step D1).
    calibration_run: calibration::CalibrationHost,
    /// The sync offset remembered per audio device (`device-sync.toml`, ADR-0126
    /// decision 7), read at start like the recent targets.
    device_sync: crate::device_sync_file::DeviceSyncFile,
    /// The last measurement result offered, until an offset change of any source.
    offered: Option<device_sync::Offered>,
    /// The sync offset measurement (ADR-0126); nothing runs without one.
    measure: sync_measure::MeasureHost,
    /// Free space of recording folders for the estimate; `None` when the app gave no
    /// reader. Starts a thread only while a read runs (P13).
    disk_probe: Option<rbs_record::estimate::DiskProbe>,
    /// A transition also puts the old program scene in preview (ADR-0098).
    swap_after_transition: bool,
    /// The media pause release the studio is started with (`pause_release_s`).
    media_pause_release: Duration,
    /// The play states sent last ([`Update::Media`]); `None` sends them again.
    media_sent: Option<Vec<MediaTick>>,
    /// The generation of the thumbnail request that may still deliver (ADR-0077).
    thumbs: Arc<AtomicU64>,
    /// OS permission prompts showing now, each with the commands waiting for its
    /// answer (ADR-0141 decision 4). One prompt per kind at a time.
    permission_asking: HashMap<PermissionKind, Vec<Command>>,
    /// Kinds asked for in this run: adding a source prompts at most once per kind.
    permission_asked: Vec<PermissionKind>,
    /// The kinds some shown source was refused, when the UI last got the statuses for
    /// them: a newly refused kind sends the statuses, so Properties offers Allow or Fix
    /// permission.
    permission_refused: Vec<PermissionKind>,
    /// The generation of the region-preview grab that may still deliver (ADR-0103).
    region_grabs: Arc<AtomicU64>,
    /// The collection as last handed to `autosave`.
    saved: Option<SceneCollection>,
    studio: Studio,
    adapters: RecordAdapters,
    collection: SceneCollection,
    /// Bumped when the whole collection value is replaced. Together with
    /// [`SceneCollection::generation`] it says whether source rows must be rebuilt.
    collection_epoch: u64,
    /// The last [`Self::list_stamp`]. `None` until the first publish.
    list_stamp: Option<u64>,
    canvas: CanvasId,
    /// The canvas's program scene: the one the Sources panel and the source commands
    /// edit (kept in step by [`Host::keep_program`]).
    scene: SceneId,
    rows: Vec<SourceRow>,
    /// The program scene's rows last published (the Mixer and `source.changed`).
    heard: Vec<SourceRow>,
    /// The global sources last published to the Mixer (ADR-0065).
    globals_shown: Vec<SourceRow>,
    /// Scene sources an API call places into, instead of the edited scene.
    into: Option<SceneId>,
    /// In studio mode with the program scene also in preview: the copy of the program
    /// scene the program shows until the next transition, so edits stay off air
    /// (ADR-0063). Never saved, never listed.
    held: Option<Held>,
    /// The scene the last held copy was taken from, so a transition away from the
    /// copy names the real scene on `transition.state`.
    held_was: Option<SceneId>,
    /// The id of the last held copy, so the next copy taken after it gets the other
    /// id and a transition from one copy to the next is seen as one.
    held_id_was: Option<SceneId>,
    /// The preview scene's picture endpoint last sent to the UI.
    scene_preview_sent: Option<PreviewEndpoint>,
    /// The vertical canvas's panel shows: its picture runs (ADR-0093 decision 7).
    vertical_picture: bool,
    vertical_preview_sent: Option<PreviewEndpoint>,
    /// Open and suspended projectors (ADR-0125 decision 3). Empty: nothing of them.
    projectors: Vec<ProjectorRun>,
    /// Open Interact windows (ADR-0132 decision 6). Empty: nothing of them.
    interacts: Vec<interact::InteractRun>,
    /// The scene list as last sent.
    scenes_shown: Option<SceneList>,
    /// `scene.list` for the snapshot last published or last polled. See
    /// [`scene_cache`].
    scene_answer: scene_cache::SceneAnswerCache,
    /// The second canvas that runs, and what was last sent about it (ADR-0093).
    vertical: Option<VerticalState>,
    /// The saved vertical canvas could not start again; told once at the next publish.
    vertical_restore_error: Option<String>,
    /// Whether Record and Go live also act on the vertical outputs (ADR-0111), as the
    /// UI last sent them.
    vertical_flags: VerticalFlags,
    /// The output profile's Separate files switch (ADR-0086 decision 4).
    separate_files: bool,
    /// The profile's `recording.iso.on_hardware_limit` (ADR-0087 decision 5), and
    /// whether this build knows the saved value.
    limit_policy: (LimitPolicy, bool),
    /// Scenes `add_vertical` made shared, so removing the canvas can scope them back.
    vertical_shared: Vec<SceneId>,
    vertical_sent: Option<Option<VerticalState>>,
    /// The master meter last sent to the UI.
    master_sent: Option<Arc<rbs_audio::LevelMeter>>,
    /// Meters published for `audio.levels`.
    levels: Arc<crate::levels::LevelFeed>,
    /// The last picture size seen per video source (ADR-0058).
    sizes: BTreeMap<SourceId, (u32, u32)>,
    /// What Copy attributes took (ADR-0133 decision 6); `None` costs nothing.
    attribute_clipboard: Option<attributes::AttributeSnapshot>,
    /// The user's undo history of scene edits, shared with control API clients.
    history: History<Undo>,
    /// The next undo and redo as last sent.
    history_shown: (Option<StepInfo>, Option<StepInfo>),
    waiting: bool,
    /// A program switch held back until its entering texts are uploaded, at most
    /// `TEXT_WAIT_TICKS` render ticks (ADR-0129 decision 7).
    held_switch: Option<SceneId>,
    /// The program scene the studio ran at the last update, as (its id in the run
    /// collection, the user's scene it shows): a held copy (ADR-0063) is the scene it
    /// was taken from. `None` before the first update.
    ran_program: Option<(SceneId, SceneId)>,
    /// The latest T-bar value not yet applied (ADR-0135 decision 1).
    pending_progress: Option<f32>,
    /// A T-bar's end is making the program change: it does not cancel itself.
    taking: bool,
    /// A manual transition held on the main canvas at the end of the last round.
    tbar_manual: bool,
    /// The T-bar went to 0 this round: the rail is there already.
    tbar_zeroed: bool,
    /// How long the main recording and stream have run, for live texts (ADR-0129
    /// decision 11).
    text_elapsed: text_live::TextElapsed,
    /// `text-timers.toml` as last read or written.
    text_timer_file: Vec<crate::text_timers::Entry>,
    /// The collection's saved timers were handed to the studio.
    text_timers_restored: bool,
    /// A text item's drag waiting to rest before it gets its new box (ADR-0129
    /// decision 12).
    crisp_pending: Option<text_resize::CrispPending>,
    /// The font picker waits for the families (read off the control thread).
    fonts_wanted: bool,
    /// The recording and stream of each canvas, by [`slot`]
    /// (ADR-0093 decision 6).
    outs: [CanvasOutputs; 2],
    /// The main recording's separate files while it runs (ADR-0086).
    iso: Option<IsoFiles>,
    /// Where the stream key is kept.
    keys: Arc<dyn KeyStore>,
    /// The account of the output profile in use.
    key_account: String,
    /// Makes an engine for a restart.
    engine: Box<dyn Fn() -> Engine>,
    gpu: Arc<GpuContext>,
    /// The browser runner tree (ADR-0109), as the studio takes it.
    browser_tree: Option<Arc<dyn BrowserTree>>,
    /// The same tree: whether it needs the engine and whether a runner is alive.
    browser: Arc<dyn HostBrowserTree>,
    /// The installed browser engine the tree passes to each runner (ADR-0145).
    engine_slot: Arc<EngineSlot>,
    /// `browser-profiles/`, where kept sign-ins live (ADR-0132 decision 10); `None`
    /// when this run keeps none.
    profiles_dir: Option<std::path::PathBuf>,
    /// The add-on catalog and running installs (ADR-0145).
    add_ons: add_ons::AddOnsHost,
    /// The preview's ring size from the canvas rate at the last start, when this
    /// platform publishes a preview. Restarts size the ring again from the canvas
    /// that is about to run.
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
    /// The Mixer panel is shown. Combined with an `audio.levels` subscription.
    meters_ui: bool,
    /// What the mixer was last told.
    meters_applied: bool,
    /// The control API, while remote control is on.
    api: Option<Api>,
    /// The recording settings as the UI saved them.
    profile: RecordProfile,
    /// Kinds the adapters can add.
    kinds: Vec<SourceKind>,
    /// Video encoders, best first. Empty until [`Host::ensure_encoders`].
    encoders: Vec<EncoderInfo>,
    /// Video and audio encoders have been probed once.
    encoders_probed: bool,
    /// Why this system cannot record.
    cannot_record: Option<String>,
    /// The stream settings as the UI uses them; `None` until the UI sent them.
    stream_profile: Option<StreamProfile>,
    /// The vertical canvas's recording settings as saved; `None`: it uses `profile`.
    vertical_profile: Option<RecordProfile>,
    /// The vertical canvas's stream settings as saved; `None`: none saved, so the
    /// vertical stream has no server.
    vertical_stream_profile: Option<StreamProfile>,
    /// The output profile's stream destinations as the UI sent them (ADR-0127).
    destinations: Destinations,
    /// The destinations section did not load (ADR-0148 decision 1): streaming counts
    /// as available on both canvases.
    destinations_unloaded: bool,
    /// The output profiles and scene collections as the UI listed them last; `None`
    /// until the UI sent them.
    library: Option<(api::Library, api::Library)>,
    /// The `seq` of a switch asked for through the control API that the UI has not
    /// answered yet ([`Command::SetLibrary`]). Outputs cannot start meanwhile.
    switch_pending: Option<u64>,
    /// The last `seq` given to [`Update::SwitchRequested`].
    switch_seq: u64,
    /// When the Stats panel's counters go out next, on `clock`; `None` while no
    /// output runs.
    stats_due: Option<Timestamp>,
    /// The studio's master clock (the engine's), for the stats interval.
    clock: Arc<dyn Clock>,
    /// The platform's virtual camera.
    camera_factory: Option<Arc<dyn VirtualCameraFactory>>,
    /// What the virtual camera sends while it sends: started again after a restart.
    camera: Option<VirtualCameraSettings>,
    /// When the camera's counters go out next; `None` while it does not send.
    camera_due: Option<Timestamp>,
    /// The monitor output: its saved device and volume, when its device opens, what
    /// the API saw last (ADR-0097).
    monitor: monitor::MonitorHost,
    /// The main canvas's loudness meter runs (`loudness.set_enabled`), with its saved
    /// target and limit (ADR-0092; `loudness.toml`, where `shown` is `loudness_on`).
    loudness_on: bool,
    loudness_settings: crate::loudness_settings::LoudnessSettings,
    /// What the UI was last told of the loudness meter ([`Update::Loudness`]).
    loudness_sent: Option<LoudnessView>,
    /// The camera as the app's settings know it; `None` until the app sent it.
    camera_setup: Option<CameraSetup>,
    /// The `vcam.changed` state last published.
    camera_published: Option<api::VcamStatus>,
    /// The replay buffer's settings and state (ADR-0088).
    replay: replay::ReplayHost,
    /// Recording protection while an output runs (ADR-0090).
    protection: protection::ProtectionHost,
}

/// What the control thread is handed besides the studio configuration.
struct ControlIo {
    rx: Receiver<Msg>,
    inbox: std::sync::Weak<ControlOut>,
    levels: Arc<crate::levels::LevelFeed>,
}

fn run(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    io: ControlIo,
    notify: &dyn Fn(Update),
) {
    let ControlIo { rx, inbox, levels } = io;
    // A control thread, not a real-time one: its panics get crash reports (ADR-0055).
    rbs_crash::report_panics_on_this_thread();
    let started = rbs_crash::contain(|| start(config, adapters, gpu, engine, levels))
        .unwrap_or_else(|_| Err(rbs_crash::CAUGHT_TEXT.to_owned()));
    let mut host = match started {
        Ok((mut host, update)) => {
            notify(update);
            host.inbox = inbox;
            host.studio.bind_control_thread();
            if let Some(out) = host.inbox.upgrade() {
                out.bind(host.studio.control_wake());
            }
            host
        }
        Err(e) => {
            rbs_core::error!(
                rbs_core::log::Area::Studio,
                "the studio could not start: {e}"
            );
            notify(Update::Failed(e.clone()));
            // Keep answering until the UI goes away, so it never waits on a dead thread;
            // control API requests get the reason.
            let mut api: Option<Api> = None;
            while let Ok(m) = rx.recv() {
                match m {
                    Msg::AttachApi(a) => api = Some(*a),
                    Msg::DetachApi => api = None,
                    Msg::Ui(_)
                    | Msg::ApiWake
                    | Msg::PickFailed(_)
                    | Msg::ReplayWritten(_)
                    | Msg::Thumbnail { .. }
                    | Msg::RegionPreview { .. }
                    | Msg::Calibration { .. }
                    | Msg::PermissionAnswered(_)
                    | Msg::StingerLook { .. }
                    | Msg::TextMeasured { .. }
                    | Msg::AddOn { .. } => {}
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
    // The first round runs before any park, so a bypass during start is collected.
    let mut wait = Some(Duration::ZERO);
    let mut caught_in_row = 0;
    loop {
        let msg = match rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => break,
        };
        if msg.is_none() {
            match wait {
                Some(d) if d.is_zero() => {}
                due => {
                    if host.studio.bypass_pending() {
                        wait = Some(Duration::ZERO);
                        continue;
                    }
                    // The load above is the last read before this park. Nothing in
                    // between may park, or it would consume the bypass token.
                    match due {
                        Some(d) => std::thread::park_timeout(d),
                        None => std::thread::park(),
                    }
                    wait = Some(Duration::ZERO);
                    continue;
                }
            }
        }
        // Each round is a panic boundary (ADR-0055 decision 3): a bug in one command or
        // one round fails that round, and the studio and its outputs keep running.
        let kind = failures::Kind::of(msg.as_ref());
        let logged = |u: Update| {
            failures::log_update(kind, &u);
            notify(u);
        };
        match rbs_crash::contain(|| round(&mut host, &mut failed, msg, &logged)) {
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
                rbs_core::error!(rbs_core::log::Area::Studio, "a studio round panicked");
                wait = Some(MAX_WAIT);
                notify(Update::Error(rbs_crash::CAUGHT_TEXT.into()));
                let _ = rbs_crash::contain(|| host.recover(&mut failed, caught_in_row, notify));
            }
        }
    }
    // A calibration run stops; the app does not wait for its worker.
    host.calibration_run.stop();
    // Closing while recording: keep the tails too, as Stop does.
    let ending: Vec<OutputId> = host
        .outs
        .iter()
        .filter_map(|o| o.recording.as_ref().map(|(_, id)| *id))
        .filter(|id| host.studio.end_output(*id))
        .collect();
    let started = std::time::Instant::now();
    while !ending.iter().all(|id| host.studio.output_tail_done(*id))
        && started.elapsed() < CLOSE_TAIL_GUARD
    {
        host.studio.service();
        // The control thread at shutdown, not the UI thread (ADR-0055 decision 6).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(ENDING_WAIT);
    }
    for c in OutputCanvas::ALL {
        host.end_stream(c, None, notify);
        host.stop_recording(c, None, notify);
    }
    host.save_text_timers();
    let _ = host.studio.shutdown();
}

/// Why a text's box cannot change during a take that records its picture separately
/// (ADR-0129 decision 5).
const STOP_TO_CHANGE_BOX: &str =
    "Stop recording to change the box: the separate picture track keeps one size.";

/// How often the control loop looks at a program switch held back for its texts.
const HELD_SWITCH_POLL: Duration = Duration::from_millis(4);

/// Caught panics in a row after which the studio counts as failed: its recording and
/// stream end cleanly instead of running on with a host that keeps panicking.
const CAUGHT_LIMIT: u32 = 3;

/// One round of the control loop: the message that woke it (`None`: the wait ended),
/// one API request, then the studio's service. Returns how long to wait for the next.
fn round(
    host: &mut Host,
    failed: &mut Option<String>,
    msg: Option<Msg>,
    notify: &dyn Fn(Update),
) -> Option<Duration> {
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
        Some(Msg::PermissionAnswered(kind)) => host.permission_answered(kind, notify),
        Some(Msg::ReplayWritten(result)) => host.replay_written(result, notify),
        Some(Msg::Thumbnail {
            generation,
            adapter,
            target,
            picture,
        }) => {
            // A picture of a list that was closed or replaced meanwhile is dropped.
            if generation == host.thumbs.load(AtomicOrdering::Acquire) {
                notify(Update::Thumbnail {
                    adapter,
                    target,
                    picture,
                });
            }
        }
        Some(Msg::RegionPreview {
            generation,
            id,
            picture,
        }) => {
            if generation == host.region_grabs.load(AtomicOrdering::Acquire) {
                notify(Update::RegionPreview {
                    id,
                    generation,
                    picture,
                });
            }
        }
        Some(Msg::Calibration { id, msg }) => host.calibration_msg(id, msg),
        Some(Msg::AddOn { index, msg }) => host.addon_msg(index, msg, notify),
        Some(Msg::StingerLook { key, result }) => {
            if host.stinger_looked(key, result) {
                host.publish(notify);
            }
        }
        Some(Msg::TextMeasured { id, text, size }) => {
            if text_measure::apply(host, id, &text, size) {
                host.publish(notify);
            }
        }
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
    if failed.is_none() {
        host.sync_meters();
    }
    if failed.is_none()
        && let Some(why) = host.dead.take()
    {
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.end_outputs(&why, notify);
        notify(Update::Failed(why));
    }
    if failed.is_some() {
        // A run still meets its deadline while the studio is failed.
        let calibration = host.drive_calibration();
        return if served_api {
            Some(Duration::ZERO)
        } else {
            Some(calibration.unwrap_or(Duration::from_secs(3600)))
        };
    }
    host.feed_text_live();
    host.studio.service();
    let busy = host.waiting || host.held_switch.is_some();
    let cap = round_cap(&host.outs, busy);
    // An output or a switch is checked every round and has no deadline of its own.
    // The engine's wait is taken again after the last settle, not from this service.
    let poll = busy
        || host
            .outs
            .iter()
            .any(|o| o.recording.is_some() || o.streaming.is_some() || o.ending);
    let mut armed = poll;
    let mut wait = if poll { cap } else { Duration::MAX };
    if let Some(left) = host.studio.control_extra() {
        arm(&mut wait, &mut armed, left);
    }
    if served_api {
        // More may wait; look again right after any UI command.
        wait = Duration::ZERO;
        armed = true;
    }
    if let Some(why) = host.studio.failure() {
        // Clients see the recording end with the failure instead of a stale state.
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.end_outputs(&why, notify);
        notify(Update::Failed(why));
        return finish_wait(wait, armed);
    }
    // A replacement that waited for its predecessor opens now.
    let waiting = host.studio.waiting();
    if host.waiting && !waiting {
        host.apply(notify);
    }
    host.waiting = waiting;
    if let Some(left) = host.drive_crisp(notify) {
        arm(&mut wait, &mut armed, left);
    }
    if host.fonts_wanted {
        host.list_text_fonts(notify);
        if host.fonts_wanted {
            arm(&mut wait, &mut armed, text_resize::FONTS_POLL);
        }
    }
    if host.held_switch.is_some() {
        host.drive_held_switch(notify);
        if host.held_switch.is_some() {
            // Look again within a render tick.
            arm(&mut wait, &mut armed, HELD_SWITCH_POLL);
        }
    }
    // The latest T-bar value (ADR-0135 decision 1), once per round.
    if let Some(t) = host.pending_progress.take() {
        host.drive_progress(t, notify);
    }
    // A transition ended (ADR-0128 decision 8): one update without the outgoing scene
    // removes the incoming compositor and unlinks what only the outgoing scene used.
    if host.studio.take_transition_done() {
        host.studio.settle_transitions(&host.adapters.sources);
    }
    host.publish_transitions();
    // A rehearsal's time is over (ADR-0135 decision 12): the Preview picture shows the
    // preview scene again.
    if host.studio.rehearsal_done()
        && let Err(e) = host.studio.end_rehearsal(&host.adapters.sources)
    {
        notify(Update::Error(format!("The rehearsal could not end: {e}")));
    }
    // A held T-bar that ended other than by going to 0 (its take, a transition, a
    // switch or a preview change): the rail's T-bar goes back to 0.
    let manual = host.studio.manual_transition(host.canvas);
    if host.tbar_manual && !manual && !host.tbar_zeroed {
        notify(Update::TBarReset);
    }
    host.tbar_manual = manual;
    host.tbar_zeroed = false;
    // A hold scene, or its sound, fell back (ADR-0124 phase 2b): its parts leave and
    // the stream states say so.
    if host.studio.take_hold_fallback() {
        host.studio.settle_hold_scenes(&host.adapters.sources);
        for c in OutputCanvas::ALL {
            host.refresh_stream_hold(c);
        }
        notify(host.hold_update());
        host.publish(notify);
    }
    for c in OutputCanvas::ALL {
        let o = &host.outs[slot(c)];
        if let Some((_, id)) = &o.recording
            && let Some(why) = host.studio.output_failure(*id)
        {
            host.stop_recording(c, Some(why), notify);
        }
        let o = &host.outs[slot(c)];
        if o.ending
            && let Some((_, id)) = &o.recording
        {
            if host.studio.output_tail_done(*id) {
                host.stop_recording(c, None, notify);
            } else {
                // The tail takes a few frames; look again soon.
                arm(&mut wait, &mut armed, ENDING_WAIT);
            }
        }
        if host.outs[slot(c)].streaming.is_some() {
            if let Some(why) = host.check_destinations(c, notify) {
                host.end_stream(c, Some(why), notify);
            } else {
                host.report_link(c, notify);
                if let Some(left) = host.drive_hold(c) {
                    arm(&mut wait, &mut armed, left);
                }
            }
        }
    }
    let now = host.clock.now();
    if let Some(left) = host.report_stats(now, notify) {
        arm(&mut wait, &mut armed, left);
    }
    host.drive_log_summary(now, &mut wait, &mut armed);
    if let Some(left) = host.drive_protection(now, notify) {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.report_camera(now, notify) {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.drive_monitor(now) {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.drive_calibration() {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.drive_sync_measure(notify) {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.drive_marker_flush(now, notify) {
        arm(&mut wait, &mut armed, left);
    }
    if let Some(left) = host.report_media(notify) {
        arm(&mut wait, &mut armed, left);
    }
    host.report_monitor(notify);
    if let Some(left) = host.report_replay(now, notify) {
        arm(&mut wait, &mut armed, left);
    }
    // One wake for the soonest timed-show deadline. An empty cue map adds nothing (ADR-0105, P13).
    if host.studio.expire_cues(now) {
        host.apply(notify);
    }
    if let Some(left) = host.studio.cue_wait(now) {
        arm(&mut wait, &mut armed, left);
    }
    // A running transition steps on the master clock; none arms nothing (P13).
    if let Some(left) = host.studio.transition_wait(now) {
        arm(&mut wait, &mut armed, left);
    }
    // A rehearsal ends on the master clock; none arms nothing (P13).
    if let Some(left) = host.studio.rehearsal_wait(now) {
        arm(&mut wait, &mut armed, left);
    }
    // After every settle in this round, including one that timed out and left a swap
    // in flight. A bypass during that settle is in this wait, not in the earlier one.
    if let Some(left) = host.studio.collect_engine() {
        arm(&mut wait, &mut armed, left);
    }
    host.publish(notify);
    finish_wait(wait, armed)
}

/// Keeps `left` as the soonest deadline. `armed` stays false only while no driver and
/// no engine asked for a wake, so the control thread can block on the next message.
fn arm(wait: &mut Duration, armed: &mut bool, left: Duration) {
    *wait = (*wait).min(left);
    *armed = true;
}

fn finish_wait(wait: Duration, armed: bool) -> Option<Duration> {
    armed.then_some(wait)
}

/// The longest sleep after a round: [`MAX_WAIT`] while an output runs or ends on any
/// canvas, or a replacement or switch waits (`busy`), since those are checked each
/// round without a deadline of their own (failures, tails, links, holds); otherwise
/// [`IDLE_WAIT`].
fn round_cap(outs: &[CanvasOutputs], busy: bool) -> Duration {
    let running = outs
        .iter()
        .any(|o| o.recording.is_some() || o.streaming.is_some() || o.ending);
    if running || busy { MAX_WAIT } else { IDLE_WAIT }
}

fn start(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    levels: Arc<crate::levels::LevelFeed>,
) -> Result<(Host, Update), String> {
    let keys = Arc::clone(&config.keys);
    #[cfg(feature = "fault-injection")]
    let config_fault = config.fault;
    let (mut adapters, mut notes) = adapters();
    adapters.probe_io();
    let can_monitor = adapters.sources.can_monitor();
    let gpu = gpu()?;
    notes.push(format!("gpu: {}", gpu.adapter_name()));
    let loaded = config.collection.clone();
    let mut collection = config.collection.unwrap_or_default();
    let (canvas, scene) = shown_scene(&mut collection, config.size, config.fps)?;
    // The collection's canvas; `config.size` only chose the preview's aspect ratio.
    let (run_size, run_fps) = collection
        .canvas(canvas)
        .map_or((config.size, config.fps), |c| (c.size, c.frame_rate));
    // The runner ships with the app; the engine it loads is an add-on, resolved now
    // only when the collection has a browser source (ADR-0145 decision 6).
    // Signed-in profiles sit beside the add-ons, in the local data directory.
    let profiles = config
        .add_ons
        .as_ref()
        .and_then(|setup| crate::browser_profiles::dir_beside(&setup.root));
    let add_ons = add_ons::AddOnsHost::new(config.add_ons);
    let engine_slot = Arc::new(EngineSlot::default());
    if collection
        .sources()
        .any(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
    {
        engine_slot.set(add_ons.browser_engine());
    }
    let browser = seams::browser_tree(
        config.browser.as_ref(),
        Arc::clone(&engine_slot),
        profiles.clone(),
        &gpu,
    );
    let browser_tree: Option<Arc<dyn BrowserTree>> = Some(Arc::clone(&browser) as _);
    let mut settings = StudioSettings::new(canvas);
    // Width and height stay as configured. The ring follows the canvas that runs:
    // a saved collection can use another rate than the one the descriptor was built with.
    settings.preview = config.preview.map(|preview| PreviewDesc {
        surfaces: preview_surfaces(run_fps),
        ..preview
    });
    settings.scene_preview = SCENE_PICTURE;
    settings.audio_offsets = filters::audio_offsets(&collection);
    settings.media_pause_release = config.media_pause_release;
    let held = held_copy(&collection, canvas, None);
    let run = held.as_ref().and_then(|h| run_copy(&collection, canvas, h));
    let first = engine();
    let clock = first.clock();
    let studio = Studio::start_with_tree(
        first,
        Arc::clone(&gpu),
        run.as_ref().unwrap_or(&collection),
        &adapters.sources,
        &settings,
        browser_tree.clone(),
    )
    .map_err(|e| e.to_string())?;
    let display = seams::probe_display(config.main_display, &adapters.sources);
    let installed = adapters.sources.installed();
    let kinds: Vec<SourceKind> = SourceKind::ALL
        .into_iter()
        .filter(|k| installed.contains(&k.type_id()))
        .chain(
            [
                SourceKind::Image,
                SourceKind::Color,
                SourceKind::Media,
                SourceKind::Browser,
                SourceKind::Text,
            ]
            .into_iter()
            .filter(|k| installed.contains(&k.type_id())),
        )
        .collect();
    // Video and audio encoders are probed on first use (ADR-0154's neighbour, F-08):
    // output settings about to open, or a recording or stream about to start.
    let encoders = Vec::new();
    let cannot_record = None;
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
    let mut host = Host {
        studio,
        adapters,
        collection,
        collection_epoch: 0,
        list_stamp: None,
        canvas,
        scene,
        rows: Vec::new(),
        heard: Vec::new(),
        globals_shown: Vec::new(),
        into: None,
        held,
        held_was: None,
        held_id_was: None,
        scene_preview_sent: None,
        vertical_picture: false,
        vertical_preview_sent: None,
        projectors: Vec::new(),
        interacts: Vec::new(),
        scenes_shown: None,
        scene_answer: scene_cache::SceneAnswerCache::default(),
        vertical: None,
        vertical_restore_error: None,
        vertical_flags: VerticalFlags::default(),
        separate_files: true,
        limit_policy: (LimitPolicy::Ask, true),
        vertical_shared: Vec::new(),
        vertical_sent: None,
        master_sent: None,
        sizes: BTreeMap::new(),
        attribute_clipboard: None,
        history: History::new(),
        history_shown: (None, None),
        waiting: false,
        held_switch: None,
        ran_program: None,
        pending_progress: None,
        tbar_manual: false,
        tbar_zeroed: false,
        taking: false,
        text_elapsed: text_live::TextElapsed::default(),
        text_timer_file: config
            .config_dir
            .as_deref()
            .map(|d| {
                let (file, note) = crate::text_timers::load(d);
                if let Some(why) = note {
                    rbs_core::warn!(rbs_core::log::Area::App, "text timers were not read: {why}");
                }
                file
            })
            .unwrap_or_default(),
        text_timers_restored: false,
        crisp_pending: None,
        fonts_wanted: false,
        outs: [CanvasOutputs::default(), CanvasOutputs::default()],
        iso: None,
        stats_due: None,
        clock,
        camera_factory: config.virtual_camera.map(|f| f.0),
        camera: None,
        camera_due: None,
        monitor: monitor::MonitorHost::new(config.monitor, can_monitor),
        loudness_on: config.loudness.shown,
        loudness_settings: config.loudness,
        loudness_sent: None,
        camera_setup: None,
        camera_published: None,
        replay: replay::ReplayHost::new(),
        protection: protection::ProtectionHost::new(config.system_probe),
        keys,
        key_account: config.key_account,
        engine: Box::new(engine),
        gpu,
        browser_tree,
        browser,
        engine_slot,
        profiles_dir: profiles,
        preview_surfaces: settings.preview.map(|p| p.surfaces),
        dead: None,
        restarting: false,
        caught: false,
        unanswered: None,
        #[cfg(feature = "fault-injection")]
        fault: config_fault,
        autosave: config.autosave,
        recent: config
            .config_dir
            .as_deref()
            .map(|d| crate::recent_targets::load(d).0)
            .unwrap_or_default(),
        device_sync: config
            .config_dir
            .as_deref()
            .map(|d| crate::device_sync_file::load(d).0)
            .unwrap_or_default(),
        offered: None,
        measure: sync_measure::MeasureHost::default(),
        config_dir: config.config_dir,
        calibration: config.calibration,
        calibration_run: calibration::CalibrationHost::new(config.calibration_setup),
        disk_probe: config
            .read_free_space
            .map(|r| rbs_record::estimate::DiskProbe::new(r.0)),
        swap_after_transition: config.swap_after_transition,
        media_pause_release: config.media_pause_release,
        media_sent: None,
        thumbs: Arc::new(AtomicU64::new(0)),
        permission_asking: HashMap::new(),
        permission_asked: Vec::new(),
        permission_refused: Vec::new(),
        region_grabs: Arc::new(AtomicU64::new(0)),
        // A collection that had to be completed is saved at the first publish.
        saved: loaded,
        events: EventPublisher::new(),
        meters_ui: true,
        meters_applied: true,
        api: None,
        profile: RecordProfile::new(),
        kinds,
        encoders,
        encoders_probed: false,
        cannot_record,
        stream_profile: None,
        vertical_profile: None,
        vertical_stream_profile: None,
        destinations: Destinations::default(),
        destinations_unloaded: false,
        library: None,
        switch_pending: None,
        switch_seq: 0,
        inbox: Weak::new(),
        seams: seams::HostSeams::new(config.chapters, config.os_version),
        stinger: stinger::StingerFile::default(),
        add_ons,
        levels,
    };
    // A vertical canvas saved with the collection runs again (ADR-0093 decision 9).
    host.restore_vertical();
    // Show loudness is saved: the meter runs again from the start.
    host.restore_loudness();
    Ok((host, update))
}

/// The vertical canvas a saved collection holds: the first canvas besides `main`
/// (a studio runs at most two, and only `canvas.add_vertical` adds the second).
fn saved_vertical(collection: &SceneCollection, main: CanvasId) -> Option<VerticalState> {
    collection
        .canvases()
        .find(|c| c.id != main)
        .map(|c| VerticalState {
            canvas: c.id,
            size: c.size,
            follow: c.follow == Some(main),
        })
}

/// Capture region (has, l, t, r, b), chrome (on, t, r, b, l, fill), work area, and
/// timed show (ms, remaining) for a sources list row.
type RowExtra = (
    bool,
    u32,
    u32,
    u32,
    u32,
    bool,
    u32,
    u32,
    u32,
    u32,
    u8,
    bool,
    u32,
    u32,
);

impl Host {
    /// Runs a control API command for a window action that already has a method.
    /// The window and a remote client then share one implementation.
    fn run_ui_api(&mut self, command: rbs_api::Command, notify: &dyn Fn(Update)) -> bool {
        let result = rbs_crash::contain(|| ApiTarget { host: self, notify }.run(command));
        match result {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                notify(Update::Error(e.message));
                false
            }
            Err(_) => {
                self.caught = true;
                notify(Update::Error(rbs_crash::CAUGHT_TEXT.to_owned()));
                false
            }
        }
    }

    fn command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        // Every command by concern; each handler matches only its own variants.
        match c {
            Command::ListTargets(..)
            | Command::AddSourceWithAudio { .. }
            | Command::AddSource { .. }
            | Command::ReadPermissions
            | Command::ListAddOns
            | Command::AddOn { .. }
            | Command::RequestPermission(..)
            | Command::DeleteSource(..)
            | Command::RemoveFromScene(..)
            | Command::SetGlobal { .. }
            | Command::RemoveGlobal(..)
            | Command::AddGlobalSource { .. }
            | Command::UndoDelete
            | Command::Undo
            | Command::Redo
            | Command::MoveSource { .. }
            | Command::MoveSourceTo { .. }
            | Command::MoveScene { .. }
            | Command::MoveSceneTo { .. }
            | Command::SetSourceActive { .. }
            | Command::SetSourceLocked { .. }
            | Command::SetSourceVolume { .. }
            | Command::SetSourceMuted { .. }
            | Command::SetMetersUi(..)
            | Command::EnsureEncoders
            | Command::SetSceneOwnLevels { .. }
            | Command::ResetSceneLevel { .. }
            | Command::SetSourceMixerHidden { .. } => self.source_command(c, notify),
            Command::AddVerticalCanvas(..)
            | Command::RemoveVerticalCanvas
            | Command::AutoLayoutVertical(..)
            | Command::SetVerticalSize(..)
            | Command::SetMasterVolume { .. }
            | Command::SetMasterMuted { .. }
            | Command::SetLoudnessShown(..)
            | Command::ResetLoudness
            | Command::SetLoudnessSettings { .. }
            | Command::SetSourceMonitor { .. }
            | Command::SetMonitorSolo(..)
            | Command::SetMonitorDevice(..)
            | Command::SetMonitorVolume(..)
            | Command::ListMonitorDevices => self.canvas_and_mix_command(c, notify),
            Command::AddFilter { .. }
            | Command::AddVoiceChain(..)
            | Command::AddSample { .. }
            | Command::AddStamp { .. }
            | Command::AddImage { .. }
            | Command::AddMedia { .. }
            | Command::SetStingerFile(..)
            | Command::FitTextBox(..)
            | Command::ListTextFonts
            | Command::TextTimer { .. }
            | Command::AddText { .. }
            | Command::AddBrowser { .. }
            | Command::SetSourceFill { .. }
            | Command::SetSourceFilled { .. }
            | Command::ApplyLook { .. }
            | Command::RemoveFilter(..)
            | Command::MoveFilter { .. }
            | Command::SetFilterEnabled { .. }
            | Command::SetFilterValues { .. }
            | Command::PreviewFilter { .. }
            | Command::RestoreFilter(..)
            | Command::OpenSyncMeasure(..)
            | Command::StartSyncMeasure(..)
            | Command::CloseSyncMeasure
            | Command::ApplySyncToDeviceSources(..)
            | Command::SetSourceSyncOffset { .. }
            | Command::RetargetSource { .. }
            | Command::GrabThumbnails { .. }
            | Command::StopThumbnails
            | Command::OpenSourceSettings(..)
            | Command::CloseSourceSettings
            | Command::SetSourceSettings { .. }
            | Command::Media { .. }
            | Command::ReloadSource(..)
            | Command::ForgetBrowserSignIn(..)
            | Command::BrowserControl { .. }
            | Command::SetCompanionAudio { .. } => self.source_edit_command(c, notify),
            Command::RenameSource { .. }
            | Command::SetGlide { .. }
            | Command::SetTransition(..)
            | Command::SetSourceActivity { .. }
            | Command::SetSourceGlide { .. }
            | Command::AddExistingSource(..)
            | Command::AddScene(..)
            | Command::DuplicateScene(..)
            | Command::DuplicateSceneWithCopies(..)
            | Command::DuplicateSource(..)
            | Command::CopyAttributes(..)
            | Command::OpenAttributes { .. }
            | Command::ResetSource { .. }
            | Command::PasteAttributes { .. }
            | Command::RemoveScene(..)
            | Command::RenameScene { .. }
            | Command::SetSceneTransition { .. }
            | Command::SwitchScene(..)
            | Command::SetStudioMode(..)
            | Command::Transition
            | Command::TransitionProgress(..)
            | Command::Rehearse
            | Command::SetSwapAfterTransition(..) => self.scene_command(c, notify),
            Command::SetSourceVisible { .. }
            | Command::SetSourceBox { .. }
            | Command::SetSourceCrop { .. }
            | Command::TransformSource { .. } => self.item_command(c, notify),
            Command::SetSourceRegion { .. }
            | Command::SetRecordSeparately { .. }
            | Command::SetSourceCaptureRegion { .. }
            | Command::SetDisplayChrome { .. }
            | Command::FitDisplayChrome(..)
            | Command::ResetDisplayChrome(..)
            | Command::SetItemShowFor { .. }
            | Command::ShowItem(..)
            | Command::SetSourceStyle { .. }
            | Command::SetSourceScroll { .. }
            | Command::ScrollLive { .. } => self.item_display_command(c, notify),
            Command::StartRecording(..)
            | Command::StartVerticalRecording(..)
            | Command::StopRecording
            | Command::StopVerticalRecording
            | Command::SetVerticalProfile(..)
            | Command::SetVerticalStreamProfile(..)
            | Command::SetStreamDestinations(..)
            | Command::SetVerticalFlags(..)
            | Command::SetSeparateFiles(..)
            | Command::SetVerticalPicture(..)
            | Command::OpenProjector { .. }
            | Command::OpenInteract(..)
            | Command::CloseInteract(..)
            | Command::InteractInput { .. }
            | Command::CloseProjector(..)
            | Command::SetCanvas { .. }
            | Command::SetProfile(..)
            | Command::StartStream(..)
            | Command::StartVerticalStream(..)
            | Command::EndStream
            | Command::EndVerticalStream
            | Command::StartVirtualCamera { .. }
            | Command::StopVirtualCamera
            | Command::SetReplay(..)
            | Command::ArmReplay
            | Command::DisarmReplay
            | Command::AddMarker
            | Command::RemoveMarkers(..) => self.output_command(c, notify),
            Command::PauseRecording
            | Command::HoldStream
            | Command::Break
            | Command::SaveReplay { .. }
            | Command::SetVirtualCamera(..)
            | Command::SetStreamProfile(..)
            | Command::SaveStreamKey(..)
            | Command::SaveVerticalStreamKey(..)
            | Command::StartDestination(..)
            | Command::StopDestination(..)
            | Command::SaveDestinationKey(..)
            | Command::SetKeyAccount(..)
            | Command::SaveImportedKey(..)
            | Command::RemoveStreamKey(..)
            | Command::SwitchCollection { .. }
            | Command::SetLibrary { .. } => self.stream_command(c, notify),
        }
    }

    /// What a kind can capture, in OS order, then one entry per OS picker.
    fn targets(&self, kind: SourceKind) -> Vec<Target> {
        let raw = self
            .adapters
            .sources
            .targets(&kind.type_id())
            .unwrap_or_default();
        // Pairing a camera with its microphone lists audio inputs; it runs only for
        // cameras, while a list is asked for (P13).
        let mics = if kind == SourceKind::Camera {
            self.unused_microphones()
        } else {
            Vec::new()
        };
        let can_exclude = self.adapters.sources.can_exclude_own_sound();
        let mut list: Vec<Target> = raw
            .into_iter()
            .map(|t| {
                let mic = t
                    .related_audio
                    .as_ref()
                    .and_then(|id| mics.iter().find(|m| &m.target == id))
                    .map(|m| m.name.clone());
                let sound = kind != SourceKind::Screen || can_exclude;
                Target {
                    mic,
                    sound,
                    ..target_of(t)
                }
            })
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
            app: None,
            recent: None,
            picker: true,
            mic: None,
            sound: false,
        }));
        for t in &mut list {
            t.recent = recent_rank(self.recent.list(kind.type_id().as_str()), t);
        }
        list
    }

    /// The audio inputs that are not a source yet.
    fn unused_microphones(&self) -> Vec<rbs_sources::SourceTarget> {
        let used: Vec<&str> = self
            .collection
            .sources()
            .filter(|s| s.type_id == SourceKind::AudioInput.type_id())
            .filter_map(|s| match s.settings.get(keys::TARGET) {
                Some(SettingValue::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        self.adapters
            .sources
            .targets(&SourceKind::AudioInput.type_id())
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !used.contains(&m.target.as_str()))
            .collect()
    }

    /// The microphone that belongs with the camera `picture` shows, when the audio
    /// system has it and it is not a source yet.
    fn camera_microphone(&self, picture: &rbs_scene::model::Source) -> Option<Target> {
        let (adapter, id) = match (
            picture.settings.get(keys::ADAPTER),
            picture.settings.get(keys::TARGET),
        ) {
            (Some(SettingValue::Text(a)), Some(SettingValue::Text(t))) => (a.as_str(), t.as_str()),
            _ => return None,
        };
        let related = self
            .adapters
            .sources
            .targets(&SourceKind::Camera.type_id())
            .unwrap_or_default()
            .into_iter()
            .find(|t| t.adapter.as_str() == adapter && t.target == id)?
            .related_audio?;
        let mic = self
            .unused_microphones()
            .into_iter()
            .find(|m| m.target == related)?;
        Some(target_of(mic))
    }

    /// Grabs the pictures of `rows` on up to two threads, each target once, and posts
    /// them as they come. Nothing runs when the list is closed: a newer generation
    /// ends the workers at their next row (P13).
    /// One Region-section grab for Source settings (ADR-0103). Off the control thread.
    fn grab_region_preview(&self, id: SourceId) {
        let Some(source) = self.collection.source(id) else {
            return;
        };
        let Some(kind) = SourceKind::of(&source.type_id) else {
            return;
        };
        if !matches!(
            kind,
            SourceKind::Screen | SourceKind::Window | SourceKind::Camera
        ) {
            return;
        }
        let adapter = match source.settings.get(keys::ADAPTER) {
            Some(SettingValue::Text(text)) => text.clone(),
            _ => return,
        };
        let target = match source.settings.get(keys::TARGET) {
            Some(SettingValue::Text(text)) => text.clone(),
            _ => return,
        };
        let type_id = kind.type_id();
        let adapter_id = rbs_core::registry::AdapterId::new(adapter);
        let Some(factory) = self.adapters.sources.capture_factory(&type_id, &adapter_id) else {
            return;
        };
        let generation = self.region_grabs.fetch_add(1, AtomicOrdering::AcqRel) + 1;
        let current = Arc::clone(&self.region_grabs);
        let inbox = Weak::clone(&self.inbox);
        let _ = std::thread::Builder::new()
            .name("rbs-region-grab".into())
            .spawn(move || {
                if current.load(AtomicOrdering::Acquire) != generation {
                    return;
                }
                let picture = factory
                    .region_grab(
                        &rbs_capture::TargetId::new(target),
                        rbs_capture::REGION_GRAB_LONG,
                    )
                    .ok();
                let Some(tx) = inbox.upgrade() else {
                    return;
                };
                let _ = tx.send(Msg::RegionPreview {
                    generation,
                    id,
                    picture,
                });
            });
    }

    fn grab_thumbnails(&self, kind: SourceKind, rows: Vec<(String, String)>, max: (u32, u32)) {
        let generation = self.thumbs.fetch_add(1, AtomicOrdering::AcqRel) + 1;
        let type_id = kind.type_id();
        let size = rbs_core::media::Size {
            width: max.0,
            height: max.1,
        };
        let queue = Arc::new(Mutex::new(
            rows.into_iter()
                .filter_map(|(adapter, target)| {
                    let id = rbs_core::registry::AdapterId::new(adapter.clone());
                    let factory = self.adapters.sources.capture_factory(&type_id, &id)?;
                    Some((adapter, target, factory))
                })
                .collect::<std::collections::VecDeque<_>>(),
        ));
        for n in 0..2 {
            let (queue, current, inbox) = (
                Arc::clone(&queue),
                Arc::clone(&self.thumbs),
                Weak::clone(&self.inbox),
            );
            let spawned = std::thread::Builder::new()
                .name(format!("rbs-thumbnail-{n}"))
                .spawn(move || {
                    while current.load(AtomicOrdering::Acquire) == generation {
                        let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                        let Some((adapter, target, factory)) = next else {
                            break;
                        };
                        let picture = factory
                            .thumbnail(&rbs_capture::TargetId::new(target.clone()), size)
                            .ok();
                        let Some(tx) = inbox.upgrade() else { break };
                        let _ = tx.send(Msg::Thumbnail {
                            generation,
                            adapter,
                            target,
                            picture,
                        });
                    }
                });
            if spawned.is_err() {
                break;
            }
        }
    }

    /// The permission kind to ask for before adding a source of `kind`: one that was
    /// not asked yet and can show a prompt, at most once per run (ADR-0141 decision 4).
    fn prompt_first(&self, kind: SourceKind) -> Option<PermissionKind> {
        let p = PermissionKind::of_type(&kind.type_id())?;
        if self.permission_asked.contains(&p) {
            return None;
        }
        let info = self.adapters.sources.permission(p);
        (info.status == PermissionStatus::NotAsked && info.can_prompt).then_some(p)
    }

    /// Every kind with an installed adapter and its status.
    fn permission_rows(&self) -> Vec<(PermissionKind, PermissionInfo)> {
        PermissionKind::ALL
            .into_iter()
            .map(|k| (k, self.adapters.sources.permission(k)))
            .filter(|(_, info)| info.status != PermissionStatus::Unavailable)
            .collect()
    }

    fn send_permissions(&self, notify: &dyn Fn(Update)) {
        notify(Update::Permissions {
            rows: self.permission_rows(),
            waiting: self.permission_asking.keys().next().copied(),
        });
    }

    /// Shows the OS prompt for `kind` on a one-shot worker; `then` runs when it is
    /// answered. A second request while one shows waits for the same answer.
    fn ask_permission(
        &mut self,
        kind: PermissionKind,
        then: Option<Command>,
        notify: &dyn Fn(Update),
    ) {
        if let Some(waiting) = self.permission_asking.get_mut(&kind) {
            waiting.extend(then);
            return;
        }
        if !self.permission_asked.contains(&kind) {
            self.permission_asked.push(kind);
        }
        let request = self.adapters.sources.permission_request(kind);
        if request.is_empty() {
            if let Some(c) = then {
                self.post(Msg::Ui(c));
            }
            self.send_permissions(notify);
            return;
        }
        self.permission_asking
            .insert(kind, then.into_iter().collect());
        self.send_permissions(notify);
        let inbox = Weak::clone(&self.inbox);
        // Blocks until the user answers; detached, so quitting never waits for it.
        let spawned = std::thread::Builder::new()
            .name("rbs-permission".into())
            .spawn(move || {
                request.run();
                if let Some(tx) = inbox.upgrade() {
                    let _ = tx.send(Msg::PermissionAnswered(kind));
                }
            });
        if spawned.is_err() {
            self.post(Msg::PermissionAnswered(kind));
        }
    }

    /// A prompt was answered: run what waited for it, reopen sources of the kind that
    /// were refused when it is allowed now (not for kinds that apply after a restart),
    /// and tell the UI.
    fn permission_answered(&mut self, kind: PermissionKind, notify: &dyn Fn(Update)) {
        let waiting = self.permission_asking.remove(&kind).unwrap_or_default();
        let info = self.adapters.sources.permission(kind);
        if info.status == PermissionStatus::Allowed && !info.restart {
            let refused: Vec<SourceId> = self
                .studio
                .sources()
                .iter()
                .filter(|st| st.fault == Some(SourceFault::PermissionDenied))
                .map(|st| st.source)
                .filter(|id| {
                    self.collection
                        .source(*id)
                        .and_then(|s| PermissionKind::of_type(&s.type_id))
                        == Some(kind)
                })
                .collect();
            let reopen = !refused.is_empty();
            for id in refused {
                let _ = self.reload_source(id);
            }
            if reopen {
                self.apply(notify);
            }
        }
        for c in waiting {
            self.post(Msg::Ui(c));
        }
        self.send_permissions(notify);
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
        self.heard.clear();
        self.scene_preview_sent = None;
        self.vertical_preview_sent = None;
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

    /// The main canvas in pixels, for an output size fitted to it (ADR-0102).
    fn main_canvas_size(&self) -> (u32, u32) {
        let canvas = self.api_canvas();
        (canvas.width, canvas.height)
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
            vertical: Some(self.vertical_flags.to_api()),
            loudness: Some(api::LoudnessSettings {
                target: self.loudness_settings.target,
                true_peak_limit: self.loudness_settings.true_peak_limit,
            }),
        }
    }

    fn settings_changed(&mut self) {
        let settings = self.api_settings();
        self.events
            .changed(StateKey::topic(Topic::SettingsChanged), || {
                EventPayload::SettingsChanged(settings)
            });
    }

    /// Puts meters on the mixer while the Mixer is shown or a client samples levels.
    fn sync_meters(&mut self) {
        let on = self.meters_ui || self.events.subscribed(rbs_api::Topic::AudioLevels);
        if on == self.meters_applied {
            return;
        }
        self.studio.set_meters_on(on);
        self.meters_applied = on;
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

    /// `item`'s box as the studio's canvas draws it now (ADR-0110).
    fn shown_layout(&self, item: &SceneItem) -> Layout {
        self.collection
            .placed_layout(self.canvas, item.layout_on(self.canvas))
    }

    /// `live`, which is in the studio canvas's current pixels, in the layout space
    /// the document stores (ADR-0110).
    fn stored_layout(&self, live: &Layout) -> Layout {
        self.collection.stored_layout(self.canvas, live)
    }

    /// Stores `live` (current canvas pixels) as the item's layout. One undo record.
    fn save_live_layout(
        &mut self,
        id: ItemId,
        canvas: Option<CanvasId>,
        live: Layout,
    ) -> Result<Undo, SceneError> {
        // An own layout is stored in its canvas's space, the default in the studio's.
        let space = canvas.unwrap_or(self.canvas);
        let stored = self.collection.stored_layout(space, &live);
        self.collection.set_item_layout(id, canvas, stored)
    }

    /// Restarts the studio with the canvas at `size` and `fps`. Stored item boxes stay
    /// in the canvas's layout space and are fitted when drawn (ADR-0110). When the new
    /// canvas does not start, the old one starts again and the error says why; when
    /// that fails too, `dead` says why.
    fn set_canvas(&mut self, size: Size, fps: FrameRate) -> Result<(), String> {
        if self.any_recording() {
            return Err("Stop the recording before changing the canvas.".into());
        }
        if self.any_streaming() {
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
        // Layouts stay in the canvas's layout space (ADR-0110). Flattening fits them
        // into `size`; returning to the layout size shows the stored boxes again.
        self.restarting = true;
        let _ = self.studio.shutdown();
        let restarted = self.restart();
        // Still restarting: a panic here leaves the studio dead (ADR-0055 decision 3).
        inject!(self, Fault::DuringCanvasRestart);
        self.restarting = false;
        match restarted {
            Ok(()) => {
                // A canvas change is not an undo step (control API). The layout space is
                // unchanged, so earlier box edits stay valid; they are dropped with the
                // restart so undo cannot cross it.
                self.history.clear();
                Ok(())
            }
            Err(why) => {
                self.note_collection();
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
            let why = self
                .dead
                .get_or_insert_with(|| {
                    "An internal error stopped the studio while it restarted.".into()
                })
                .clone();
            // The round that panicked does not reach its own dead check, and the next
            // round may not run until something wakes the thread. Say it failed now.
            if failed.is_none() {
                *failed = Some(why.clone());
                notify(Update::Failed(why));
            }
        }
        let running = self.studio.outputs();
        let tracked = |o: &CanvasOutputs| {
            (
                o.recording.as_ref().map(|(_, id)| *id),
                o.streaming.as_ref().map(|(_, _, id)| *id),
            )
        };
        let mut stopped = 0;
        for id in &running {
            let known = self.outs.iter().any(|o| {
                let (r, s) = tracked(o);
                r == Some(*id)
                    || s == Some(*id)
                    || o.destinations.iter().any(|d| d.output == Some(*id))
            });
            if !known {
                let _ = rbs_crash::contain(|| self.studio.stop_output(*id));
                stopped += 1;
            }
        }
        // Separate files without their master: a start that panicked half-way, or a
        // master that is no longer tracked.
        if self.outs[slot(OutputCanvas::Main)].recording.is_none() {
            self.iso = None;
        }
        if self.iso.is_none() && self.studio.iso_video_running() {
            let _ = rbs_crash::contain(|| self.studio.stop_iso_video());
        }
        if self.iso.is_none() && self.studio.iso_audio_running() {
            let _ = rbs_crash::contain(|| self.studio.stop_iso_audio());
        }
        #[cfg(feature = "fault-injection")]
        notify(Update::Recovered { stopped });
        let _ = stopped;
        for c in OutputCanvas::ALL {
            let (recording, streaming) = tracked(&self.outs[slot(c)]);
            if recording.is_some_and(|id| !running.contains(&id)) {
                let _ = rbs_crash::contain(|| self.stop_recording(c, Some(WHY.into()), notify));
            }
            if streaming.is_some_and(|id| !running.contains(&id)) {
                let _ = rbs_crash::contain(|| self.end_stream(c, Some(WHY.into()), notify));
            }
        }
        if self.camera.is_some() != self.studio.virtual_camera().is_some() {
            self.studio.stop_virtual_camera();
            self.camera = None;
            notify(Update::VirtualCamera(None));
            self.camera_changed();
        }
        if in_row >= CAUGHT_LIMIT && failed.is_none() {
            let why = String::from(
                "RBS stopped the studio after repeated internal errors (reports were saved).",
            );
            *failed = Some(why.clone());
            for c in OutputCanvas::ALL {
                let _ = rbs_crash::contain(|| self.stop_recording(c, Some(why.clone()), notify));
                let _ = rbs_crash::contain(|| self.end_stream(c, Some(why.clone()), notify));
            }
            for id in self.studio.outputs() {
                let _ = rbs_crash::contain(|| self.studio.stop_output(id));
            }
            for o in &mut self.outs {
                o.recording = None;
                o.streaming = None;
                o.destinations.clear();
            }
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
        let preview = self
            .preview_surfaces
            .map(|_| preview_desc(canvas.size, canvas.frame_rate));
        self.preview_surfaces = preview.map(|desc| desc.surfaces);
        settings.preview = preview;
        settings.scene_preview = SCENE_PICTURE;
        settings.audio_offsets = filters::audio_offsets(&self.collection);
        settings.media_pause_release = self.media_pause_release;
        if let Some(v) = self
            .vertical
            .filter(|v| self.collection.canvas(v.canvas).is_some())
        {
            settings.extra_canvases = vec![self.vertical_extra(v.canvas)];
        } else if self.vertical.take().is_some() {
            self.forget_vertical_outputs();
        }
        let run = self.run_collection();
        // Tickers go on where they were (ADR-0150 decision 5).
        settings.scroll_runs = Some(self.studio.scroll_handover());
        // Timers and elapsed times run on across the restart (ADR-0129 decision 11).
        let timers = self.studio.text_timers();
        let old_now = self.clock.now();
        let engine = (self.engine)();
        let clock = engine.clock();
        let studio = Studio::start_with_tree(
            engine,
            Arc::clone(&self.gpu),
            run.as_ref().unwrap_or(&self.collection),
            &self.adapters.sources,
            &settings,
            self.browser_tree.clone(),
        )
        .map_err(|e| e.to_string())?;
        self.studio = studio;
        self.clock = clock;
        self.studio.set_text_timers(&timers);
        self.text_elapsed = self.text_elapsed.moved(old_now, self.clock.now());
        self.waiting = self.studio.waiting();
        // Pictures belonged to the old studio; the next publish adds them again with
        // the canvas's size now (ADR-0125 decision 3).
        for p in &mut self.projectors {
            p.picture = None;
        }
        // The camera keeps sending across a canvas change; when it cannot, the next
        // round says so.
        if let Some(settings) = self.camera.take()
            && self.start_camera(settings).is_err()
        {
            self.camera_due = Some(self.clock.now());
        }
        self.camera_changed();
        // A collection opened with a saved vertical canvas runs it again (ADR-0093
        // decision 9).
        self.restore_vertical();
        // The replay buffer keeps its place: it is armed again in the new studio.
        self.rearm_replay(&|_| {});
        // The loudness meter measures again, from scratch, in the new studio.
        self.restore_loudness();
        rbs_core::info!(rbs_core::log::Area::Studio, "the studio restarted");
        Ok(())
    }

    /// A recording runs on either canvas.
    fn any_recording(&self) -> bool {
        self.outs.iter().any(|o| o.recording.is_some())
    }

    /// The program scene canvas `c` shows: the main canvas's, which a following
    /// vertical canvas shows too, or the vertical canvas's own.
    fn program_for(&self, c: OutputCanvas) -> Option<SceneId> {
        match c {
            OutputCanvas::Main => Some(self.scene),
            OutputCanvas::Vertical => {
                let v = self.vertical.as_ref()?;
                if v.follow {
                    Some(self.scene)
                } else {
                    self.collection.canvas(v.canvas).and_then(|c| c.program)
                }
            }
        }
    }

    /// The `seq` map of `c`'s key on a per-canvas topic.
    fn canvas_seq(&self, topic: Topic, c: OutputCanvas) -> api::SeqMap {
        let key = canvas_key(topic, c);
        api::SeqMap::from([(key.to_string(), self.events.seq(key))])
    }

    // ----- global sources and safe removal (ADR-0065) -----

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
        let program = self.scene;
        let preview = self.preview();
        let offsets = filters::audio_offsets(&self.collection);
        let boxes = self.recorded_text_boxes();
        let mut result = self.undo_or_redo_records(back);
        if result.is_ok() && self.recorded_text_boxes() != boxes {
            // The step would change the box of a text with a separate picture track
            // in the running take (ADR-0129 decision 5): stepped back, refused.
            let _ = self.undo_or_redo_records(!back);
            result = Err((false, STOP_TO_CHANGE_BOX.to_owned()));
        }
        self.resync_audio_offsets(&offsets);
        // Undo and redo never change what is on air (ADR-0059) or the preview scene
        // (ADR-0061): a record that put a scene back on air or in preview is
        // overruled, and a removed program is replaced.
        let now = self.collection.canvas(self.canvas).and_then(|c| c.program);
        if now != Some(program) && self.collection.scene(program).is_some() {
            let _ = self.collection.set_program(self.canvas, Some(program));
        }
        self.keep_program();
        let kept = preview.filter(|p| self.collection.scene(*p).is_some());
        let restored = match (preview, kept) {
            (None, _) => None,
            (Some(_), Some(p)) => Some(p),
            // The preview scene went away: studio mode stays, with the program.
            (Some(_), None) => Some(self.scene),
        };
        if self.preview() != restored {
            let _ = self.collection.set_preview(self.canvas, restored);
        }
        result
    }

    /// The box of every text source a running take writes a separate picture track
    /// of; empty while no take records one.
    fn recorded_text_boxes(&self) -> Vec<(SourceId, Option<SettingValue>, Option<SettingValue>)> {
        if !self.studio.iso_video_running() {
            return Vec::new();
        }
        self.collection
            .sources()
            .filter(|s| {
                rbs_sources::types::class_of(&s.type_id)
                    == Some(rbs_sources::types::SourceClass::Text)
                    && self.studio.iso_video_records(s.id)
            })
            .map(|s| {
                let get = |k: &str| s.settings.get(k).cloned();
                (
                    s.id,
                    get(rbs_sources::types::keys::WIDTH),
                    get(rbs_sources::types::keys::HEIGHT),
                )
            })
            .collect()
    }

    fn undo_or_redo_records(&mut self, back: bool) -> Result<(), (bool, String)> {
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

    /// The scene that holds an item.
    fn scene_of(&self, item: ItemId) -> Option<SceneId> {
        self.collection.item(item).map(|(s, _)| s)
    }

    /// Runs the edited collection.
    /// Runs the studio with a change, or says why it was refused.
    fn answer(&mut self, done: Result<bool, Refusal>, notify: &dyn Fn(Update)) {
        match done {
            Ok(true) => self.apply(notify),
            Ok(false) => {}
            Err(e) => {
                notify(Update::Error(e.text().to_owned()));
                // A switch made before a refused removal is still news.
                self.publish(notify);
            }
        }
    }

    /// The window's answer to a command run through its control API composition: a
    /// refusal shows as the error bar (the composition published what changed).
    fn answer_api(&mut self, done: Result<rbs_api::Response, ApiError>, notify: &dyn Fn(Update)) {
        if let Err(e) = done {
            notify(Update::Error(e.message));
        }
    }

    /// Like [`Host::answer`] for volume and mute, which reached the mixer already.
    fn answer_live(&mut self, done: Result<bool, Refusal>, notify: &dyn Fn(Update)) {
        match done {
            Ok(true) => self.publish(notify),
            Ok(false) => {}
            Err(e) => notify(Update::Error(e.text().to_owned())),
        }
    }

    fn apply(&mut self, notify: &dyn Fn(Update)) {
        self.prune_monitor_solo();
        self.mark_scene_changes();
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        let program = running
            .canvas(self.canvas)
            .and_then(|c| c.program)
            .map(|id| {
                let real = match &self.held {
                    Some(h) if h.scene.id == id => h.from,
                    _ => id,
                };
                (id, real)
            });
        let same = same_picture(self.ran_program, program);
        self.ran_program = program;
        let updated = if same {
            self.studio
                .update_same_picture(running, &self.adapters.sources)
        } else {
            self.studio.update(running, &self.adapters.sources)
        };
        match updated {
            Ok(u) => {
                self.waiting = self.studio.waiting();
                // Source failures show in the list; other failures need words.
                for f in u.failed.iter().filter(|f| f.node != "source") {
                    notify(Update::Error(format!("{}: {}", f.node, f.reason)));
                }
            }
            Err(e) => notify(Update::Error(e.to_string())),
        }
        self.publish_transitions();
        self.publish_scroll_finished();
        self.look_at_stinger();
        self.publish(notify);
    }

    /// Publishes the transitions' starts and ends since the last call on
    /// `transition.state` (ADR-0128 decision 13), one key per canvas. Like every state
    /// topic the slot keeps the latest: a cut's start and end in one update reach a
    /// slow client as `ended` with a `seq` that skipped the start.
    fn publish_scroll_finished(&mut self) {
        for e in self.studio.take_scroll_finished() {
            let canvas = if e.canvas == self.canvas {
                OutputCanvas::Main
            } else if self.vertical.map(|v| v.canvas) == Some(e.canvas) {
                OutputCanvas::Vertical
            } else {
                continue;
            };
            let finished = api::ScrollFinished {
                canvas,
                items: e.items.iter().map(|i| i.get()).collect(),
            };
            self.events
                .changed(canvas_key(Topic::ItemScrollFinished, canvas), || {
                    EventPayload::ItemScrollFinished(finished)
                });
        }
    }

    /// Sends the rows and the undo history when they changed, and saves the collection
    /// when it changed (every edit, undo, canvas change or control API change passes
    /// here; the writer thread keeps only the latest snapshot, so a drag costs one
    /// write behind it, never a wait here).
    fn publish(&mut self, notify: &dyn Fn(Update)) {
        self.sync_projectors(notify);
        self.sync_interacts(notify);
        self.publish_save();
        self.refresh_sizes();
        if self.vertical_sent != Some(self.vertical) {
            self.vertical_sent = Some(self.vertical);
            notify(Update::Vertical(self.vertical));
        }
        if let Some(why) = self.vertical_restore_error.take() {
            notify(Update::Error(why));
        }
        let master = self.studio.master_meter().cloned();
        let same = match (&self.master_sent, &master) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.master_sent = master.clone();
            notify(Update::MasterMeter(master));
        }
        let loudness = self.loudness_view();
        if self.loudness_sent.as_ref() != Some(&loudness) {
            self.loudness_sent = Some(loudness.clone());
            notify(Update::Loudness(loudness));
        }
        let stamp = self.compute_list_stamp();
        if self.list_stamp != Some(stamp) {
            self.list_stamp = Some(stamp);
            let scenes = self.scene_list();
            if self.scenes_shown.as_ref() != Some(&scenes) {
                self.scenes_shown = Some(scenes.clone());
                let listed = api_scenes(&scenes);
                self.scene_answer
                    .store(scene_cache::SceneAnswerKey::of(self), listed.clone());
                self.events
                    .changed(StateKey::topic(Topic::SceneChanged), move || {
                        EventPayload::SceneChanged(listed)
                    });
                notify(Update::Scenes(scenes));
            }
            // `source.changed` and the Mixer: the program scene (ADR-0061 decision 8).
            let heard = self.rows_of(self.scene);
            let globals = self.global_rows();
            if heard != self.heard || globals != self.globals_shown {
                if heard != self.heard {
                    let listed: Vec<api::SourceInfo> = heard.iter().map(api_source).collect();
                    self.events
                        .changed(StateKey::topic(Topic::SourceChanged), || {
                            EventPayload::SourceChanged(listed)
                        });
                }
                self.heard = heard.clone();
                self.globals_shown = globals.clone();
                // The Mixer hears the global sources with the program (ADR-0065).
                let mut mixed = globals;
                mixed.extend(heard);
                notify(Update::Heard(mixed));
            }
            let rows = self.rows();
            if rows != self.rows {
                let mut refused: Vec<PermissionKind> = rows
                    .iter()
                    .filter_map(|r| match r.status {
                        SourceStatus::Unavailable {
                            fault: SourceFault::PermissionDenied,
                            ..
                        } => PermissionKind::of_type(&r.kind?.type_id()),
                        _ => None,
                    })
                    .collect();
                refused.sort_by_key(|k| PermissionKind::ALL.iter().position(|a| a == k));
                refused.dedup();
                if refused != self.permission_refused {
                    if refused.iter().any(|k| !self.permission_refused.contains(k)) {
                        self.send_permissions(notify);
                    }
                    self.permission_refused = refused;
                }
                self.rows = rows.clone();
                notify(Update::Sources(rows));
            }
        }
        let picture = self.studio.scene_preview();
        if picture != self.scene_preview_sent {
            self.scene_preview_sent = picture.clone();
            notify(Update::ScenePreview(picture));
        }
        let vertical = self
            .vertical
            .and_then(|v| self.studio.canvas_preview(v.canvas));
        if vertical != self.vertical_preview_sent {
            self.vertical_preview_sent = vertical.clone();
            notify(Update::VerticalPreview(vertical));
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
        self.publish_levels();
    }

    fn note_collection(&mut self) {
        self.collection_epoch = self.collection_epoch.wrapping_add(1);
    }

    /// Whether the scene list and the source rows still match what was published.
    /// Cue time is included, so a running Show cue still rebuilds. Document edits
    /// bump [`SceneCollection::generation`]; replacing the collection bumps the epoch.
    fn compute_list_stamp(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.collection_epoch.hash(&mut hasher);
        self.collection.generation().hash(&mut hasher);
        self.scene.hash(&mut hasher);
        self.preview().hash(&mut hasher);
        self.edited().hash(&mut hasher);
        self.vertical.map(|v| v.canvas).hash(&mut hasher);
        self.stinger.cut_found_ms().hash(&mut hasher);
        self.stinger.looking().hash(&mut hasher);
        self.stinger.alpha_unsupported().hash(&mut hasher);
        self.stinger_length()
            .map(|l| l.as_millis() as u64)
            .hash(&mut hasher);
        let now = self.clock.now();
        let states = self.studio.sources();
        for source in self.collection.sources() {
            source.id.hash(&mut hasher);
            let state = states.iter().find(|st| st.source == source.id);
            (state.is_some(), state.and_then(|st| st.failed.as_deref())).hash(&mut hasher);
            state
                .and_then(|st| st.fault)
                .map(|f| f as u8)
                .hash(&mut hasher);
            seams::page_reason(&self.studio, source.id, state).hash(&mut hasher);
            let latency = self.studio.audio_latency(source.id);
            latency.map(|l| l.filter_latency_ns).hash(&mut hasher);
            latency.map(|l| l.heard_offset_ns).hash(&mut hasher);
            latency.map(|l| l.uncompensated_ns).hash(&mut hasher);
            self.studio
                .meter(source.id)
                .map(Arc::as_ptr)
                .map(|p| p as usize)
                .hash(&mut hasher);
            self.sizes.get(&source.id).hash(&mut hasher);
            for filter in &source.filters {
                filter.id.hash(&mut hasher);
                self.studio
                    .audio_filter_latency_ns(filter.id)
                    .hash(&mut hasher);
                self.studio
                    .audio_filter_meter(filter.id)
                    .map(Arc::as_ptr)
                    .map(|p| p as usize)
                    .hash(&mut hasher);
            }
            for (_, name) in self.device_peers(source.id) {
                name.hash(&mut hasher);
            }
        }
        for scene_id in [Some(self.scene), self.preview(), Some(self.edited())]
            .into_iter()
            .flatten()
        {
            let Some(scene) = self.collection.scene(scene_id) else {
                continue;
            };
            for item in &scene.items {
                item.id.hash(&mut hasher);
                item.visible.hash(&mut hasher);
                item.locked.hash(&mut hasher);
                item.show_for_ms.hash(&mut hasher);
                self.studio.cue_remaining_ms(item.id, now).hash(&mut hasher);
                self.scroll_paused_of(item).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// The running audio sources' meters, for `audio.levels`. A source that is both
    /// global and in the program scene is listed once.
    fn publish_levels(&self) {
        let mut next = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for row in self.globals_shown.iter().chain(self.heard.iter()) {
            if let Some(meter) = &row.meter
                && seen.insert(row.id)
            {
                next.push((row.id.get(), Arc::clone(meter)));
            }
        }
        self.levels.replace_if_changed(next);
    }

    /// The edited scene's sources, top first (the program scene; in studio mode the
    /// preview scene, ADR-0061).
    fn rows(&self) -> Vec<SourceRow> {
        let mut rows = self.global_rows();
        rows.extend(self.rows_of(self.edited()));
        rows
    }

    /// A scene's sources, top first.
    fn rows_of(&self, scene: SceneId) -> Vec<SourceRow> {
        let states = self.studio.sources();
        let globals = self.globals();
        let global = Some(scene) == globals;
        let panel = self.panel_scenes();
        let Some(scene) = self.collection.scene(scene) else {
            return Vec::new();
        };
        let this = scene.id;
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
                // Match by id: a rename must not make a running source look Idle and
                // drop it from the Mixer.
                let state = states.iter().find(|st| st.source == s.id);
                let status = match state {
                    None => SourceStatus::Idle,
                    Some(st) => match (&st.failed, st.fault) {
                        (Some(why), Some(fault)) if fault != SourceFault::Other => {
                            SourceStatus::Unavailable {
                                fault,
                                detail: why.clone(),
                            }
                        }
                        (Some(why), _) => SourceStatus::Failed(why.clone()),
                        (None, _) => SourceStatus::Running,
                    },
                };
                let (
                    cap_has,
                    cap_l,
                    cap_t,
                    cap_r,
                    cap_b,
                    chrome_on,
                    chrome_t,
                    chrome_r,
                    chrome_b,
                    chrome_l,
                    chrome_fill,
                    has_work_rect,
                    show_for_ms,
                    cue_remaining_ms,
                ) = self.source_row_extra(s, item.id);
                SourceRow {
                    media: MediaSettings::of(s),
                    activity: ActivityChoice::of(s),
                    latency: self.studio.audio_latency(s.id),
                    device_peers: self
                        .device_peers(s.id)
                        .into_iter()
                        .map(|(_, name)| name)
                        .collect(),
                    id: s.id,
                    item: item.id,
                    companion_of: s.companion_of(),
                    record_separately: s.record_separately,
                    name: s.name.clone(),
                    kind,
                    layout: kind
                        .is_some_and(|k| !k.is_audio())
                        .then(|| self.shown_layout(item)),
                    // Global sources are not drawn on the vertical canvas yet.
                    vertical_layout: kind
                        .is_some_and(|k| !k.is_audio() && !global)
                        .then(|| self.vertical_edits())
                        .flatten()
                        .map(|c| self.collection.placed_layout(c, item.layout_on(c))),
                    meter: matches!(status, SourceStatus::Running)
                        .then(|| self.studio.meter(s.id).cloned())
                        .flatten(),
                    picture: self.sizes.get(&s.id).copied(),
                    visible: item.visible,
                    active: !s.standby(),
                    locked: item.locked,
                    audio: s.audio,
                    heard: self
                        .collection
                        .program_levels(self.canvas)
                        .and_then(|l| l.level(s.id))
                        .map(|l| l.over(s.audio)),
                    filters: filters::rows(
                        s,
                        |f| self.studio.audio_filter_meter(f).cloned(),
                        |f| self.studio.audio_filter_latency_ns(f),
                    ),
                    status,
                    global,
                    also_in: {
                        let places = self.collection.placements_of(s.id);
                        panel
                            .iter()
                            .filter(|p| **p != this && places.iter().any(|(sc, _)| sc == *p))
                            .filter_map(|p| self.collection.scene(*p).map(|x| x.name.clone()))
                            .collect()
                    },
                    target: match s.settings.get(keys::TARGET) {
                        Some(SettingValue::Text(t)) => t.clone(),
                        _ => String::new(),
                    },
                    in_globals: !global
                        && globals.is_some_and(|g| {
                            self.collection
                                .placements_of(s.id)
                                .iter()
                                .any(|(sc, _)| *sc == g)
                        }),
                    glide: s.glide,
                    fill: match s.settings.get(keys::COLOR) {
                        Some(SettingValue::Text(t)) => t.clone(),
                        _ => String::new(),
                    },
                    filled: !matches!(
                        s.settings.get(keys::FILLED),
                        Some(SettingValue::Bool(false))
                    ),
                    text: (SourceKind::of(&s.type_id) == Some(SourceKind::Text))
                        .then(|| s.settings.clone()),
                    browser: (kind == Some(SourceKind::Browser)).then(|| browser_page_settings(s)),
                    browser_reason: seams::page_reason(&self.studio, s.id, state),
                    cap_has,
                    cap_l,
                    cap_t,
                    cap_r,
                    cap_b,
                    chrome_on,
                    chrome_t,
                    chrome_r,
                    chrome_b,
                    chrome_l,
                    chrome_fill,
                    has_work_rect,
                    show_for_ms,
                    cue_remaining_ms,
                    scroll_paused: self.scroll_paused_of(item),
                }
            })
            .collect()
    }
}

impl Host {
    /// `profile.list` (`collection` false) or `collection.list`.
    fn library_list(&self, collection: bool) -> Result<Response, ApiError> {
        let (profiles, collections) = self
            .library
            .as_ref()
            .ok_or_else(|| failed("no output profiles or scene collections are loaded yet"))?;
        let mut library = if collection {
            collections.clone()
        } else {
            profiles.clone()
        };
        library.pending = self.switch_pending.is_some();
        Ok(Response::Library(library))
    }

    /// `profile.switch` (`collection` false) or `collection.switch`: checked here,
    /// then handed to the UI, which switches as its File menu does.
    fn library_switch(
        &mut self,
        collection: bool,
        id: String,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let (profiles, collections) = self
            .library
            .as_ref()
            .ok_or_else(|| failed("no output profiles or scene collections are loaded yet"))?;
        let (library, what, doing) = if collection {
            (
                collections,
                "scene collection",
                "switching scene collections",
            )
        } else {
            (profiles, "output profile", "switching output profiles")
        };
        if !library.entries.iter().any(|e| e.id == id) {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no {what} with the id {id:?}"),
            ));
        }
        if library.current == id {
            return Ok(Response::Done(Empty {}));
        }
        let conflict = |m: String| Err(ApiError::new(ErrorKind::Conflict, m));
        if self.any_recording() {
            return conflict(format!("stop the recording before {doing}"));
        }
        if self.any_streaming() {
            return conflict(format!("end the stream before {doing}"));
        }
        if self.switch_pending.is_some() {
            return conflict("another switch is still being made; try again".to_owned());
        }
        self.switch_seq += 1;
        self.switch_pending = Some(self.switch_seq);
        notify(Update::SwitchRequested {
            seq: self.switch_seq,
            collection,
            id,
        });
        Ok(Response::Done(Empty {}))
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
        SceneError::ScrollOnScene(_) => {
            "Scroll is for sources, not for scenes placed in a scene.".to_owned()
        }
        SceneError::InvalidLayout => {
            "Position and size must be numbers, and the size cannot be 0.".to_owned()
        }
        SceneError::UnknownItem(_) | SceneError::UnknownScene(_) | SceneError::UnknownSource(_) => {
            "This source is not in the scene any more.".to_owned()
        }
        SceneError::InvalidSyncOffset | SceneError::InvalidVolume => format!("{e}."),
        SceneError::ChromeNotDisplay => {
            "Only a display source can hide the taskbar or Dock.".to_owned()
        }
        SceneError::UnknownFilter(_) => "This filter is not there any more.".to_owned(),
        SceneError::StingerPlaced(_) => {
            "This source is the stinger, so it cannot be in a scene.".to_owned()
        }
        other => format!("The scene could not be changed ({other:?}). Try again."),
    }
}

/// How long Fit box to text waits for the renderer. Adding a text source does not
/// wait: it uses the default box and [`text_measure`] patches the size later.
const TEXT_MEASURE_WAIT: Duration = Duration::from_millis(200);

/// [`measure_text_wrapped`] at `max_width` (Fit box to text).
fn measure_text_at(
    renderer: Arc<dyn rbs_text::TextRenderer>,
    block: rbs_text::TextBlock,
    max_width: Option<u32>,
) -> Option<[u32; 2]> {
    measure_text_wrapped(renderer, block, max_width, TEXT_MEASURE_WAIT)
}

/// Measures `block` with `renderer` on a thread of its own, waiting at most `wait`.
/// A late answer is dropped with its thread; a failure or a panic is `None`.
fn measure_text_wrapped(
    renderer: Arc<dyn rbs_text::TextRenderer>,
    block: rbs_text::TextBlock,
    max_width: Option<u32>,
    wait: Duration,
) -> Option<[u32; 2]> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rbs-text-measure".into())
        .spawn(move || {
            let measured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                renderer.measure(&block, max_width)
            }));
            let _ = tx.send(measured.ok().and_then(Result::ok));
        })
        .ok()?;
    rx.recv_timeout(wait).ok().flatten()
}

/// One `media.*` method (protocol 1.83).
/// A playback action on a media source (ADR-0108 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaOp {
    /// Play from the cursor.
    Play,
    /// Pause at the cursor.
    Pause,
    /// Move the cursor to this many milliseconds.
    Seek(u64),
    /// Play from the start.
    Restart,
    /// Read the state only.
    State,
}

/// `item.remove`'s answer (protocol 1.88, ADR-0142).
fn item_removed(removed: &Removed) -> ItemRemoved {
    ItemRemoved {
        source_deleted: removed.source_deleted,
        removed_items: removed.removed_items.iter().map(|i| i.get()).collect(),
        deleted_sources: removed.deleted_sources.iter().map(|s| s.get()).collect(),
    }
}

/// A registry with only the encoder `id` of `all`, probed.
fn encoder_catalog(adapters: &RecordAdapters) -> (Vec<EncoderInfo>, Option<String>) {
    let encoders: Vec<EncoderInfo> = adapters
        .video_encoders
        .select(&Requirement::new())
        .candidates
        .iter()
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
    (encoders, cannot_record)
}

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
/// `base`, else `base 2`, `base 3`, ...: the first name no scene has.
fn unique_scene_name(collection: &SceneCollection, base: &str) -> String {
    let taken = |n: &str| collection.scenes().any(|s| s.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_owned())
}

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
    let allowed = collection
        .showable_scenes(canvas)
        .into_iter()
        .next()
        .map(|s| s.id);
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

/// Ids of the held program copy (ADR-0063): far above any id a collection issues, so
/// they never meet one, and the same for every hold that starts from a real scene, so
/// the program's compositor and layers keep their nodes from one hold to the next. A
/// copy's item has the id of the item it copies plus the copy's id.
const HELD_IDS: u32 = 0xF000_0000;

/// The other id of a held copy: a copy taken right after another one (a transition
/// from a held program to the scene held next) gets the id the last one did not have,
/// so the studio sees the program change and runs the transition.
const HELD_IDS_NEXT: u32 = 0xF800_0000;

/// The program as it was when its scene went into preview too (ADR-0063).
#[derive(Clone, Debug)]
struct Held {
    /// The program scene the copy was taken from.
    from: SceneId,
    /// The copy, with ids from [`HELD_IDS`].
    scene: Scene,
}

/// A copy of the canvas's program scene, for the program to show while the same scene
/// is edited in preview; `None` unless the canvas is in studio mode with its program
/// scene in preview.
fn held_copy(
    collection: &SceneCollection,
    canvas: CanvasId,
    after: Option<SceneId>,
) -> Option<Held> {
    let c = collection.canvas(canvas)?;
    let program = c.program?;
    if c.preview != Some(program) {
        return None;
    }
    copy_of(collection, program, after)
}

/// A copy of `program` as it is now, to hold on air (ADR-0063), with the id that is
/// not `after`'s, the copy it follows.
fn copy_of(collection: &SceneCollection, program: SceneId, after: Option<SceneId>) -> Option<Held> {
    let scene = collection.scene(program)?;
    let mut held = scene.clone();
    let base = if after.map(SceneId::get) == Some(HELD_IDS) {
        HELD_IDS_NEXT
    } else {
        HELD_IDS
    };
    held.id = SceneId::from_raw(base);
    // Unique in the run collection; read only if a failure names the scene.
    held.name = format!("{} (on air)", scene.name);
    while collection.scenes().any(|s| s.name == held.name) {
        held.name.push('*');
    }
    for item in &mut held.items {
        item.id = ItemId::from_raw(base.checked_add(item.id.get())?);
    }
    Some(Held {
        from: program,
        scene: held,
    })
}

/// Whether the program the studio runs changed only between a scene and its held copy
/// (ADR-0151 follow-up): studio mode entered or left, or the scene on air previewed,
/// with the same picture on air. Each side is (run id, the user's scene). From one
/// copy to the next (a transition of a scene to itself, ADR-0063 decision 3) is a
/// real change.
fn same_picture(before: Option<(SceneId, SceneId)>, now: Option<(SceneId, SceneId)>) -> bool {
    match (before, now) {
        (Some((was, was_real)), Some((id, real))) => {
            was != id && was_real == real && (was == was_real || id == real)
        }
        _ => false,
    }
}

/// What is held after a change (ADR-0063, ADR-0098): nothing outside studio mode; the
/// copy kept while its scene is on air, whatever the preview scene is, less the items
/// whose source or nested scene is gone from the collection; a new copy when the
/// program scene is also the preview scene and nothing of it is held (the hold starts:
/// studio mode entered, a transition, or an undo or redo put another scene on air);
/// nothing otherwise. Only a transition (which drops the copy) or another scene on air
/// ends a hold, so previewing another scene never puts edits on air.
fn next_held(
    held: Option<Held>,
    collection: &SceneCollection,
    canvas: CanvasId,
    after: Option<SceneId>,
) -> Option<Held> {
    let c = collection.canvas(canvas)?;
    let program = c.program?;
    let preview = c.preview?;
    match held {
        Some(mut h) if h.from == program => {
            h.scene.items.retain(|i| match i.content {
                ItemContent::Source(id) => collection.source(id).is_some(),
                ItemContent::Scene(id) => collection.scene(id).is_some(),
                _ => true,
            });
            Some(h)
        }
        _ if preview == program => held_copy(collection, canvas, after),
        _ => None,
    }
}

/// The collection the studio runs while the program is held: `collection` with the
/// held copy added and on air on `canvas`. `None` when the copy does not fit.
fn run_copy(
    collection: &SceneCollection,
    canvas: CanvasId,
    held: &Held,
) -> Option<SceneCollection> {
    let canvases = collection
        .canvases()
        .map(|c| {
            let mut c = c.clone();
            if c.id == canvas {
                c.program = Some(held.scene.id);
            }
            c
        })
        .collect();
    let mut scenes: Vec<Scene> = collection.scenes().cloned().collect();
    // Scene levels are not held (ADR-0091 decision 3): the copy on air carries the real
    // scene's levels as they are now, like the sources' own volume and mute.
    let mut copy = held.scene.clone();
    copy.audio = collection.scene(held.from).and_then(|s| s.audio.clone());
    scenes.push(copy);
    // The collection's glide and transition go with it: without them a held program
    // ran every change as the default Move.
    SceneCollection::from_parts_with_transition(
        canvases,
        collection.sources().cloned().collect(),
        scenes,
        collection.id_counters(),
        collection.glide(),
        collection.transition(),
    )
    .ok()
}

/// `collection` as the studio runs it while a program copy is held (ADR-0063): the
/// held copy when there is one and it fits, else `None` (the collection itself).
fn run_copy_or(
    collection: &SceneCollection,
    canvas: CanvasId,
    held: Option<&Held>,
) -> Option<SceneCollection> {
    run_copy(collection, canvas, held?)
}

/// A container's file extension.
fn container_ext(container: ContainerId) -> &'static str {
    match container {
        ContainerId::Matroska => "mkv",
        ContainerId::Mp4Fragmented => "mp4",
        ContainerId::MpegTs => "ts",
        ContainerId::Flv => "flv",
        ContainerId::Wav => "wav",
        ContainerId::Flac => "flac",
        ContainerId::Mov => "mov",
        _ => "bin",
    }
}

/// Removes what a refused start left in the recording folder it made: the named files,
/// the manifest and its temporary file, then the folder when nothing else is in it.
/// A delete that fails is left for the user (the start already answers `failed`).
/// Removes the files a refused start may have made and its folder. Returns the names
/// still there (a file another program holds, on Windows).
fn remove_recording_folder(folder: &Path, names: &[String]) -> Vec<String> {
    const MANIFEST_FILE: &str = "recording.rbs.toml";
    let mut left = Vec::new();
    for name in names
        .iter()
        .map(String::as_str)
        .chain([MANIFEST_FILE, &format!(".{MANIFEST_FILE}.tmp")])
    {
        let path = folder.join(name);
        if std::fs::remove_file(&path).is_err() && path.exists() {
            left.push(name.to_owned());
        }
    }
    let _ = std::fs::remove_dir(folder);
    left
}

/// The wire name of a control API enum value (`limit`, `video`, ...).
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// The control API reason of a gate refusal.
fn api_refusal(why: rbs_record::estimate::GateRefusal) -> api::RefusalReason {
    use rbs_record::estimate::GateRefusal as G;
    match why {
        G::HardwareLimit => api::RefusalReason::HardwareLimit,
        G::OverLimit => api::RefusalReason::OverLimit,
        G::Heavy => api::RefusalReason::Heavy,
    }
}

/// A start refused by the gate (control API 1.54): `judged` is the plan it judged,
/// `tracks` the files concerned, `software` the video files whose encoder would have
/// fallen to a software one (`software_fallback` reasons).
fn gate_refusal(
    reason: api::RefusalReason,
    judged: &rbs_record::estimate::Estimate,
    tracks: &[rbs_record::estimate::TrackRef],
    software: &[SourceId],
) -> StartError {
    let mut estimate = api_estimate(judged, None);
    add_software_fallback(&mut estimate.reasons, software);
    let tracks: Vec<api::EstimateTrack> = tracks
        .iter()
        .map(|t| api::EstimateTrack {
            source: t.source.get(),
            role: api_track_role(t.role),
        })
        .collect();
    let message = match reason {
        api::RefusalReason::HardwareLimit => format!(
            "{} separate video file(s) are more than their encoder can take; start with \
             on_hardware_limit \"skip\" to record without them",
            tracks.len()
        ),
        api::RefusalReason::Heavy => "the recording would be heavy for this computer; start \
             with acknowledge_heavy to record anyway"
            .to_owned(),
        api::RefusalReason::OpenFailed => "nothing was recorded".to_owned(),
        _ => "the separate video files are over a limit of this computer".to_owned(),
    };
    StartError::Refused(
        message,
        Box::new(api::StartRefusal {
            reason,
            estimate,
            tracks,
        }),
    )
}

/// Adds a `software_fallback` reason for each video file of `software`.
fn add_software_fallback(reasons: &mut Vec<api::EstimateReason>, software: &[SourceId]) {
    for source in software {
        reasons.push(api::EstimateReason {
            code: api::EstimateCode::SoftwareFallback,
            resource: None,
            track: Some(api::EstimateTrack {
                source: source.get(),
                role: api::TrackRole::Video,
            }),
        });
    }
}

/// The control API parameters naming a source in the scene being edited, on the main
/// canvas: what the window's commands mean.
fn in_scene(id: SourceId) -> rbs_api::request::SourceInScene {
    rbs_api::request::SourceInScene {
        source: id.get(),
        scene: None,
        canvas: OutputCanvas::default(),
    }
}

/// The control API's spelling of a `run` value (ADR-0152).
/// A page's deprecated `keep_running`, computed from its activity policy (ADR-0152
/// decision 10): true when its effective `run` is `placed`.
fn keeps_running(source: &Source) -> bool {
    rbs_sources::activity_traits(&source.type_id)
        .effective(Some(&source.activity))
        .run
        == rbs_scene::activity::Run::Placed
}

/// A page's settings as the Page tab reads them: the stored keys and the computed
/// `keep_running` (true only), which is no longer stored (ADR-0152 decision 10).
fn browser_page_settings(source: &Source) -> Settings {
    let mut settings = source.settings.clone();
    if keeps_running(source) {
        settings.insert(keys::KEEP_RUNNING.to_owned(), SettingValue::Bool(true));
    }
    settings
}

/// A film's deprecated `restart_when_shown`, computed from its activity policy
/// (ADR-0152 decision 10): true unless its effective `on_return` is not `restart`.
fn restarts_when_shown(source: &Source) -> bool {
    rbs_sources::activity_traits(&source.type_id)
        .effective(Some(&source.activity))
        .on_return
        .is_none_or(|r| r == rbs_scene::activity::OnReturn::Restart)
}

fn api_run(r: rbs_scene::activity::Run) -> api::ActivityRun {
    use rbs_scene::activity::Run;
    match r {
        Run::Placed => api::ActivityRun::Placed,
        Run::Shown => api::ActivityRun::Shown,
        _ => api::ActivityRun::OnAir,
    }
}

/// The scene model's `run` value for the control API's spelling (ADR-0152).
fn activity_run(r: api::ActivityRun) -> rbs_scene::activity::Run {
    use rbs_scene::activity::Run;
    match r {
        api::ActivityRun::Placed => Run::Placed,
        api::ActivityRun::Shown => Run::Shown,
        api::ActivityRun::OnAir => Run::OnAir,
    }
}

/// The control API's spelling of an `on_return` value (ADR-0152).
fn api_return(o: rbs_scene::activity::OnReturn) -> api::ActivityReturn {
    use rbs_scene::activity::OnReturn;
    match o {
        OnReturn::Continue => api::ActivityReturn::Continue,
        OnReturn::Resume => api::ActivityReturn::Resume,
        _ => api::ActivityReturn::Restart,
    }
}

/// The scene model's `on_return` value for the control API's spelling (ADR-0152).
fn activity_return(o: api::ActivityReturn) -> rbs_scene::activity::OnReturn {
    use rbs_scene::activity::OnReturn;
    match o {
        api::ActivityReturn::Continue => OnReturn::Continue,
        api::ActivityReturn::Resume => OnReturn::Resume,
        api::ActivityReturn::Restart => OnReturn::Restart,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_a_scene_and_its_held_copy_show_the_same_picture() {
        let id = SceneId::from_raw;
        let (x, y, copy, next) = (id(3), id(4), id(HELD_IDS), id(HELD_IDS_NEXT));
        // Studio mode entered (the hold starts) and left (the hold ends).
        assert!(same_picture(Some((x, x)), Some((copy, x))));
        assert!(same_picture(Some((copy, x)), Some((x, x))));
        // A transition of the scene to itself, another scene, or nothing new.
        assert!(!same_picture(Some((copy, x)), Some((next, x))));
        assert!(!same_picture(Some((copy, x)), Some((y, y))));
        assert!(!same_picture(Some((x, x)), Some((x, x))));
        assert!(!same_picture(None, Some((x, x))));
    }

    use super::*;

    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    #[test]
    fn browser_input_from_the_api_is_checked_and_becomes_wire_events() {
        use rbs_api::request::{BrowserInputEvent as E, BrowserModifiers, BrowserMouseButton};
        use rbs_browser_ipc::input::{self as wire, InputEvent, Key, NamedKey};
        let shift = BrowserModifiers {
            shift: true,
            ..BrowserModifiers::default()
        };
        let click = |clicks| E::Button {
            x: 3,
            y: 4,
            button: BrowserMouseButton::Right,
            down: true,
            clicks,
            modifiers: shift,
        };
        assert_eq!(
            browser_input_event(&click(2)).unwrap(),
            InputEvent::Button {
                x: 3,
                y: 4,
                button: wire::Button::Right,
                down: true,
                clicks: 2,
                modifiers: wire::SHIFT,
            }
        );
        assert!(browser_input_event(&click(0)).is_err());
        assert!(browser_input_event(&click(4)).is_err());
        let key = |name: &str| E::Key {
            key: name.to_owned(),
            down: true,
            modifiers: BrowserModifiers::default(),
        };
        assert_eq!(
            browser_input_event(&key("Enter")).unwrap(),
            InputEvent::Key {
                key: Key::Named(NamedKey::Enter),
                down: true,
                modifiers: 0,
            }
        );
        assert!(browser_input_event(&key("NoSuchKey")).is_err());
        let text = |t: String| E::Text { text: t };
        assert_eq!(
            browser_input_event(&text("hi".into())).unwrap(),
            InputEvent::Text("hi".into())
        );
        assert!(browser_input_event(&text("x".repeat(257))).is_err());
    }

    /// While an output runs or a switch waits, the round keeps the short sleep. Idle
    /// does not use that cap: with no deadline the control thread blocks on the next
    /// message (F-08).
    #[test]
    fn the_control_thread_sleeps_longer_while_nothing_runs() {
        let mut outs = [CanvasOutputs::default(), CanvasOutputs::default()];
        assert_eq!(round_cap(&outs, false), IDLE_WAIT);
        assert_eq!(round_cap(&outs, true), MAX_WAIT);
        outs[1].ending = true;
        assert_eq!(round_cap(&outs, false), MAX_WAIT);
    }

    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    #[test]
    fn a_reopened_window_is_found_by_application_and_title_only_when_unique() {
        let window = |id: &str, name: &str, app: Option<&str>| Target {
            name: name.to_owned(),
            adapter: "a".to_owned(),
            target: id.to_owned(),
            is_default: false,
            size: None,
            app: app.map(str::to_owned),
            recent: None,
            picker: false,
            mic: None,
            sound: false,
        };
        let find = |listed: Vec<Target>, app: &str| match_by_hints(listed, "a", "old", app, "zsh");
        // One match, the application in another case.
        let one = vec![
            window("new", "zsh", Some("TERMINAL")),
            window("x", "vim", Some("Terminal")),
        ];
        assert_eq!(find(one, "Terminal").unwrap().target, "new");
        // Two windows alike: nothing is guessed.
        let two = vec![
            window("n1", "zsh", Some("Terminal")),
            window("n2", "zsh", Some("Terminal")),
        ];
        assert!(find(two, "Terminal").is_none());
        // Another application, no application known, a different title.
        assert!(find(vec![window("n", "zsh", Some("iTerm"))], "Terminal").is_none());
        assert!(find(vec![window("n", "zsh", None)], "Terminal").is_none());
        assert!(find(vec![window("n", "zsh2", Some("Terminal"))], "Terminal").is_none());
        // The stored window is still there: it is used as it is.
        let still = vec![
            window("old", "zsh", Some("Terminal")),
            window("new", "zsh", Some("Terminal")),
        ];
        assert!(find(still, "Terminal").is_none());
    }

    #[test]
    fn hints_name_the_target_and_its_application() {
        let target = |name: &str, app: Option<&str>| Target {
            name: name.to_owned(),
            adapter: String::new(),
            target: String::new(),
            is_default: false,
            size: None,
            app: app.map(str::to_owned),
            recent: None,
            picker: false,
            mic: None,
            sound: false,
        };
        let h = hints_of(&target("zsh - Terminal", Some("Terminal")));
        assert_eq!(
            h.get(HINT_TITLE).map(String::as_str),
            Some("zsh - Terminal")
        );
        assert_eq!(h.get(HINT_APP).map(String::as_str), Some("Terminal"));
        // No application known: the title alone, so Reload by hints cannot match.
        assert_eq!(hints_of(&target("Display 1", None)).len(), 1);
        assert!(hints_of(&target("", Some(""))).is_empty());
    }

    /// A collection with a program scene holding one audio item, in studio mode with
    /// the program scene in preview too.
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn held_collection() -> (SceneCollection, CanvasId, SceneId, SourceId) {
        let d = HostConfig::desktop();
        let mut col = SceneCollection::new();
        let (canvas, scene) = shown_scene(&mut col, d.size, d.fps).unwrap();
        let (src, _) = col
            .add_source("Mic", SourceKind::AudioInput.type_id(), Settings::new())
            .unwrap();
        col.add_item(
            scene,
            ItemContent::Source(src),
            Layout::new([0.0, 0.0], [10.0, 10.0]),
            None,
        )
        .unwrap();
        col.set_preview(canvas, Some(scene)).unwrap();
        (col, canvas, scene, src)
    }

    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    #[test]
    fn a_held_program_runs_with_the_collections_transition_and_glide() {
        // Studio mode with the program held (ADR-0063): the run collection must keep
        // the collection's transition, or every change runs as the default Move.
        let (mut col, canvas, scene, _) = held_collection();
        let mut t = rbs_scene::CollectionTransition {
            kind: rbs_scene::TransitionKind::Swipe,
            ..rbs_scene::CollectionTransition::default()
        };
        t.swipe.duration_ms = 700;
        col.set_transition(t).unwrap();
        let glide = rbs_scene::CollectionGlide {
            duration_ms: 900,
            ..col.glide()
        };
        col.set_glide(glide).unwrap();
        col.set_preview(canvas, Some(scene)).unwrap();
        let held = held_copy(&col, canvas, None).unwrap();
        let run = run_copy(&col, canvas, &held).unwrap();
        assert_eq!(run.transition(), t);
        assert_eq!(run.glide(), glide);
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn a_held_program_keeps_the_scene_as_it_was_and_is_never_listed() {
        let (mut col, canvas, scene, _) = held_collection();
        // Outside studio mode, or with another scene in preview: nothing is held.
        col.set_preview(canvas, None).unwrap();
        assert!(held_copy(&col, canvas, None).is_none());
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        assert!(held_copy(&col, canvas, None).is_none());

        // The program scene in preview too: the program shows a copy of it.
        col.set_preview(canvas, Some(scene)).unwrap();
        let held = held_copy(&col, canvas, None).unwrap();
        assert_eq!(held.from, scene);
        assert_eq!(held.scene.items.len(), 1);
        // An edit made after the hold reaches the scene, not the copy on air.
        let item = col.scene(scene).unwrap().items[0].id;
        assert_eq!(
            held.scene.items[0].id.get(),
            HELD_IDS + item.get(),
            "an item's copy keeps its id, offset"
        );
        col.remove_item(item).unwrap();
        let held = next_held(Some(held), &col, canvas, None).unwrap();
        let run = run_copy(&col, canvas, &held).unwrap();
        let program = run.canvas(canvas).unwrap().program.unwrap();
        assert_eq!(program, held.scene.id);
        assert_eq!(run.scene(program).unwrap().items.len(), 1, "as it was");
        assert_eq!(
            run.scene(scene).unwrap().items.len(),
            0,
            "the edit, in preview"
        );
        assert_eq!(run.canvas(canvas).unwrap().preview, Some(scene));
        // The user's collection never holds the copy.
        assert!(col.scene(held.scene.id).is_none());
        assert_eq!(col.canvas(canvas).unwrap().program, Some(scene));
        assert_eq!(col.scenes().count(), 2);
        // The same scene id for every hold, so the program keeps its nodes.
        assert_eq!(
            held_copy(&col, canvas, None).unwrap().scene.id,
            held.scene.id
        );
        // Leaving studio mode lets the copy go.
        col.set_preview(canvas, None).unwrap();
        assert!(next_held(Some(held), &col, canvas, None).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn previewing_another_scene_keeps_the_program_held() {
        // As after a transition that keeps the preview scene (ADR-0098): the program
        // scene is in preview and held, and its items are edited.
        let (mut col, canvas, scene, _) = held_collection();
        let held = held_copy(&col, canvas, None).unwrap();
        let item = col.scene(scene).unwrap().items[0].id;
        col.remove_item(item).unwrap();
        // Another scene goes to preview: the edit still waits for a transition.
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        let held = next_held(Some(held), &col, canvas, None).unwrap();
        assert_eq!(held.from, scene);
        let run = run_copy(&col, canvas, &held).unwrap();
        let program = run.canvas(canvas).unwrap().program.unwrap();
        assert_eq!(run.scene(program).unwrap().items.len(), 1, "as it was");
        assert_eq!(run.canvas(canvas).unwrap().preview, Some(other));
        // A copy that must be taken again (it no longer fits) is of the held scene, also
        // with another scene in preview.
        let again = copy_of(&col, held.from, None).unwrap();
        assert_eq!(again.from, scene);
        assert!(run_copy(&col, canvas, &again).is_some());
        // Without a hold, another scene in preview starts none.
        assert!(next_held(None, &col, canvas, None).is_none());
        // The other scene on air (a transition): the copy goes; it is in preview too,
        // so the hold starts again with it.
        col.set_program(canvas, Some(other)).unwrap();
        let after = Some(held.scene.id);
        let next = next_held(Some(held), &col, canvas, after).unwrap();
        assert_eq!(next.from, other);
        // The copy after a copy has the other id, so the studio sees a new program
        // and runs the transition; the one after that has the first id again.
        assert_ne!(Some(next.scene.id), after);
        let third = copy_of(&col, other, Some(next.scene.id)).unwrap();
        assert_eq!(Some(third.scene.id), after);
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn a_held_copy_never_outlives_its_scene_or_its_sources() {
        let (mut col, canvas, scene, src) = held_collection();
        let held = held_copy(&col, canvas, None).unwrap();
        // A source deleted while held leaves the copy, which still fits.
        let item = col.scene(scene).unwrap().items[0].id;
        col.remove_item(item).unwrap();
        col.delete_source(src).unwrap();
        let kept = next_held(Some(held.clone()), &col, canvas, None).unwrap();
        assert!(kept.scene.items.is_empty());
        assert!(run_copy(&col, canvas, &kept).is_some());
        // Another scene on air in studio mode with itself in preview (as after an
        // undo removed the held scene): a new copy of that scene.
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_program(canvas, Some(other)).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        let next = next_held(Some(held), &col, canvas, None).unwrap();
        assert_eq!(next.from, other);
    }

    #[test]
    fn previews_keep_the_canvas_aspect() {
        let d = preview_desc(Size::new(3440, 1440), FrameRate::FPS_30);
        assert_eq!((d.width, d.height), (1280, 534));
    }

    #[test]
    fn preview_ring_covers_the_layer_hold_at_30_and_60_fps() {
        let surfaces = |fps| preview_desc(Size::new(1920, 1080), fps).surfaces;
        let n30 = surfaces(FrameRate::FPS_30);
        let n60 = surfaces(FrameRate::FPS_60);
        // A ring of 3 is shown + written + in flight, with nothing held after a
        // replace. The layer's default hold needs more than that at both rates
        // (geometry::ring_needed; ADR-0034, ADR-0036), so 3 drops frames.
        assert!(n30 > 3, "30 fps needs {n30}");
        assert!(n60 > 3, "60 fps needs {n60}");
        assert!(n30 <= MAX_PREVIEW_SURFACES && n60 <= MAX_PREVIEW_SURFACES);
        assert_eq!(n30, preview_surfaces(FrameRate::FPS_30));
        assert_eq!(n60, preview_surfaces(FrameRate::FPS_60));
        // macOS: 34 ms → 5 at 30 fps, 6 at 60 fps. Windows: one frame + 4 ms → 5 and 5.
        // Linux uses the macOS hold; it does not publish a preview.
        #[cfg(not(windows))]
        {
            assert_eq!((n30, n60), (5, 6));
        }
        #[cfg(windows)]
        {
            assert_eq!((n30, n60), (5, 5));
        }
        // Far above the cap: 2000 fps still stays within MAX_PREVIEW_SURFACES.
        let fast = match FrameRate::new(2_000, 1) {
            Some(rate) => rate,
            None => FrameRate::FPS_60,
        };
        assert_eq!(surfaces(fast), MAX_PREVIEW_SURFACES);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn host_preview_desc_follows_the_canvas_rate() {
        let at = |fps| {
            HostConfig::canvas(Size::new(1920, 1080), fps)
                .preview
                .map(|p| p.surfaces)
        };
        assert_eq!(
            at(FrameRate::FPS_30),
            Some(preview_surfaces(FrameRate::FPS_30))
        );
        assert_eq!(
            at(FrameRate::FPS_60),
            Some(preview_surfaces(FrameRate::FPS_60))
        );
        let desktop = HostConfig::desktop().preview.map(|p| p.surfaces);
        assert_eq!(desktop, Some(preview_surfaces(FrameRate::FPS_30)));
    }
}
