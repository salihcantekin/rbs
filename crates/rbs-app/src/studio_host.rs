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
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
use rbs_core::media::{ContainerId, Size};
use rbs_core::registry::{AdapterFactory, Registry};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_engine::Engine;
use rbs_gpu::GpuContext;
use rbs_gpu::share::{MAX_PREVIEW_SURFACES, PreviewDesc, PreviewEndpoint, ring_needed};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AudioSettings, ExtraCanvas, Link, OutputId, OutputSettings, Progress, RecordAdapters,
    RecordError, Studio, StudioSettings, VideoSettings, VirtualCameraSettings,
};
use rbs_scene::model::{
    Border, CanvasId, Crop, DisplayChrome, FilterId, ItemContent, ItemId, ItemStyle, Layout,
    MasterAudio, Scene, SceneId, SceneItem, SceneScope, SettingValue, Settings, Shadow, Shape,
    Source, SourceAudio, SourceId, SourceMonitor, SourceRegion,
};
use rbs_scene::{
    AutoLayout, CenterAxis, ChromeFill, FitMode, SceneError, Undo, place_chrome, recrop_keeping,
    visible_size,
};
use rbs_scene::{COMPANION_OF_HINT, STANDBY_HINT, SceneCollection};
use rbs_sources::BrowserTree;
use rbs_sources::types::{self, keys};
use rbs_sources::{SourceError, SourceTarget};
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};
use rbs_vcam::{InstallState, VcamStats, VirtualCameraFactory};

pub mod filters;
mod monitor;
pub mod replay;

use crate::canvas_profile::CanvasProfile;
use crate::edit_history::{EditKind, History, HistoryError, StepInfo};
use crate::monitor_settings::MonitorSettings;
use crate::recent_targets::{Recent, RecentTargets};
use crate::record_profile::{API_PROFILE_ID, RecordProfile, output_size_matches};
use crate::remote_control::ApiTokens;
use crate::stream_key::{self, KeyStore, MemoryKeyStore};
use crate::stream_profile::{Protocol, StreamProfile, parse_server};
use crate::user_session::CollectionSaver;
use crate::vertical_outputs::VerticalFlags;

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
    /// Where its picture is on the vertical canvas, in that canvas's pixels, while one
    /// exists (video sources, ADR-0093 decision 4): its own layout there, else the
    /// default one placed in that canvas.
    pub vertical_layout: Option<Layout>,
    /// Its item in the program scene.
    pub item: ItemId,
    /// The picture this source is the "Capture audio too" companion of (ADR-0096).
    pub companion_of: Option<SourceId>,
    /// Width and height in pixels of its picture, as last seen while it ran (video
    /// sources, ADR-0058); `None` before it ran.
    pub picture: Option<(u32, u32)>,
    /// Shown in the program scene. A hidden source is not drawn; its device stays
    /// open unless `active` is false.
    pub visible: bool,
    /// The device stays open while a scene that places the source is on air or in
    /// preview. False when the user deactivated it.
    pub active: bool,
    /// Locked: the picture panel does not select, move, resize or crop it (ADR-0059).
    pub locked: bool,
    /// The source's volume, a linear factor from 0 to 1, its mute (ADR-0059) and its
    /// sync offset (ADR-0062).
    pub audio: SourceAudio,
    /// Its filters in chain order (ADR-0062).
    pub filters: Vec<filters::FilterRow>,
    /// A global source's row (ADR-0065): its item is in the canvas's global scene.
    pub global: bool,
    /// The other scenes that place the source, by name, in the Scenes panel's order;
    /// `in_globals` says whether it is also a global source.
    pub also_in: Vec<String>,
    /// The source is also a global source (on a scene row of a source that is both).
    pub in_globals: bool,
    /// The target id stored in its settings (`target`), empty when it has none: the
    /// Source tab marks it in the list (ADR-0077).
    pub target: String,
    /// Per-source slide override (ADR-0100); `None` uses the collection defaults.
    pub glide: Option<rbs_scene::SourceGlide>,
    /// A colour source's fill (`color`), empty for every other kind.
    pub fill: String,
    /// A colour source draws its fill. False leaves the inside empty. True for every
    /// other kind and when the setting is absent.
    pub filled: bool,
    /// Capture region in full-picture pixels (ADR-0103); absent when `cap_has` is false.
    pub cap_has: bool,
    /// Left edge of the capture region.
    pub cap_l: u32,
    /// Top edge of the capture region.
    pub cap_t: u32,
    /// Right edge of the capture region.
    pub cap_r: u32,
    /// Bottom edge of the capture region.
    pub cap_b: u32,
    /// Display chrome (ADR-0104); off when `chrome_on` is false. `chrome_fill`: 0 cover,
    /// 1 fit.
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
    /// The listed display reported a work area for Reset chrome.
    pub has_work_rect: bool,
    /// Stored timed-show duration (ADR-0105); 0 when unset.
    pub show_for_ms: u32,
    /// Milliseconds left on a running Show cue; 0 when none.
    pub cue_remaining_ms: u32,
}

impl PartialEq for SourceRow {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.kind == other.kind
            && self.status == other.status
            && self.layout == other.layout
            && self.vertical_layout == other.vertical_layout
            && self.item == other.item
            && self.companion_of == other.companion_of
            && self.picture == other.picture
            && self.visible == other.visible
            && self.active == other.active
            && self.locked == other.locked
            && self.audio == other.audio
            && self.filters == other.filters
            && self.global == other.global
            && self.also_in == other.also_in
            && self.in_globals == other.in_globals
            && self.target == other.target
            && self.glide == other.glide
            && self.fill == other.fill
            && self.filled == other.filled
            && self.cap_has == other.cap_has
            && self.cap_l == other.cap_l
            && self.cap_t == other.cap_t
            && self.cap_r == other.cap_r
            && self.cap_b == other.cap_b
            && self.chrome_on == other.chrome_on
            && self.chrome_t == other.chrome_t
            && self.chrome_r == other.chrome_r
            && self.chrome_b == other.chrome_b
            && self.chrome_l == other.chrome_l
            && self.chrome_fill == other.chrome_fill
            && self.has_work_rect == other.has_work_rect
            && self.show_for_ms == other.show_for_ms
            && self.cue_remaining_ms == other.cue_remaining_ms
            && match (&self.meter, &other.meter) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
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

/// What the UI asks the studio to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// List what a kind can capture ([`Update::Targets`]).
    ListTargets(SourceKind),
    /// Add a display or window and its "Capture audio too" companion sound as one
    /// undo step (ADR-0096). A refused companion adds nothing.
    AddSourceWithAudio {
        /// Its kind (a display or a window).
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// Add a source of `kind` capturing `target` to the program scene, on top.
    AddSource {
        /// Its kind.
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// Delete a source from every scene and the global sources (glossary: "Delete
    /// source").
    DeleteSource(SourceId),
    /// Remove a source's item from the edited scene, or its global item (glossary:
    /// "Remove from scene", ADR-0065); the source goes too when that was its last
    /// placement.
    RemoveFromScene(SourceId),
    /// Make a source global on the studio's canvas (`true`: it leaves the canvas's
    /// scenes, ADR-0065) or move its global item into the edited scene (`false`).
    SetGlobal {
        /// The source.
        id: SourceId,
        /// Global, else back in the edited scene.
        global: bool,
    },
    /// Remove a source from the global sources (Settings › Global sources); the
    /// source goes too when that was its last placement.
    RemoveGlobal(SourceId),
    /// Add a new source of `kind` capturing `target` as a global source (one undo
    /// step).
    AddGlobalSource {
        /// Its kind.
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// The toast's Undo after a delete: undoes the newest edit if it is a delete, else
    /// nothing (another edit came after it).
    UndoDelete,
    /// Undo the newest scene edit (U4): an add, a delete, a layer move or a box; all
    /// the boxes of one drag are one edit. [`Update::History`] says what is next.
    Undo,
    /// Redo the edit undone last.
    Redo,
    /// Move a source among the sources with a picture in the program scene
    /// (`up` / `down` one step, `front` / `back` to an end). Nothing happens at that end.
    MoveSource {
        /// The source.
        id: SourceId,
        /// Which way (ADR-0084 Arrange).
        direction: rbs_api::request::LayerDirection,
    },
    /// Move a scene one place or to an end in the canvas's scene list.
    MoveScene {
        /// The scene.
        id: SceneId,
        /// Which way.
        direction: rbs_api::request::LayerDirection,
    },
    /// Move a scene to where scene `to` is in the scene list (a row dragged onto
    /// another); one undo step.
    MoveSceneTo {
        /// The scene.
        id: SceneId,
        /// The scene whose place it takes.
        to: SceneId,
    },
    /// Move a source to where source `to` is in the program scene's layer order (a
    /// row dragged onto another row of the Sources list); one undo step.
    MoveSourceTo {
        /// The source.
        id: SourceId,
        /// The source whose place it takes.
        to: SourceId,
    },
    /// Lock or unlock a source in the program scene (one undo step, ADR-0059).
    SetSourceLocked {
        /// The source.
        id: SourceId,
        /// Locked.
        locked: bool,
    },
    /// A source's volume, a linear factor from 0 to 1 (ADR-0059). A live control: not
    /// on the undo stack, and it reaches the mixer without rebinding the scene.
    SetSourceVolume {
        /// The source.
        id: SourceId,
        /// From 0 to 1.
        volume: f32,
    },
    /// Mute or unmute a source's sound (ADR-0059). A live control: not on the undo
    /// stack.
    SetSourceMuted {
        /// The source.
        id: SourceId,
        /// Muted.
        muted: bool,
    },
    /// Adds a vertical second canvas and runs it beside the main one (ADR-0093 decision
    /// 8): the canvas is made, follows the main canvas's program scene when asked, gets
    /// an Auto layout of the program scene when asked, and starts running (one output
    /// can take it, one picture can show it). [`Update::Vertical`] answers. Not on the
    /// undo stack.
    AddVerticalCanvas(VerticalCanvas),
    /// Stops the second canvas and removes it from the collection (its outputs must be
    /// stopped first). [`Update::Vertical`] answers.
    RemoveVerticalCanvas,
    /// The canvas's master volume, a linear factor from 0 to 1 (ADR-0081). A live
    /// control like a source's volume: not on the undo stack, no rebind.
    SetMasterVolume {
        /// From 0 to 1.
        volume: f32,
    },
    /// Mute or unmute the canvas's master (ADR-0081). A live control.
    SetMasterMuted {
        /// Muted.
        muted: bool,
    },
    /// Whether a source is heard on the monitor (ADR-0097 decision 5). A live control:
    /// not on the undo stack; the device opens or closes on the host's next round.
    SetSourceMonitor {
        /// The source.
        id: SourceId,
        /// Off, Only or Also.
        monitor: SourceMonitor,
    },
    /// The sources soloed on the monitor (session state, never saved); empty ends solo.
    SetMonitorSolo(Vec<SourceId>),
    /// The monitor device (`None`: System default), saved in `monitor.toml`.
    SetMonitorDevice(Option<String>),
    /// The monitor volume, linear from 0 to 1, saved in `monitor.toml`.
    SetMonitorVolume(f32),
    /// Lists the playback devices ([`Update::MonitorDevices`] answers). Enumerates:
    /// sent only while a device picker is shown (ADR-0097 decision 7).
    ListMonitorDevices,
    /// Hide a source from the Mixer or show it again (ADR-0080). A live view choice like
    /// the mute: not on the undo stack; the sound is untouched.
    SetSourceMixerHidden {
        /// The source.
        id: SourceId,
        /// Hidden from the Mixer.
        hidden: bool,
    },
    /// Add a filter of a built-in type with its defaults at the end of a source's chain
    /// (ADR-0062; one undo step).
    AddFilter {
        /// The source.
        source: SourceId,
        /// The filter type's id.
        type_id: String,
    },
    /// Apply a built-in look to a source's item and filters (ADR-0075; one undo step).
    ApplyLook {
        /// The source.
        source: SourceId,
        /// The look's id (`builtin:...`).
        look: String,
    },
    /// Add a built-in sample picture to the scene being edited: a background below
    /// everything, a frame above it (ADR-0074, ADR-0075; one undo step).
    AddSample {
        /// The sample's id (`rbs_sources::still::SAMPLES`).
        sample: String,
    },
    /// Add a colour shape on the scene being edited, centred, one undo step.
    /// 0 circle, 1 square, 2 rectangle, 3 rounded rectangle, 4 ellipse, 5 triangle,
    /// 6 arrow.
    AddStamp {
        /// Which shape.
        kind: u8,
    },
    /// Add an image source from a PNG file, fitted into the canvas, one undo step.
    AddImage {
        /// The file.
        path: std::path::PathBuf,
    },
    /// Add a media file source (ADR-0108), centred at half the canvas, one undo step.
    AddMedia {
        /// A local regular file. The demo decoder ignores contents.
        path: std::path::PathBuf,
    },
    /// Add a browser page (ADR-0109), fitted to the canvas, one undo step.
    AddBrowser {
        /// `http`, `https`, or `file` URL.
        url: String,
    },
    /// The fill colour of a colour source (`#rrggbb` or `#rrggbbaa`), one undo step.
    SetSourceFill {
        /// The source.
        id: SourceId,
        /// The colour.
        color: String,
    },
    /// Whether a colour source draws its fill. Off leaves the inside empty and, when
    /// the shape has no border, turns a border on so the shape stays visible. One
    /// undo step.
    SetSourceFilled {
        /// The source.
        id: SourceId,
        /// Draw the fill.
        on: bool,
    },
    /// Remove a filter (one undo step).
    RemoveFilter(FilterId),
    /// Move a filter to `index` in its chain (one undo step).
    MoveFilter {
        /// The filter.
        filter: FilterId,
        /// Its new place, 0 first.
        index: usize,
    },
    /// Turn a filter on or off (one undo step).
    SetFilterEnabled {
        /// The filter.
        filter: FilterId,
        /// On.
        enabled: bool,
    },
    /// Write a filter's full values (a released slider, a colour, Reset to defaults;
    /// one undo step).
    SetFilterValues {
        /// The filter.
        filter: FilterId,
        /// Every field's value.
        values: rbs_filters::Values,
    },
    /// Show values on the running filter without changing the collection (a slider
    /// drag, ADR-0062 decision 3).
    PreviewFilter {
        /// The filter.
        filter: FilterId,
        /// Every field's value.
        values: rbs_filters::Values,
    },
    /// Put the collection's values back on the running filter (a cancelled drag).
    RestoreFilter(FilterId),
    /// The sync offset of a source's sound in milliseconds (ADR-0062 decision 10): a
    /// live control, not on the undo stack.
    SetSourceSyncOffset {
        /// The source.
        id: SourceId,
        /// Milliseconds; positive delays the sound.
        offset_ms: i32,
    },
    /// Point a source at another target of its kind, keeping its name, layout, filters
    /// and place in every scene (one undo step, ADR-0077).
    RetargetSource {
        /// The source.
        id: SourceId,
        /// What it should capture now.
        target: Target,
    },
    /// Grab small pictures of the listed targets of `kind`, two at a time off the control
    /// thread, one [`Update::Thumbnail`] each (ADR-0077). A newer request or
    /// [`Command::StopThumbnails`] makes older work stop at its next item.
    GrabThumbnails {
        /// The source kind whose adapters are asked.
        kind: SourceKind,
        /// (adapter, target) of each row, in the order wanted.
        rows: Vec<(String, String)>,
        /// The largest picture wanted, in pixels.
        max: (u32, u32),
    },
    /// The target list closed: stop grabbing and forget the pictures.
    StopThumbnails,
    /// Ask for what the Source settings window shows ([`Update::SourceSettings`]).
    OpenSourceSettings(SourceId),
    /// The Source settings window closed: drop a late region preview.
    CloseSourceSettings,
    /// Change a source's stored settings by a patch (a `null` removes a key; one undo
    /// step, ADR-0077): what Apply and OK of the Source settings window send.
    SetSourceSettings {
        /// The source.
        id: SourceId,
        /// The keys to change.
        patch: BTreeMap<String, serde_json::Value>,
    },
    /// Try a source that is not running again with its stored settings (ADR-0077).
    ReloadSource(SourceId),
    /// Adds (`on`) or removes the "Capture audio too" companion of a display or window
    /// source, one undo step (ADR-0096).
    SetCompanionAudio {
        /// The display or window source.
        source: SourceId,
        /// Add (`true`) or remove the companion.
        on: bool,
    },
    /// Rename a source (one undo step). Names are unique in the collection.
    RenameSource {
        /// The source.
        id: SourceId,
        /// The new name.
        name: String,
    },
    /// Collection glide defaults (ADR-0100). One undo step.
    SetGlide {
        /// Duration in milliseconds (`0..=5000`).
        duration_ms: u32,
        /// `0` linear, `1` ease-in-out.
        easing: u8,
    },
    /// Per-source slide override (ADR-0100). `custom` false clears it. One undo step.
    SetSourceGlide {
        /// The source.
        id: SourceId,
        /// When false, clear the override.
        custom: bool,
        /// Duration in milliseconds when `custom`.
        duration_ms: u32,
        /// `0` linear, `1` ease-in-out when `custom`.
        easing: u8,
    },
    /// Place a source the collection already has into the program scene, on top (one
    /// undo step); it runs as one instance in every scene that places it.
    AddExistingSource(SourceId),
    /// Add an empty scene the canvas may show (one undo step); `None` names it
    /// "Scene", "Scene 2", ... It does not go on air.
    AddScene(Option<String>),
    /// Copy a scene with the same sources and layouts (one undo step). It does not go
    /// on air.
    DuplicateScene(SceneId),
    /// Remove a scene (one undo step). Refused for the last scene the canvas may show;
    /// the scene on air is first switched away from (not undone with the removal).
    RemoveScene(SceneId),
    /// Rename a scene (one undo step).
    RenameScene {
        /// The scene.
        id: SceneId,
        /// The new name.
        name: String,
    },
    /// Put a scene on air (a cut); in studio mode, make it the preview scene instead
    /// (ADR-0061). A live action: not on the undo stack (ADR-0021).
    SwitchScene(SceneId),
    /// Enter (`true`) or leave studio mode (ADR-0061): the preview scene starts as the
    /// program scene. A live action.
    SetStudioMode(bool),
    /// Move the preview scene to air (a cut, ADR-0061); the preview keeps its scene, or
    /// with [`Command::SetSwapAfterTransition`] gets the old program scene (ADR-0098).
    /// Refused outside studio mode. A live action.
    Transition,
    /// Whether a transition also puts the old program scene in preview (ADR-0098; the
    /// app setting `swap_after_transition`, off by default).
    SetSwapAfterTransition(bool),
    /// Keep a source's device open, or release it (one undo step). Hiding does not
    /// do this.
    SetSourceActive {
        /// The source.
        id: SourceId,
        /// Open while a scene that places it is on air or in preview.
        active: bool,
    },
    /// Show or hide a source in the program scene (one undo step).
    SetSourceVisible {
        /// The source.
        id: SourceId,
        /// Shown.
        visible: bool,
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
        /// The canvas whose layout changes: the main one, or the vertical one, where
        /// the box becomes the item's own layout there (ADR-0093 decision 4, U15).
        canvas: OutputCanvas,
    },
    /// Cut source pixels from the edges of a video source's picture (ADR-0058), on the
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
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Flip, turn, fit or centre a video source's picture on the layout the canvas
    /// shows (ADR-0084). One undo step; a refusal is an [`Update::Error`].
    TransformSource {
        /// The source.
        id: SourceId,
        /// What to do.
        op: TransformOp,
        /// Moves that share a non-zero gesture are one undo step. 0 never joins.
        gesture: u64,
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Show a part of a video source's picture filling as much of the canvas as fits
    /// (ADR-0058, Properties' Show buttons). One undo step.
    SetSourceRegion {
        /// The source.
        id: SourceId,
        /// Which part.
        region: CropRegion,
    },
    /// Capture region on the source (ADR-0103). One undo step; does not restart capture.
    SetSourceCaptureRegion {
        /// The source.
        id: SourceId,
        /// The region, or `None` to clear.
        region: Option<SourceRegion>,
    },
    /// Display chrome insets (ADR-0104). One undo step.
    SetDisplayChrome {
        /// The source.
        id: SourceId,
        /// The chrome, or `None` to turn it off.
        chrome: Option<DisplayChrome>,
    },
    /// Places the item with the stored chrome fill (ADR-0104).
    FitDisplayChrome(SourceId),
    /// Copies chrome from the display work area (ADR-0104).
    ResetDisplayChrome(SourceId),
    /// How long Show draws a hidden item (ADR-0105). One undo step.
    SetItemShowFor {
        /// The scene item.
        item: ItemId,
        /// Milliseconds, or `None` to clear.
        show_for_ms: Option<u32>,
    },
    /// Live timed show until `now + show_for_ms` (ADR-0105).
    ShowItem(ItemId),
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
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Start recording the program ([`Update::Recording`] says how it went).
    StartRecording(RecordRequest),
    /// Stop the running recording; the file is complete when
    /// [`RecordingUpdate::Stopped`] arrives.
    StopRecording,
    /// Start recording the vertical canvas ([`Update::VerticalRecording`] says how it
    /// went; ADR-0093 decision 6).
    StartVerticalRecording(RecordRequest),
    /// Stop the vertical canvas's recording.
    StopVerticalRecording,
    /// Go live with the vertical canvas ([`Update::VerticalStream`] says how it went);
    /// its key is read from the vertical account ([`stream_key::vertical_account`]).
    StartVerticalStream(StreamRequest),
    /// End the vertical canvas's stream.
    EndVerticalStream,
    /// The vertical canvas's recording settings as saved (`None`: it uses the main
    /// ones). Sent at start and after each change.
    SetVerticalProfile(Option<RecordProfile>),
    /// The vertical canvas's stream settings as saved (`None`: none saved). Sent at
    /// start and after each change.
    SetVerticalStreamProfile(Option<StreamProfile>),
    /// Whether Record and Go live also act on the vertical outputs (ADR-0111), as the
    /// output profile saved them. Sent at start and after each change.
    SetVerticalFlags(VerticalFlags),
    /// The vertical canvas's panel shows (`true`) or not (ADR-0093 decision 7): its
    /// picture runs only while it shows, so a closed panel costs nothing. Kept while
    /// there is no vertical canvas, for the next one.
    SetVerticalPicture(bool),
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
    /// Start sending the program to the virtual camera (ADR-0066 decision 12) at this
    /// size and rate; answered with [`Update::VirtualCamera`], and an
    /// [`Update::Error`] when it did not start.
    StartVirtualCamera {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Frames per second.
        fps: FrameRate,
    },
    /// Stop sending to the virtual camera; answered with [`Update::VirtualCamera`].
    StopVirtualCamera,
    /// The virtual camera as Settings › Virtual camera knows it: what `vcam.start` and
    /// `vcam.status` use for control API clients. Sent at start and after each change.
    SetVirtualCamera(CameraSetup),
    /// The replay buffer's settings as saved (ADR-0088): used when it arms. Sent at
    /// start and after each change; a buffer that runs keeps the settings it started
    /// with until it is armed again.
    SetReplay(crate::replay_settings::ReplaySetup),
    /// Arm the replay buffer; answered with [`Update::Replay`], and an
    /// [`Update::Error`] when it did not start.
    ArmReplay,
    /// Disarm the replay buffer and free what it held; answered with [`Update::Replay`].
    DisarmReplay,
    /// Save the last `seconds` (all of the buffer for `None`) as a clip file; answered
    /// with [`Update::ReplaySaved`] once it is written.
    SaveReplay {
        /// How much of the past, or `None` for all of it.
        seconds: Option<u32>,
    },
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
    /// Save the vertical stream's key (`None` removes it) under the vertical account
    /// ([`stream_key::vertical_account`]); answered with [`Update::VerticalStreamKey`].
    SaveVerticalStreamKey(Option<Secret>),
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
    /// The output profiles and scene collections with the ones in use, for
    /// `profile.list` and `collection.list`. Sent at start and after every change of
    /// the libraries or of the ones in use; after an [`Update::SwitchRequested`] once
    /// the switch is done or refused (for an output profile after its settings and
    /// key account were sent, for a scene collection after the studio answered).
    SetLibrary {
        /// The output profiles.
        profiles: api::Library,
        /// The scene collections.
        collections: api::Library,
        /// The `seq` of the [`Update::SwitchRequested`] this answers, if any.
        answered: Option<u64>,
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
    /// `codec.video.encode` values (`h264`, `hevc`). Empty when the probe said nothing.
    pub codecs: Vec<String>,
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
            codecs: set(&codec_caps::VIDEO_ENCODE),
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
            for c in &o.codecs {
                if !u.codecs.contains(c) {
                    u.codecs.push(c.clone());
                }
            }
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
    /// Encoded picture size (ADR-0102). `None`: this output's canvas.
    pub size: Option<(u32, u32)>,
}

/// The canvas an output records or streams (ADR-0093 decision 6).
pub use rbs_api::types::OutputCanvas;

/// `c`'s place in the host's per-canvas state.
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
    /// The running stream: its URL, the connection last reported and its output.
    streaming: Option<(String, Link, OutputId)>,
    /// The recording, as the control API reports it.
    record_state: api::RecordState,
    /// The stream, as the control API reports it.
    stream_state: api::StreamState,
}

impl Default for CanvasOutputs {
    fn default() -> Self {
        Self {
            recording: None,
            ending: false,
            auto_recording: false,
            streaming: None,
            record_state: api::RecordState::Idle { last: None },
            stream_state: api::StreamState::Idle { last: None },
        }
    }
}

impl CanvasOutputs {
    /// A recording or a stream runs.
    fn busy(&self) -> bool {
        self.recording.is_some() || self.streaming.is_some()
    }
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
            size: p.output_size.map(|s| s.pair()),
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

/// A scene the canvas may show, as the Scenes panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneRow {
    /// Its id.
    pub id: SceneId,
    /// Its name.
    pub name: String,
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
}

/// The scenes and sources of the collection, as the Scenes panel and the source
/// pickers show them (ADR-0059).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneList {
    /// The scenes the canvas may show, in panel order.
    pub scenes: Vec<SceneRow>,
    /// The one on air.
    pub program: SceneId,
    /// The preview scene in studio mode (ADR-0061); `None` outside it.
    pub preview: Option<SceneId>,
    /// The canvas's master volume and mute (ADR-0081).
    pub master: MasterAudio,
    /// Collection morph defaults (ADR-0100).
    pub glide: rbs_scene::CollectionGlide,
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
            SceneError::DuplicateName(_) => Self::Conflict(edit_error(e)),
            SceneError::EmptyName
            | SceneError::InvalidVolume
            | SceneError::InvalidSyncOffset
            | SceneError::InvalidShowFor
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

/// The main display's framebuffer, for the Video settings row (ADR-0102 decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainDisplay {
    /// Physical pixels. Not multiplied by [`Self::scale`].
    pub size: (u32, u32),
    /// OS pixels per logical point, when the adapter read it. `None` when it did not
    /// (the label then has no percent).
    pub scale: Option<rbs_capture::DisplayScale>,
}

/// What the studio tells the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The monitor output (ADR-0097): device, volume, whether it is wanted and plays,
    /// its latency, solo and problem. Sent once at the start and when it changes.
    Monitor(api::MonitorStatus),
    /// The playback devices, answering [`Command::ListMonitorDevices`].
    MonitorDevices(Vec<api::MonitorDevice>),
    /// The second canvas runs (`Some`) or does not (`None`), sent when it changes
    /// (ADR-0093) and once at the start.
    Vertical(Option<VerticalState>),
    /// The scenes, the one on air and the collection's sources, after any change of
    /// them (ADR-0059).
    Scenes(SceneList),
    /// The studio mixer's master meter, when the studio has one and each time it is
    /// made again (ADR-0081); `None` while there is no mixer.
    MasterMeter(Option<Arc<rbs_audio::LevelMeter>>),
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
        /// The main display's framebuffer in physical pixels, when known, and its OS
        /// scale for the Video settings label (ADR-0102). The canvas uses `size` and
        /// is not multiplied by the scale.
        display: Option<MainDisplay>,
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
    /// The program scene's sources, top first: what is heard and recorded, for the
    /// Mixer. The same as [`Update::Sources`] outside studio mode (ADR-0061).
    Heard(Vec<SourceRow>),
    /// Where the UI connects to the preview scene's picture in studio mode (ADR-0061),
    /// each time it appears (`Some`) or goes (`None`).
    ScenePreview(Option<PreviewEndpoint>),
    /// Where the UI connects to the vertical canvas's picture (ADR-0093 decision 7),
    /// each time it appears (`Some`) or goes (`None`).
    VerticalPreview(Option<PreviewEndpoint>),
    /// The vertical canvas's picture did not start; the UI asks again only after its
    /// panel was hidden and shown.
    VerticalPictureFailed,
    /// What a kind can capture, in OS order.
    Targets(SourceKind, Vec<Target>),
    /// One target's picture ([`Command::GrabThumbnails`]); `None` when the adapter has
    /// none for it (the row keeps its icon).
    Thumbnail {
        /// The adapter that listed the target.
        adapter: String,
        /// The target.
        target: String,
        /// The picture, if one could be made.
        picture: Option<rbs_capture::Thumbnail>,
    },
    /// What the Source settings window of a source shows ([`Command::OpenSourceSettings`]).
    SourceSettings(SourceSettingsView),
    /// Region-section preview grab (ADR-0103); `None` when the adapter cannot grab.
    RegionPreview {
        /// The source.
        id: SourceId,
        /// Generation so a closed window drops a late answer.
        generation: u64,
        /// The grab, when one was made.
        picture: Option<rbs_capture::RegionGrab>,
    },
    /// A source was deleted, or (`from_scene`) removed from one scene;
    /// [`Command::UndoDelete`] brings it back.
    Deleted {
        /// The source's name.
        name: String,
        /// Only its item in one scene went (Remove from scene, ADR-0065).
        from_scene: bool,
    },
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
    /// The vertical canvas's recording started, did not start or stopped (ADR-0093
    /// decision 6).
    VerticalRecording(RecordingUpdate),
    /// The vertical canvas's stream started, did not start, changed its connection
    /// or ended.
    VerticalStream(StreamUpdate),
    /// [`Command::SaveStreamKey`] done: `Ok(true)` a key is saved, `Ok(false)` none is,
    /// `Err` why the store refused.
    StreamKey(Result<bool, String>),
    /// [`Command::SaveVerticalStreamKey`] done, as [`Update::StreamKey`].
    VerticalStreamKey(Result<bool, String>),
    /// A control API client changed the recording settings; the UI shows and saves
    /// them.
    Profile(RecordProfile),
    /// The control API changed whether Record and Go live also act on the vertical
    /// outputs (`output.set_with_vertical`, ADR-0111); the UI saves them in the profile.
    VerticalFlags(VerticalFlags),
    /// A control API client changed the stream settings (not the server or the key);
    /// the UI shows and saves them.
    StreamProfile(StreamProfile),
    /// A control API client (`rbs-cli token` over local IPC) made or revoked an access
    /// token: it is saved and in effect; the UI reads the token list again.
    TokensChanged,
    /// The running outputs' counters for the Stats panel, once a second while any
    /// output runs.
    Stats(Stats),
    /// The virtual camera started or stopped sending (`None`), and its counters once a
    /// second while it sends.
    VirtualCamera(Option<CameraStatus>),
    /// The replay buffer's state: armed or not, what it holds, and whether a clip is
    /// being saved. Sent when it changes and once a second while armed.
    Replay(replay::ReplayState),
    /// A saved clip's file, or why it could not be saved.
    ReplaySaved(Result<std::path::PathBuf, String>),
    /// [`Command::SwitchCollection`] with this `seq` is done: `Ok` the new collection
    /// runs, `Err` why the one before still runs.
    CollectionSwitched {
        /// The request's number.
        seq: u64,
        /// How it went.
        result: Result<(), String>,
    },
    /// A control API client asked to switch output profiles (`collection` false) or
    /// scene collections to the entry `id` (`profile.switch`, `collection.switch`);
    /// the UI switches as its File menu does, then answers with
    /// [`Command::SetLibrary`].
    SwitchRequested {
        /// The request's number, echoed in [`Command::SetLibrary`]'s `answered`.
        seq: u64,
        /// Scene collections, else output profiles.
        collection: bool,
        /// The entry's id.
        id: String,
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

/// The virtual camera while RBS sends to it ([`Update::VirtualCamera`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraStatus {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second, rounded.
    pub rate: u32,
    /// What its session did with the frames, when the adapter shares its counters.
    pub counters: Option<VcamStats>,
    /// Why its node failed, until it runs again.
    pub failure: Option<String>,
}

/// The virtual camera as the app knows it ([`Command::SetVirtualCamera`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CameraSetup {
    /// Whether it is added; `None` while the app has not read it yet.
    pub state: Option<InstallState>,
    /// The saved width in pixels.
    pub width: u32,
    /// The saved height in pixels.
    pub height: u32,
    /// The saved frame rate.
    pub fps: FrameRate,
    /// Settings is adding, removing or reformatting the camera now: no start.
    pub busy: bool,
}

/// The platform's virtual camera ([`HostConfig::virtual_camera`]).
#[derive(Clone)]
pub struct CameraFactory(pub Arc<dyn VirtualCameraFactory>);

impl std::fmt::Debug for CameraFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CameraFactory").field(&self.0.id()).finish()
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
    /// The config folder where the recent targets are kept
    /// ([`crate::recent_targets`]); `None`: they are kept in memory only.
    pub recent_dir: Option<PathBuf>,
    /// A transition also puts the old program scene in preview (ADR-0098); false by
    /// default.
    pub swap_after_transition: bool,
    /// The monitor output's device and volume (ADR-0097; `monitor.toml`). The device
    /// is opened only while a source is monitored or soloed.
    pub monitor: MonitorSettings,
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
            recent_dir: None,
            swap_after_transition: false,
            monitor: MonitorSettings::default(),
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
    tx: Option<Arc<Sender<Msg>>>,
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
        let tx = Arc::new(tx);
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
    inbox: Weak<Sender<Msg>>,
    /// Saves the collection after a change ([`HostConfig::autosave`]).
    autosave: Option<CollectionSaver>,
    /// The recent targets per source type and the folder they are saved in.
    recent: RecentTargets,
    recent_dir: Option<PathBuf>,
    /// A transition also puts the old program scene in preview (ADR-0098).
    swap_after_transition: bool,
    /// The generation of the thumbnail request that may still deliver (ADR-0077).
    thumbs: Arc<AtomicU64>,
    /// The generation of the region-preview grab that may still deliver (ADR-0103).
    region_grabs: Arc<AtomicU64>,
    /// The collection as last handed to `autosave`.
    saved: Option<SceneCollection>,
    studio: Studio,
    adapters: RecordAdapters,
    collection: SceneCollection,
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
    /// The preview scene's picture endpoint last sent to the UI.
    scene_preview_sent: Option<PreviewEndpoint>,
    /// The vertical canvas's panel shows: its picture runs (ADR-0093 decision 7).
    vertical_picture: bool,
    vertical_preview_sent: Option<PreviewEndpoint>,
    /// The scene list as last sent.
    scenes_shown: Option<SceneList>,
    /// The second canvas that runs, and what was last sent about it (ADR-0093).
    vertical: Option<VerticalState>,
    /// The saved vertical canvas could not start again; told once at the next publish.
    vertical_restore_error: Option<String>,
    /// Whether Record and Go live also act on the vertical outputs (ADR-0111), as the
    /// UI last sent them.
    vertical_flags: VerticalFlags,
    /// Scenes `add_vertical` made shared, so removing the canvas can scope them back.
    vertical_shared: Vec<SceneId>,
    vertical_sent: Option<Option<VerticalState>>,
    /// The master meter last sent to the UI.
    master_sent: Option<Arc<rbs_audio::LevelMeter>>,
    /// Meters published for `audio.levels`.
    levels: Arc<crate::levels::LevelFeed>,
    /// The last picture size seen per video source (ADR-0058).
    sizes: BTreeMap<SourceId, (u32, u32)>,
    /// The user's undo history of scene edits, shared with control API clients.
    history: History<Undo>,
    /// The next undo and redo as last sent.
    history_shown: (Option<StepInfo>, Option<StepInfo>),
    waiting: bool,
    /// The recording and stream of each canvas, by [`slot`]
    /// (ADR-0093 decision 6).
    outs: [CanvasOutputs; 2],
    /// Where the stream key is kept.
    keys: Arc<dyn KeyStore>,
    /// The account of the output profile in use.
    key_account: String,
    /// Makes an engine for a restart.
    engine: Box<dyn Fn() -> Engine>,
    gpu: Arc<GpuContext>,
    /// The browser helper tree when `rbs-browser-helper` was found (ADR-0109).
    browser_tree: Option<Arc<dyn BrowserTree>>,
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
    /// The stream settings as the UI uses them; `None` until the UI sent them.
    stream_profile: Option<StreamProfile>,
    /// The vertical canvas's recording settings as saved; `None`: it uses `profile`.
    vertical_profile: Option<RecordProfile>,
    /// The vertical canvas's stream settings as saved; `None`: none saved, so the
    /// vertical stream has no server.
    vertical_stream_profile: Option<StreamProfile>,
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
    /// The camera as the app's settings know it; `None` until the app sent it.
    camera_setup: Option<CameraSetup>,
    /// The `vcam.changed` state last published.
    camera_published: Option<api::VcamStatus>,
    /// The replay buffer's settings and state (ADR-0088).
    replay: replay::ReplayHost,
}

/// What the control thread is handed besides the studio configuration.
struct ControlIo {
    rx: Receiver<Msg>,
    inbox: Weak<Sender<Msg>>,
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
                    Msg::Ui(_)
                    | Msg::ApiWake
                    | Msg::PickFailed(_)
                    | Msg::ReplayWritten(_)
                    | Msg::Thumbnail { .. }
                    | Msg::RegionPreview { .. } => {}
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
        Command::StartVerticalRecording(_) => Some(Update::VerticalRecording(
            RecordingUpdate::NotStarted(why()),
        )),
        Command::StartVerticalStream(_) => {
            Some(Update::VerticalStream(StreamUpdate::NotStarted(why())))
        }
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
        host.end_outputs(&why, notify);
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
        host.end_outputs(&why, notify);
        notify(Update::Failed(why));
        return wait;
    }
    // A replacement that waited for its predecessor opens now.
    let waiting = host.studio.waiting();
    if host.waiting && !waiting {
        host.apply(notify);
    }
    host.waiting = waiting;
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
                wait = wait.min(ENDING_WAIT);
            }
        }
        if let Some((_, _, id)) = &host.outs[slot(c)].streaming {
            if let Some(why) = host.studio.output_failure(*id) {
                host.end_stream(c, Some(why), notify);
            } else {
                host.report_link(c, notify);
            }
        }
    }
    let now = host.clock.now();
    if let Some(left) = host.report_stats(now, notify) {
        wait = wait.min(left);
    }
    if let Some(left) = host.report_camera(now, notify) {
        wait = wait.min(left);
    }
    if let Some(left) = host.drive_monitor(now) {
        wait = wait.min(left);
    }
    host.report_monitor(notify);
    if let Some(left) = host.report_replay(now, notify) {
        wait = wait.min(left);
    }
    // One wake for the soonest timed-show deadline. An empty cue map adds nothing
    // (ADR-0105, P13).
    if host.studio.expire_cues(now) {
        host.apply(notify);
    }
    if let Some(left) = host.studio.cue_wait(now) {
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
    levels: Arc<crate::levels::LevelFeed>,
) -> Result<(Host, Update), String> {
    let keys = Arc::clone(&config.keys);
    #[cfg(feature = "fault-injection")]
    let config_fault = config.fault;
    let (mut adapters, mut notes) = adapters();
    adapters.probe();
    let can_monitor = adapters.sources.can_monitor();
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
    let browser_tree: Option<Arc<dyn BrowserTree>> = crate::browser_tree::HelperBrowserTree::new()
        .ok()
        .map(|tree| Arc::new(tree) as Arc<dyn BrowserTree>);
    let mut settings = StudioSettings::new(canvas);
    // Width and height stay as configured. The ring follows the canvas that runs:
    // a saved collection can use another rate than the one the descriptor was built with.
    settings.preview = config.preview.map(|preview| PreviewDesc {
        surfaces: preview_surfaces(run_fps),
        ..preview
    });
    settings.scene_preview = SCENE_PICTURE;
    settings.audio_offsets = filters::audio_offsets(&collection);
    let held = held_copy(&collection, canvas);
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
    let screens = adapters
        .sources
        .targets(&SourceKind::Screen.type_id())
        .unwrap_or_default();
    let display = screens
        .iter()
        .find(|t| t.is_default)
        .or_else(|| screens.first())
        .and_then(|t| {
            t.size.map(|size| MainDisplay {
                size,
                scale: t.scale,
            })
        });
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
            ]
            .into_iter()
            .filter(|k| installed.contains(&k.type_id())),
        )
        .filter(|k| *k != SourceKind::Browser || browser_tree.is_some())
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
    let mut host = Host {
        studio,
        adapters,
        collection,
        canvas,
        scene,
        rows: Vec::new(),
        heard: Vec::new(),
        globals_shown: Vec::new(),
        into: None,
        held,
        scene_preview_sent: None,
        vertical_picture: false,
        vertical_preview_sent: None,
        scenes_shown: None,
        vertical: None,
        vertical_restore_error: None,
        vertical_flags: VerticalFlags::default(),
        vertical_shared: Vec::new(),
        vertical_sent: None,
        master_sent: None,
        sizes: BTreeMap::new(),
        history: History::new(),
        history_shown: (None, None),
        waiting: false,
        outs: [CanvasOutputs::default(), CanvasOutputs::default()],
        stats_due: None,
        clock,
        camera_factory: config.virtual_camera.map(|f| f.0),
        camera: None,
        camera_due: None,
        monitor: monitor::MonitorHost::new(config.monitor, can_monitor),
        camera_setup: None,
        camera_published: None,
        replay: replay::ReplayHost::new(),
        keys,
        key_account: config.key_account,
        engine: Box::new(engine),
        gpu,
        browser_tree,
        preview_surfaces: settings.preview.map(|p| p.surfaces),
        dead: None,
        restarting: false,
        caught: false,
        unanswered: None,
        #[cfg(feature = "fault-injection")]
        fault: config_fault,
        autosave: config.autosave,
        recent: config
            .recent_dir
            .as_deref()
            .map(|d| crate::recent_targets::load(d).0)
            .unwrap_or_default(),
        recent_dir: config.recent_dir,
        swap_after_transition: config.swap_after_transition,
        thumbs: Arc::new(AtomicU64::new(0)),
        region_grabs: Arc::new(AtomicU64::new(0)),
        // A collection that had to be completed is saved at the first publish.
        saved: loaded,
        events: EventPublisher::new(),
        api: None,
        profile: RecordProfile::new(),
        kinds,
        encoders,
        cannot_record,
        stream_profile: None,
        vertical_profile: None,
        vertical_stream_profile: None,
        library: None,
        switch_pending: None,
        switch_seq: 0,
        inbox: Weak::new(),
        levels,
    };
    // A vertical canvas saved with the collection runs again (ADR-0093 decision 9).
    host.restore_vertical();
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
        match c {
            Command::ListTargets(kind) => {
                let targets = self.targets(kind);
                notify(Update::Targets(kind, targets));
            }
            Command::AddSourceWithAudio { kind, target } => {
                let done = self
                    .add(kind, &target)
                    .map_err(Refusal::Failed)
                    .and_then(|id| {
                        self.add_companion(id, true).inspect_err(|_| {
                            let _ = self.delete(id);
                        })
                    });
                match done {
                    Ok(_) => self.apply(notify),
                    Err(e) => notify(Update::Error(e.text().to_owned())),
                }
            }
            Command::AddSource { kind, target } if target.picker => self.pick(kind, &target),
            Command::AddSource { kind, target } => match self.add(kind, &target) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e)),
            },
            Command::DeleteSource(id) => match self.delete(id) {
                Ok(name) => {
                    self.apply(notify);
                    notify(Update::Deleted {
                        name,
                        from_scene: false,
                    });
                }
                Err(e) => notify(Update::Error(e)),
            },
            Command::RemoveFromScene(id) => {
                let done = self
                    .item_of(id)
                    .map_err(Refusal::NotFound)
                    .map(|(_, i)| i.id)
                    .and_then(|item| self.remove_from_scene(item));
                match done {
                    Ok((name, deleted)) => {
                        self.apply(notify);
                        notify(Update::Deleted {
                            name,
                            from_scene: !deleted,
                        });
                    }
                    Err(e) => notify(Update::Error(e.text().to_owned())),
                }
            }
            Command::SetGlobal { id, global } => {
                let done = if global {
                    self.make_global(id).map(|_| true)
                } else {
                    self.unmake_global(id, true).map(|_| true)
                };
                self.answer(done, notify);
            }
            Command::RemoveGlobal(id) => {
                let done = self.unmake_global(id, false).map(|_| true);
                self.answer(done, notify);
            }
            Command::AddGlobalSource { kind, target } => {
                let done = self.add_global(kind, &target).map(|_| true);
                self.answer(done, notify);
            }
            Command::UndoDelete => {
                if self
                    .history
                    .next_undo()
                    .is_some_and(|s| matches!(s.kind, EditKind::Delete | EditKind::Remove))
                {
                    self.step(true, notify);
                }
            }
            Command::Undo => self.step(true, notify),
            Command::Redo => self.step(false, notify),
            Command::MoveSource { id, direction } => match self.move_source(id, direction) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::MoveSourceTo { id, to } => match self.move_source_to(id, to) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::MoveScene { id, direction } => match self.move_scene(id, direction) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::MoveSceneTo { id, to } => match self.move_scene_to(id, to) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::SetSourceActive { id, active } => match self.set_source_active(id, active) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e)),
            },
            Command::SetSourceLocked { id, locked } => {
                let done = self
                    .item_of(id)
                    .map_err(Refusal::NotFound)
                    .map(|(_, i)| i.id)
                    .and_then(|item| self.set_item_locked(item, locked));
                self.answer(done, notify);
            }
            Command::SetSourceVolume { id, volume } => {
                self.run_ui_api(
                    rbs_api::Command::SourceSetVolume(rbs_api::request::SetSourceVolume {
                        source: id.get(),
                        volume,
                    }),
                    notify,
                );
            }
            Command::SetSourceMuted { id, muted } => {
                self.run_ui_api(
                    rbs_api::Command::SourceSetMuted(rbs_api::request::SetSourceMuted {
                        source: id.get(),
                        muted,
                    }),
                    notify,
                );
            }
            Command::SetSourceMixerHidden { id, hidden } => {
                self.run_ui_api(
                    rbs_api::Command::SourceSetMixerHidden(
                        rbs_api::request::SetSourceMixerHidden {
                            source: id.get(),
                            hidden,
                        },
                    ),
                    notify,
                );
            }
            Command::AddVerticalCanvas(spec) => {
                let done = self.add_vertical(spec);
                self.answer_live(done, notify);
            }
            Command::RemoveVerticalCanvas => {
                let done = self.remove_vertical();
                self.answer_live(done, notify);
            }
            Command::SetMasterVolume { volume } => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetMasterVolume(rbs_api::request::SetMasterVolume {
                        volume,
                    }),
                    notify,
                );
            }
            Command::SetMasterMuted { muted } => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetMasterMuted(rbs_api::request::SetMasterMuted {
                        muted,
                    }),
                    notify,
                );
            }
            Command::SetSourceMonitor { id, monitor } => {
                let done = self.set_source_monitor(id, monitor);
                self.answer_live(done, notify);
            }
            Command::SetMonitorSolo(sources) => {
                let raw: Vec<u32> = sources.iter().map(|s| s.get()).collect();
                if let Err(e) = self.set_monitor_solo(&raw) {
                    notify(Update::Error(e.text().to_owned()));
                }
            }
            Command::SetMonitorDevice(device) => {
                if let Err(e) = self.set_monitor_device(device) {
                    notify(Update::Error(e.text().to_owned()));
                }
            }
            Command::SetMonitorVolume(volume) => {
                if let Err(e) = self.set_monitor_volume(volume) {
                    notify(Update::Error(e.text().to_owned()));
                }
            }
            Command::ListMonitorDevices => {
                notify(Update::MonitorDevices(self.api_monitor_devices().devices));
            }
            Command::AddFilter { source, type_id } => {
                let done = self
                    .add_filter(source, &type_id, &std::collections::BTreeMap::new())
                    .map(|_| true);
                self.answer(done, notify);
            }
            Command::AddSample { sample } => {
                self.run_ui_api(
                    rbs_api::Command::SourceAddSample(rbs_api::request::AddSample { sample }),
                    notify,
                );
            }
            Command::AddStamp { kind } => match self.add_stamp(kind) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::AddImage { path } => match self.add_image_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::AddMedia { path } => match self.add_media_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::AddBrowser { url } => match self.add_browser_page(&url) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::SetSourceFill { id, color } => match self.set_source_fill(id, &color) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::SetSourceFilled { id, on } => match self.set_source_filled(id, on) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::ApplyLook { source, look } => {
                self.run_ui_api(
                    rbs_api::Command::LookApply(rbs_api::request::ApplyLook {
                        source: source.get(),
                        look,
                    }),
                    notify,
                );
            }
            Command::RemoveFilter(filter) => {
                let done = self.remove_filter(filter).map(|()| true);
                self.answer(done, notify);
            }
            Command::MoveFilter { filter, index } => {
                let done = self.move_filter(filter, index);
                self.answer(done, notify);
            }
            Command::SetFilterEnabled { filter, enabled } => {
                let done = self.set_filter_enabled(filter, enabled);
                self.answer(done, notify);
            }
            Command::SetFilterValues { filter, values } => {
                let done = self.set_filter_values(filter, values);
                // An unchanged release still puts the collection's values back on air.
                if matches!(done, Ok(false)) {
                    self.studio.restore_filter(filter);
                }
                self.answer(done, notify);
            }
            Command::PreviewFilter { filter, values } => {
                self.studio.preview_filter(filter, values);
            }
            Command::RestoreFilter(filter) => {
                self.studio.restore_filter(filter);
            }
            Command::SetSourceSyncOffset { id, offset_ms } => {
                let done = self.set_source_sync_offset(id, offset_ms);
                self.answer_live(done, notify);
            }
            Command::RetargetSource { id, target } => {
                let done = self.retarget_source(id, &target);
                self.answer(done, notify);
            }
            Command::GrabThumbnails { kind, rows, max } => self.grab_thumbnails(kind, rows, max),
            Command::StopThumbnails => {
                self.thumbs.fetch_add(1, AtomicOrdering::AcqRel);
            }
            Command::OpenSourceSettings(id) => match self.source_settings_view(id) {
                Ok(view) => {
                    notify(Update::SourceSettings(view));
                    self.grab_region_preview(id);
                }
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::CloseSourceSettings => {
                self.region_grabs.fetch_add(1, AtomicOrdering::AcqRel);
            }
            Command::SetSourceSettings { id, patch } => {
                let done = self.patch_source_settings(id, &patch);
                self.answer(done, notify);
            }
            Command::ReloadSource(id) => match self.reload_source(id) {
                Ok(Reload::Retargeted) => self.answer(Ok(true), notify),
                Ok(Reload::Restarted(done)) => self.answer_live(Ok(done), notify),
                Err(e) => self.answer_live(Err(e), notify),
            },
            Command::SetCompanionAudio { source, on } => {
                let has = self
                    .collection
                    .sources()
                    .any(|s| s.companion_of() == Some(source));
                let done = match (on, has) {
                    (true, false) => self.add_companion(source, false).map(|_| true),
                    (false, true) => self.remove_companion(source).map(|()| true),
                    _ => Ok(false),
                };
                self.answer(done, notify);
            }
            Command::RenameSource { id, name } => {
                let done = self.rename_source(id, &name);
                self.answer(done, notify);
            }
            Command::SetGlide {
                duration_ms,
                easing,
            } => {
                let done = self.set_glide(duration_ms, easing);
                self.answer(done, notify);
            }
            Command::SetSourceGlide {
                id,
                custom,
                duration_ms,
                easing,
            } => {
                let done = self.set_source_glide(id, custom, duration_ms, easing);
                self.answer(done, notify);
            }
            Command::AddExistingSource(id) => {
                let done = self.add_existing(id).map(|_| true);
                self.answer(done, notify);
            }
            Command::AddScene(name) => {
                let done = self.add_scene(name.as_deref()).map(|_| true);
                self.answer(done, notify);
            }
            Command::DuplicateScene(id) => {
                let done = self.duplicate_scene(id).map(|_| true);
                self.answer(done, notify);
            }
            Command::RemoveScene(id) => {
                let done = self.remove_scene(id).map(|()| true);
                self.answer(done, notify);
            }
            Command::RenameScene { id, name } => {
                let done = self.rename_scene(id, &name);
                self.answer(done, notify);
            }
            Command::SwitchScene(id) => {
                let done = if self.preview().is_some() {
                    self.set_preview_scene(id)
                } else {
                    self.switch_scene(id)
                };
                self.answer(done, notify);
            }
            Command::SetStudioMode(on) => {
                let done = self.set_studio_mode(on);
                self.answer(done, notify);
            }
            Command::Transition => {
                let done = self.transition();
                self.answer(done, notify);
            }
            Command::SetSwapAfterTransition(on) => self.swap_after_transition = on,
            Command::SetSourceVisible { id, visible } => {
                match self.set_source_visible(id, visible) {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e)),
                }
            }
            Command::SetSourceBox {
                seq,
                gesture,
                id,
                position,
                size,
                canvas: OutputCanvas::Vertical,
            } => {
                let ok = match self.set_vertical_box(id, position, size, gesture) {
                    Ok(changed) => {
                        if changed {
                            self.apply(notify);
                        }
                        true
                    }
                    Err(e) => {
                        notify(Update::Error(e.text().to_owned()));
                        false
                    }
                };
                notify(Update::BoxPlaced { seq, ok });
            }
            Command::SetSourceBox {
                seq,
                gesture,
                id,
                position,
                size,
                canvas: OutputCanvas::Main,
            } => {
                // The same handler a remote `item.set_box` runs. The window still
                // matches the answer by `seq`, including a refusal.
                let ok = self.run_ui_api(
                    rbs_api::Command::ItemSetBox(rbs_api::request::SetItemBox {
                        source: id.get(),
                        position,
                        size,
                        gesture,
                        seq,
                        canvas: OutputCanvas::Main,
                    }),
                    notify,
                );
                notify(Update::BoxPlaced { seq, ok });
            }
            Command::SetSourceCrop {
                seq,
                gesture,
                id,
                crop,
                keep_scale,
                canvas,
            } => {
                let [l, t, r, b] = crop;
                let edit = self
                    .item_of(id)
                    .map_err(CropRefusal::NotFound)
                    .map(|(_, item)| item.id)
                    .and_then(|item| {
                        self.set_item_crop(item, Crop::new(l, t, r, b), keep_scale, gesture, canvas)
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
            Command::TransformSource {
                id,
                op,
                gesture,
                canvas,
            } => {
                let edit = match self.item_of(id).map(|(_, item)| item.id) {
                    Ok(item) => self.transform_item(item, op, gesture, canvas),
                    Err(t) => Err(Refusal::NotFound(t)),
                };
                match edit {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e.text().into())),
                }
            }
            Command::SetSourceRegion { id, region } => match self.set_source_region(id, region) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text())),
            },
            Command::SetSourceCaptureRegion { id, region } => {
                match self.set_capture_region(id, region) {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e.text().into())),
                }
            }
            Command::SetDisplayChrome { id, chrome } => match self.set_display_chrome(id, chrome) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().into())),
            },
            Command::FitDisplayChrome(id) => match self.fit_display_chrome(id) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().into())),
            },
            Command::ResetDisplayChrome(id) => match self.reset_display_chrome(id) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().into())),
            },
            Command::SetItemShowFor { item, show_for_ms } => {
                match self.set_item_show_for(item, show_for_ms) {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e.text().into())),
                }
            }
            Command::ShowItem(item) => match self.show_item(item) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().into())),
            },
            Command::SetSourceStyle {
                gesture,
                id,
                style,
                canvas,
            } => {
                let edit = self
                    .item_of(id)
                    .map(|(_, item)| item.id)
                    .and_then(|item| self.set_item_style(item, style, gesture, canvas));
                match edit {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e)),
                }
            }
            Command::StartRecording(r) => self.record_asked(OutputCanvas::Main, &r, notify),
            Command::StartVerticalRecording(r) => {
                self.record_asked(OutputCanvas::Vertical, &r, notify);
            }
            Command::StopRecording => {
                self.end_recording(OutputCanvas::Main, notify);
                self.publish(notify);
            }
            Command::StopVerticalRecording => {
                self.end_recording(OutputCanvas::Vertical, notify);
                self.publish(notify);
            }
            Command::SetVerticalProfile(profile) => self.vertical_profile = profile,
            Command::SetVerticalStreamProfile(profile) => self.vertical_stream_profile = profile,
            Command::SetVerticalFlags(flags) => {
                if flags != self.vertical_flags {
                    self.vertical_flags = flags;
                    self.settings_changed();
                }
            }
            Command::SetVerticalPicture(on) => {
                // Turning the picture off cannot leave anything to report: the node
                // goes with the next update at the latest.
                if let Err(why) = self.set_vertical_picture(on)
                    && on
                {
                    notify(Update::VerticalPictureFailed);
                    notify(Update::Error(why));
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
            Command::StartStream(r) => self.stream_asked(OutputCanvas::Main, &r, notify),
            Command::StartVerticalStream(r) => {
                self.stream_asked(OutputCanvas::Vertical, &r, notify);
            }
            Command::EndStream => {
                self.end_stream_asked(OutputCanvas::Main, notify);
                self.publish(notify);
            }
            Command::EndVerticalStream => {
                self.end_stream_asked(OutputCanvas::Vertical, notify);
                self.publish(notify);
            }
            Command::StartVirtualCamera { width, height, fps } => {
                // An API client started it first: show the camera that runs, no error.
                if self.camera.is_some() {
                    notify(Update::VirtualCamera(self.camera_status()));
                } else {
                    let settings = VirtualCameraSettings::new(width, height, fps);
                    let _ = self.start_camera_asked(settings, notify, true);
                }
            }
            Command::StopVirtualCamera => self.stop_camera(notify),
            Command::SetReplay(setup) => {
                self.replay.setup = setup;
                self.replay_changed(notify);
            }
            Command::ArmReplay => {
                if let Err(why) = self.arm_replay(notify) {
                    notify(Update::Error(why));
                }
            }
            Command::DisarmReplay => self.disarm_replay(notify),
            Command::SaveReplay { seconds } => {
                if let Err(why) = self.save_replay(seconds) {
                    notify(Update::ReplaySaved(Err(why)));
                }
            }
            Command::SetVirtualCamera(setup) => {
                self.camera_setup = Some(setup);
                self.camera_changed();
            }
            Command::SetStreamProfile(profile) => {
                if self.stream_profile.as_ref() != Some(&profile) {
                    self.stream_profile = Some(profile);
                    self.settings_changed();
                }
            }
            Command::SaveStreamKey(key) => {
                let result = self.save_key(OutputCanvas::Main, key.as_ref());
                notify(Update::StreamKey(result));
            }
            Command::SaveVerticalStreamKey(key) => {
                let result = self.save_key(OutputCanvas::Vertical, key.as_ref());
                notify(Update::VerticalStreamKey(result));
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
                // Answered also after a caught panic, so the UI's switch, and a control
                // API switch waiting on it, end; the round then recovers the host.
                let result =
                    rbs_crash::contain(|| self.switch_collection(*collection, autosave, notify))
                        .unwrap_or_else(|_| {
                            self.caught = true;
                            Err(rbs_crash::CAUGHT_TEXT.to_owned())
                        });
                notify(Update::CollectionSwitched { seq, result });
            }
            Command::SetLibrary {
                profiles,
                collections,
                answered,
            } => {
                self.library = Some((profiles, collections));
                if answered.is_some() && answered == self.switch_pending {
                    self.switch_pending = None;
                }
            }
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
        if self.any_recording() {
            let why = "Stop the recording before switching scene collections.".to_owned();
            self.canvas_changed(self.api_canvas(), notify);
            return Err(why);
        }
        if self.any_streaming() {
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
        // A copy of the old collection's program never goes on air in the new one.
        let old_held = self.held.take();
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
                self.held = old_held;
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
        }
    }

    /// The canvases a start or stop with `canvas: "all"` acts on, main first
    /// (ADR-0111 decision 3).
    fn output_canvases(&self) -> Vec<OutputCanvas> {
        let mut all = vec![OutputCanvas::Main];
        if self.vertical.is_some() {
            all.push(OutputCanvas::Vertical);
        }
        all
    }

    fn settings_changed(&mut self) {
        let settings = self.api_settings();
        self.events
            .changed(StateKey::topic(Topic::SettingsChanged), || {
                EventPayload::SettingsChanged(settings)
            });
    }

    fn set_record_state(&mut self, c: OutputCanvas, state: api::RecordState) {
        self.outs[slot(c)].record_state = state.clone();
        match c {
            OutputCanvas::Main => self
                .events
                .changed(StateKey::topic(Topic::RecordState), || {
                    EventPayload::RecordState(state)
                }),
            OutputCanvas::Vertical => self
                .events
                .changed(canvas_key(Topic::RecordCanvasState, c), || {
                    EventPayload::RecordCanvasState(api::CanvasRecordState { canvas: c, state })
                }),
        };
    }

    fn set_stream_state(&mut self, c: OutputCanvas, state: api::StreamState) {
        if state == self.outs[slot(c)].stream_state {
            return;
        }
        self.outs[slot(c)].stream_state = state.clone();
        match c {
            OutputCanvas::Main => self
                .events
                .changed(StateKey::topic(Topic::StreamState), || {
                    EventPayload::StreamState(state)
                }),
            OutputCanvas::Vertical => self
                .events
                .changed(canvas_key(Topic::StreamCanvasState, c), || {
                    EventPayload::StreamCanvasState(api::CanvasStreamState { canvas: c, state })
                }),
        };
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
                r == Some(*id) || s == Some(*id)
            });
            if !known {
                let _ = rbs_crash::contain(|| self.studio.stop_output(*id));
                stopped += 1;
            }
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
        if let Some(v) = self
            .vertical
            .filter(|v| self.collection.canvas(v.canvas).is_some())
        {
            settings.extra_canvases = vec![self.vertical_extra(v.canvas)];
        } else if self.vertical.take().is_some() {
            self.forget_vertical_outputs();
        }
        let run = self.run_collection();
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
        self.waiting = self.studio.waiting();
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
        Ok(())
    }

    /// Starts recording to `r.path`; the file's folder is made first.
    fn start_recording(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        profile: Option<String>,
    ) -> Result<RecordingUpdate, String> {
        if self.outs[slot(c)].recording.is_some() {
            return Err("A recording is already running.".into());
        }
        let canvas = self.output_canvas(c)?;
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
        out.canvas = canvas;
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            a.bitrate_bps = r.audio_bitrate_bps;
            a
        });
        // The size this recording starts with. A later settings change waits for the
        // next start (ADR-0102). `None` stays the canvas, including a side above 7680.
        out.size = r.size;
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
            self.outs[slot(c)].recording = Some((r.path.clone(), *id));
            self.set_record_state(
                c,
                api::RecordState::Recording {
                    profile,
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                },
            );
        } else if std::fs::metadata(&r.path).is_ok_and(|m| m.len() == 0) {
            // The output may have made the file before a later node failed.
            let _ = std::fs::remove_file(&r.path);
        }
        started.map(|(_, update)| update)
    }

    /// The studio canvas `c`'s outputs use (`None`: the main one); refused in words
    /// when there is no vertical canvas.
    fn output_canvas(&self, c: OutputCanvas) -> Result<Option<CanvasId>, String> {
        match c {
            OutputCanvas::Main => Ok(None),
            OutputCanvas::Vertical => self
                .vertical
                .map(|v| Some(v.canvas))
                .ok_or_else(|| "There is no vertical canvas.".to_owned()),
        }
    }

    /// The vertical canvas is gone: its outputs' states go with it (no event), and a
    /// new one starts idle (protocol 1.29). The `seq` counters are kept, so a client
    /// never takes a new canvas's events for older ones.
    fn forget_vertical_outputs(&mut self) {
        self.outs[slot(OutputCanvas::Vertical)] = CanvasOutputs::default();
    }

    /// A recording runs on either canvas.
    fn any_recording(&self) -> bool {
        self.outs.iter().any(|o| o.recording.is_some())
    }

    /// A stream runs on either canvas.
    fn any_streaming(&self) -> bool {
        self.outs.iter().any(|o| o.streaming.is_some())
    }

    /// The canvas a control API call asks for (protocol 1.29): the main one by
    /// default; `not_found` for the vertical one while there is none.
    fn api_canvas_of(&self, asked: Option<OutputCanvas>) -> Result<OutputCanvas, ApiError> {
        let c = asked.unwrap_or_default();
        if c == OutputCanvas::Vertical && self.vertical.is_none() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "there is no vertical canvas",
            ));
        }
        Ok(c)
    }

    /// The `seq` map of `c`'s key on a per-canvas topic.
    fn canvas_seq(&self, topic: Topic, c: OutputCanvas) -> api::SeqMap {
        let key = canvas_key(topic, c);
        api::SeqMap::from([(key.to_string(), self.events.seq(key))])
    }

    /// The recording settings of `c`: the vertical canvas uses the main ones until
    /// its own are saved.
    fn record_profile(&self, c: OutputCanvas) -> &RecordProfile {
        match c {
            OutputCanvas::Main => &self.profile,
            OutputCanvas::Vertical => self.vertical_profile.as_ref().unwrap_or(&self.profile),
        }
    }

    /// The stream settings of `c`, `None` until the UI sent them (the vertical
    /// canvas's: until they are saved).
    fn stream_profile_of(&self, c: OutputCanvas) -> Option<&StreamProfile> {
        match c {
            OutputCanvas::Main => self.stream_profile.as_ref(),
            OutputCanvas::Vertical => self.vertical_stream_profile.as_ref(),
        }
    }

    /// The key store account of `c`'s stream key.
    fn key_account_of(&self, c: OutputCanvas) -> String {
        match c {
            OutputCanvas::Main => self.key_account.clone(),
            OutputCanvas::Vertical => stream_key::vertical_account(&self.key_account),
        }
    }

    /// A new file for a recording of `c` with its saved settings, named for now.
    fn recording_file(&self, c: OutputCanvas) -> PathBuf {
        let now = chrono::Local::now().naive_local();
        let p = self.record_profile(c);
        match c {
            OutputCanvas::Main => p.file_for(now),
            OutputCanvas::Vertical => p.vertical_file_for(now),
        }
    }

    /// The UI's Record for `c`: starts the recording and says how it went.
    fn record_asked(&mut self, c: OutputCanvas, r: &RecordRequest, notify: &dyn Fn(Update)) {
        let update = match self.start_recording(c, r, Some(API_PROFILE_ID.into())) {
            Ok(update) => update,
            Err(why) => RecordingUpdate::NotStarted(why),
        };
        notify(recording_update(c, update));
        self.publish(notify);
    }

    /// The UI's Go live for `c`: starts the stream, says how it went, and records
    /// with it when that is on.
    fn stream_asked(&mut self, c: OutputCanvas, r: &StreamRequest, notify: &dyn Fn(Update)) {
        let update = match self.start_stream(c, r) {
            Ok(update) => update,
            Err(why) => StreamUpdate::NotStarted(why),
        };
        let started = matches!(update, StreamUpdate::Started { .. });
        notify(stream_update(c, update));
        if started {
            self.record_with_stream(c, notify);
        }
        self.publish(notify);
    }

    /// Saves (`Some`) or removes `c`'s stream key; `Ok(true)` when one is saved now.
    fn save_key(&self, c: OutputCanvas, key: Option<&Secret>) -> Result<bool, String> {
        let account = self.key_account_of(c);
        let result = match key {
            Some(k) => self.keys.write(&account, k).map(|()| true),
            None => self.keys.remove(&account).map(|()| false),
        };
        result.map_err(|e| format!("{} refused the stream key: {e}", self.keys.place()))
    }

    /// Ends every output of both canvases because of `why` (the studio failed).
    fn end_outputs(&mut self, why: &str, notify: &dyn Fn(Update)) {
        for c in OutputCanvas::ALL {
            self.stop_recording(c, Some(why.to_owned()), notify);
            self.end_stream(c, Some(why.to_owned()), notify);
        }
    }

    /// Stops `c`'s recording, if one runs, and says so.
    fn stop_recording(
        &mut self,
        c: OutputCanvas,
        failure: Option<String>,
        notify: &dyn Fn(Update),
    ) {
        let o = &mut self.outs[slot(c)];
        o.ending = false;
        o.auto_recording = false;
        // Tracked until the end: when a stop panics half-way, `Host::recover` finds the
        // recording still tracked and ends it again (ADR-0055 decision 3).
        let Some((path, id)) = o.recording.clone() else {
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
        self.set_record_state(
            c,
            api::RecordState::Idle {
                last: Some(api::RecordingSummary {
                    failure: failure.clone(),
                    frames,
                    dropped,
                }),
            },
        );
        self.outs[slot(c)].recording = None;
        notify(recording_update(
            c,
            RecordingUpdate::Stopped {
                path,
                failure,
                frames,
                dropped,
                flush_lost,
            },
        ));
    }

    /// A recording of `c` to `path` with its saved recording settings, as
    /// `record.start` and "Record when the stream starts" make it; refused in words
    /// when the settings cannot be used.
    fn profile_request(&self, c: OutputCanvas, path: PathBuf) -> Result<RecordRequest, String> {
        let p = self.record_profile(c);
        if let Some(why) = p.problem() {
            return Err(why);
        }
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = p
            .encoder
            .clone()
            .filter(|id| self.encoders.iter().any(|e| e.id == *id));
        Ok(RecordRequest {
            path,
            container: p.format.container(),
            encoder,
            video: p.video_settings(),
            audio_bitrate_bps: u64::from(p.audio_kbps) * 1000,
            // A vertical recording with no section of its own keeps the main encoding
            // and encodes at the vertical canvas: the main size is not copied.
            size: match c {
                OutputCanvas::Main => p.output_size.map(|s| s.pair()),
                OutputCanvas::Vertical => self
                    .vertical_profile
                    .as_ref()
                    .and_then(|v| v.output_size)
                    .map(|s| s.pair()),
            },
        })
    }

    /// Asks the running recording to stop: its video ends now and the loop stops it
    /// once the frames still read back are in (ADR-0045).
    fn end_recording(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        if let Some((_, id)) = &self.outs[slot(c)].recording
            && self.studio.end_output(*id)
        {
            self.outs[slot(c)].ending = true;
        } else {
            self.stop_recording(c, None, notify);
        }
    }

    /// A stream just started: with "Record when the stream starts" on and no
    /// recording running, records the program with the recording settings, as Record
    /// would. The UI is told either way; a recording that cannot start leaves the
    /// stream running.
    fn record_with_stream(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        let on = self.stream_profile_of(c).is_some_and(|p| p.auto_record);
        let o = &self.outs[slot(c)];
        if !on || o.recording.is_some() || o.streaming.is_none() {
            return;
        }
        let started = match &self.cannot_record {
            Some(why) => Err(why.clone()),
            None => self
                .profile_request(c, self.recording_file(c))
                .and_then(|r| self.start_recording(c, &r, Some(API_PROFILE_ID.into()))),
        };
        let update = match started {
            Ok(update) => {
                self.outs[slot(c)].auto_recording = true;
                update
            }
            Err(why) => RecordingUpdate::NotStarted(why),
        };
        notify(recording_update(c, update));
    }

    /// The user (End stream) or a control API client (`stream.stop`) ends the stream:
    /// a recording that started with it stops too.
    fn end_stream_asked(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        let with_stream = self.outs[slot(c)].auto_recording;
        // A stream that failed before it was asked to end keeps its recording, as one
        // the round finds failed does.
        let failure = self.end_stream(c, None, notify);
        if with_stream && failure.is_none() && self.outs[slot(c)].recording.is_some() {
            self.end_recording(c, notify);
        }
    }

    /// Goes live: reads the key (RTMP: required) from the key store, then attaches the
    /// stream's encoders and output to the running studio. The connection is made in
    /// the background; [`Host::report_link`] tells the UI how it goes.
    fn start_stream(&mut self, c: OutputCanvas, r: &StreamRequest) -> Result<StreamUpdate, String> {
        if self.outs[slot(c)].streaming.is_some() {
            return Err("A stream is already running.".into());
        }
        let canvas = self.output_canvas(c)?;
        if self.cannot_record.is_some() {
            return Err(format!(
                "Streaming is not available on {} yet: this system has no video encoder RBS can use.",
                os_name()
            ));
        }
        let key = self.keys.read(&self.key_account_of(c)).map_err(|e| {
            let which = match c {
                OutputCanvas::Main => "",
                OutputCanvas::Vertical => " of the vertical stream",
            };
            format!(
                "The stream key{which} could not be read from {}: {e}",
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
        out.canvas = canvas;
        out.reconnect = r.reconnect;
        out.transport_options = r.transport_options.clone();
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            a.bitrate_bps = r.audio_bitrate_bps;
            a
        });
        // The size this stream starts with. A later settings change waits for the
        // next start (ADR-0102). `None` stays the canvas.
        out.size = r.size;
        let (id, video_encoder, audio_encoder) = self.start_output(&out, r.encoder.as_deref())?;
        // Before the host tracks it: recovery stops an output a panicked start left
        // running (ADR-0055 decision 3).
        inject!(self, Fault::AfterStreamStart);
        self.outs[slot(c)].streaming = Some((r.url.clone(), Link::default(), id));
        let protocol = match parse_server(&r.url).map(|s| s.protocol) {
            Ok(Protocol::Srt) => api::StreamProtocol::Srt,
            // Only RTMP and SRT servers pass the settings' check.
            _ => api::StreamProtocol::Rtmp,
        };
        self.set_stream_state(
            c,
            api::StreamState::Live {
                protocol,
                connected: false,
                connects: 0,
                reconnect_attempts: 0,
                video_encoder: video_encoder.clone(),
                audio_encoder: audio_encoder.clone(),
            },
        );
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
        let recording = self.outs[0].recording.as_ref().map(|(_, id)| *id);
        let stream = self.outs[0].streaming.as_ref().map(|(_, _, id)| *id);
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

    /// Starts sending to the virtual camera with `settings` for the user or an API
    /// client, and tells the UI and `vcam.changed`; the UI gets the error when
    /// `show_error` (an API client gets it in its answer instead).
    fn start_camera_asked(
        &mut self,
        settings: VirtualCameraSettings,
        notify: &dyn Fn(Update),
        show_error: bool,
    ) -> Result<(), String> {
        let started = self.start_camera(settings);
        match &started {
            Ok(()) => self.camera_due = None,
            Err(why) if show_error => notify(Update::Error(why.clone())),
            Err(_) => {}
        }
        notify(Update::VirtualCamera(self.camera_status()));
        self.camera_changed();
        started
    }

    /// Stops sending to the virtual camera, and tells the UI and `vcam.changed`.
    fn stop_camera(&mut self, notify: &dyn Fn(Update)) {
        self.studio.stop_virtual_camera();
        self.camera = None;
        self.camera_due = None;
        notify(Update::VirtualCamera(None));
        self.camera_changed();
    }

    /// The camera for `vcam.status` (with its counters) and `vcam.changed` (without).
    fn api_camera(&self, frames: bool) -> api::VcamStatus {
        let sending = self.camera_status();
        let (state, reason) = match self.camera_setup.as_ref().map(|s| &s.state) {
            _ if self.camera_factory.is_none() => (
                Some(api::VcamInstall::Unavailable),
                Some("The virtual camera is not available on this computer".to_owned()),
            ),
            None | Some(None) => (None, None),
            Some(Some(InstallState::Installed)) => (Some(api::VcamInstall::Added), None),
            Some(Some(InstallState::NotInstalled)) => (Some(api::VcamInstall::NotAdded), None),
            Some(Some(InstallState::NeedsUpdate { reason })) => {
                (Some(api::VcamInstall::NeedsUpdate), Some(reason.clone()))
            }
            Some(Some(InstallState::Unsupported { reason })) => {
                (Some(api::VcamInstall::Unavailable), Some(reason.clone()))
            }
            // A state a later port version adds: not known to this version.
            Some(Some(_)) => (None, None),
        };
        let (width, height, rate) = match (&sending, &self.camera_setup) {
            (Some(s), _) => (s.width, s.height, s.rate),
            (None, Some(setup)) => (setup.width, setup.height, rounded_rate(setup.fps)),
            (None, None) => (0, 0, 0),
        };
        api::VcamStatus {
            state,
            reason,
            sending: sending.is_some(),
            width,
            height,
            rate,
            frames: sending
                .as_ref()
                .filter(|_| frames)
                .and_then(|s| s.counters)
                .map(|c| api::VcamFrames {
                    sent: c.sent(),
                    replaced: c.replaced(),
                    refused: c.refused(),
                    consumer_busy: c.consumer_busy(),
                    consumer_absent: c.consumer_absent(),
                    format_pending: c.format_pending(),
                    os_errors: c.os_errors(),
                }),
            failure: sending.and_then(|s| s.failure),
        }
    }

    /// Publishes `vcam.changed` when the camera's state differs from the last one.
    fn camera_changed(&mut self) {
        let now = self.api_camera(false);
        if self.camera_published.as_ref() == Some(&now) {
            return;
        }
        self.camera_published = Some(now.clone());
        self.events
            .changed(StateKey::topic(Topic::VcamChanged), || {
                EventPayload::VcamChanged(now)
            });
    }

    /// Starts sending to the virtual camera with `settings`.
    fn start_camera(&mut self, settings: VirtualCameraSettings) -> Result<(), String> {
        let factory = self
            .camera_factory
            .clone()
            .ok_or("The virtual camera is not available on this computer.")?;
        self.studio
            .start_virtual_camera(&*factory, settings)
            .map_err(|e| format!("The virtual camera did not start: {e}"))?;
        self.camera = Some(settings);
        Ok(())
    }

    /// The virtual camera while it sends.
    fn camera_status(&self) -> Option<CameraStatus> {
        let state = self.studio.virtual_camera()?;
        let format = state.config.format;
        Some(CameraStatus {
            width: format.width,
            height: format.height,
            rate: rounded_rate(format.rate),
            counters: state.session.map(|s| s.snapshot()),
            failure: state.failure,
        })
    }

    /// Sends the virtual camera's counters once every [`STATS_INTERVAL`] while it
    /// sends (no timer otherwise: ADR-0013), and says once when it stopped by itself
    /// (it could not start again after a restart). Returns the time until the next.
    fn report_camera(&mut self, now: Timestamp, notify: &dyn Fn(Update)) -> Option<Duration> {
        if self.camera.is_none() {
            if self.camera_due.take().is_some() {
                notify(Update::Error(
                    "The virtual camera stopped: it could not start again with the new canvas."
                        .into(),
                ));
                notify(Update::VirtualCamera(None));
                self.camera_changed();
            }
            return None;
        }
        let due = *self.camera_due.get_or_insert(now);
        if now < due {
            return Some(due.saturating_since(now));
        }
        self.camera_due = Some(now + STATS_INTERVAL);
        notify(Update::VirtualCamera(self.camera_status()));
        // A failure of the camera's node, or its end, is a change of state.
        self.camera_changed();
        Some(STATS_INTERVAL)
    }

    /// Tells the UI when the stream's connection changed.
    fn report_link(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        let Some((_, last, id)) = &mut self.outs[slot(c)].streaming else {
            return;
        };
        // Only what the UI shows: a change of transport drops alone sends nothing.
        let shown = |l: &Link| (l.connected, l.connects, l.reconnect_attempts);
        if let Some(link) = self.studio.link(*id)
            && shown(&link) != shown(last)
        {
            *last = link;
            notify(stream_update(c, StreamUpdate::Link(link)));
            if let api::StreamState::Live {
                protocol,
                video_encoder,
                audio_encoder,
                ..
            } = &self.outs[slot(c)].stream_state
            {
                let state = api::StreamState::Live {
                    protocol: *protocol,
                    connected: link.connected,
                    connects: link.connects,
                    reconnect_attempts: link.reconnect_attempts,
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                };
                self.set_stream_state(c, state);
            }
        }
    }

    /// Ends the stream, if one runs, and says so; returns why it failed, if it did.
    fn end_stream(
        &mut self,
        c: OutputCanvas,
        failure: Option<String>,
        notify: &dyn Fn(Update),
    ) -> Option<String> {
        // Tracked until the end, as in `stop_recording`.
        let (_, _, id) = self.outs[slot(c)].streaming.clone()?;
        let report = self.studio.stop_output(id);
        // Still tracked: recovery ends a stream whose stop panicked half-way.
        inject!(self, Fault::InStreamEnd);
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
        self.outs[slot(c)].streaming = None;
        // A recording started with the stream outlives a stream that failed: it is the
        // copy of what the viewers missed. From now on it is the user's to stop.
        self.outs[slot(c)].auto_recording = false;
        self.set_stream_state(
            c,
            api::StreamState::Idle {
                last: Some(api::StreamSummary {
                    failure: failure.clone(),
                    frames,
                    dropped,
                    connects,
                }),
            },
        );
        notify(stream_update(
            c,
            StreamUpdate::Ended {
                failure: failure.clone(),
                frames,
                dropped,
                connects,
            },
        ));
        failure
    }

    /// Adds a source of `kind` for `target` to the program scene, on top.
    /// Puts a target first among the recent ones of its source type and saves the list
    /// (a small file, written only when the list changes). An empty `name` is looked up
    /// in the listed targets; a target that is not listed is not remembered.
    fn remember_target(&mut self, kind: SourceKind, adapter: &str, target: &str, name: &str) {
        let name = if name.is_empty() {
            match self
                .targets(kind)
                .into_iter()
                .find(|t| t.adapter == adapter && t.target == target)
            {
                Some(t) => t.name,
                None => return,
            }
        } else {
            name.to_owned()
        };
        let changed = self
            .recent
            .push(kind.type_id().as_str(), adapter, target, &name);
        if changed && let Some(dir) = &self.recent_dir {
            // A list that cannot be saved is still kept for this run.
            let _ = crate::recent_targets::save(dir, &self.recent);
        }
    }

    /// The recent targets of `kind` that still exist, newest first.
    fn recent_targets(&self, kind: SourceKind) -> Vec<Target> {
        let mut list: Vec<Target> = self
            .targets(kind)
            .into_iter()
            .filter(|t| t.recent.is_some())
            .collect();
        list.sort_by_key(|t| t.recent);
        list
    }

    /// Forgets the recent targets of `kind` and saves the list.
    fn clear_recent_targets(&mut self, kind: SourceKind) {
        if self.recent.clear(kind.type_id().as_str())
            && let Some(dir) = &self.recent_dir
        {
            let _ = crate::recent_targets::save(dir, &self.recent);
        }
    }

    fn add(&mut self, kind: SourceKind, target: &Target) -> Result<SourceId, String> {
        // A screen or window is as large as fits with its own aspect ratio, centred
        // (a 21:9 display is not squeezed into a 16:9 canvas); audio has no picture,
        // the item only places it in the scene.
        let (settings, layout) = self.new_source(kind, target)?;
        let name = unique_name(&self.collection, &target.name);
        let id = self.add_named(kind, &name, settings, layout)?;
        self.remember_target(kind, &target.adapter, &target.target, &target.name);
        // Hints are not a step of their own: undoing the add removes the source. The
        // source was just added, so the write cannot fail; a lost hint only costs a
        // later Reload by hints.
        if let Some(source) = self.collection.source(id) {
            let settings = source.settings.clone();
            let _ = self
                .collection
                .set_source_settings_and_hints(id, settings, hints_of(target));
        }
        Ok(id)
    }

    /// Settings and layout a new source of `kind` capturing `target` gets.
    fn new_source(&self, kind: SourceKind, target: &Target) -> Result<(Settings, Layout), String> {
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
            _ => Layout::new([0.0, 0.0], [1.0, 1.0]),
        };
        Ok((settings, layout))
    }

    /// The layout a new source of `kind` gets, with no target known.
    fn default_layout(&self, kind: Option<SourceKind>) -> Layout {
        let (w, h) = self
            .collection
            .canvas(self.canvas)
            .map_or((1, 1), |c| (c.size.width, c.size.height));
        match kind {
            Some(SourceKind::Camera) => camera_layout(w, h),
            Some(k) if k.is_audio() => Layout::new([0.0, 0.0], [1.0, 1.0]),
            _ => Layout::new([0.0, 0.0], [w as f32, h as f32]),
        }
    }

    // ----- global sources and safe removal (ADR-0065) -----

    /// The canvas's global scene.
    fn globals(&self) -> Option<SceneId> {
        self.collection.globals_of(self.canvas)
    }

    /// A scene the UI and the API list: it exists and is not a global scene.
    fn listed(&self, id: SceneId) -> bool {
        self.collection.scene(id).is_some() && !self.collection.is_global_scene(id)
    }

    /// The global item of a source on the studio's canvas.
    fn global_item(&self, id: SourceId) -> Option<ItemId> {
        let scene = self.collection.scene(self.globals()?)?;
        scene
            .items
            .iter()
            .find(|i| i.content == ItemContent::Source(id))
            .map(|i| i.id)
    }

    /// The canvas's global scene, made (with its records) when it has none yet.
    fn ensure_globals(&mut self, records: &mut Vec<Undo>) -> Result<SceneId, Refusal> {
        if let Some(g) = self.globals() {
            return Ok(g);
        }
        let name = unique_scene_name(&self.collection, "Global sources");
        let (id, undo) = self
            .collection
            .add_scene(&name, SceneScope::Canvas(self.canvas))
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        let undo = self
            .collection
            .set_globals(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        Ok(id)
    }

    /// Puts back what `records` did, newest first, after a step failed half-way.
    fn roll_back(&mut self, records: Vec<Undo>) {
        for r in records.into_iter().rev() {
            let _ = self.collection.undo(r);
        }
    }

    /// Scenes reachable from `scene` through nesting, itself included.
    fn reachable(&self, scene: SceneId) -> std::collections::BTreeSet<SceneId> {
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = vec![scene];
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            if let Some(sc) = self.collection.scene(s) {
                stack.extend(sc.items.iter().filter_map(|i| match i.content {
                    ItemContent::Scene(n) => Some(n),
                    _ => None,
                }));
            }
        }
        seen
    }

    /// Remove from scene (ADR-0065 decision 7): the item goes; the source too when it
    /// was its last placement. Returns the source's name and whether it was deleted.
    fn remove_from_scene(&mut self, item: ItemId) -> Result<(String, bool), Refusal> {
        let gone = || Refusal::NotFound(format!("There is no item {}.", item.get()));
        let (_, it) = self.collection.item(item).ok_or_else(gone)?;
        let ItemContent::Source(source) = it.content else {
            return Err(Refusal::Invalid(
                "Only a source can be removed from a scene here.".to_owned(),
            ));
        };
        let name = self.name_of(source);
        let last = self.collection.placements_of(source).len() <= 1;
        let (kind, undo) = if last {
            (EditKind::Delete, self.collection.delete_source(source))
        } else {
            (EditKind::Remove, self.collection.remove_item(item))
        };
        let undo = undo.map_err(|e| Refusal::of(&e))?;
        self.history.record(kind, &name, 0, vec![undo]);
        Ok((name, last))
    }

    /// Makes a source global on the studio's canvas (ADR-0065 decision 5): one item in
    /// the global scene, its items in the canvas's scenes removed; one undo step.
    fn make_global(&mut self, id: SourceId) -> Result<ItemId, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        if self.global_item(id).is_some() {
            return Err(Refusal::Conflict(format!(
                "{name} is already a global source."
            )));
        }
        let globals = self.globals();
        let leaving: Vec<(SceneId, ItemId)> = self
            .collection
            .placements_of(id)
            .into_iter()
            .filter(|(scene, _)| {
                Some(*scene) != globals
                    && self
                        .collection
                        .showable_scenes(self.canvas)
                        .into_iter()
                        .any(|s| s.id == *scene)
            })
            .collect();
        // No other canvas may lose a source it shows.
        for c in self.collection.canvases().filter(|c| c.id != self.canvas) {
            for shown in [c.program, c.preview].into_iter().flatten() {
                let reach = self.reachable(shown);
                if leaving.iter().any(|(s, _)| reach.contains(s)) {
                    return Err(Refusal::Conflict(format!(
                        "{name} is on air on {}.",
                        c.name
                    )));
                }
            }
        }
        let edited = self.edited();
        let layout = leaving
            .iter()
            .find(|(s, _)| *s == edited)
            .or_else(|| leaving.first())
            .and_then(|(_, item)| self.collection.item(*item))
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| self.default_layout(kind));
        let mut records = Vec::new();
        let done = (|| {
            let g = self.ensure_globals(&mut records)?;
            for (_, item) in &leaving {
                let undo = self
                    .collection
                    .remove_item(*item)
                    .map_err(|e| Refusal::of(&e))?;
                records.push(undo);
            }
            let (item, undo) = self
                .collection
                .add_item(
                    g,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(item)
        })();
        match done {
            Ok(item) => {
                self.history.record(EditKind::Global, &name, 0, records);
                Ok(item)
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// Takes a source out of the global sources: with `keep` its item moves into the
    /// edited scene with its layout (answers the new item), else it is removed and the
    /// source too when that was its last placement (answers `None`). One undo step.
    fn unmake_global(&mut self, id: SourceId, keep: bool) -> Result<Option<ItemId>, Refusal> {
        let name = self.name_of(id);
        let Some(item) = self.global_item(id) else {
            return Err(if self.collection.source(id).is_none() {
                Refusal::NotFound(format!("There is no source {}.", id.get()))
            } else {
                Refusal::Invalid(format!("{name} is not a global source."))
            });
        };
        if !keep {
            self.remove_from_scene(item)?;
            return Ok(None);
        }
        let layout = self
            .collection
            .item(item)
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| self.default_layout(None));
        let mut records = Vec::new();
        let done = (|| {
            let undo = self
                .collection
                .remove_item(item)
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            let (new, undo) = self
                .collection
                .add_item(
                    self.edited(),
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(new)
        })();
        match done {
            Ok(new) => {
                self.history.record(EditKind::Global, &name, 0, records);
                Ok(Some(new))
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// Adds a new source as a global source (Settings › Global sources, `source.add`
    /// with `global`); one undo step.
    fn add_global(&mut self, kind: SourceKind, target: &Target) -> Result<SourceId, Refusal> {
        let (settings, layout) = self.new_source(kind, target).map_err(Refusal::Failed)?;
        let name = unique_name(&self.collection, &target.name);
        let mut records = Vec::new();
        let done = (|| {
            let g = self.ensure_globals(&mut records)?;
            let (id, undo) = self
                .collection
                .add_source(&name, kind.type_id(), settings)
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            let (_, undo) = self
                .collection
                .add_item(
                    g,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(id)
        })();
        match done {
            Ok(id) => {
                self.history.record(EditKind::Add, &name, 0, records);
                Ok(id)
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// The canvas's global sources, top first (ADR-0065).
    fn global_rows(&self) -> Vec<SourceRow> {
        match self.globals() {
            Some(g) => self.rows_of(g),
            None => Vec::new(),
        }
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
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(id),
            self.stored_layout(&layout),
            None,
        ) {
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

    /// Adds the "Capture audio too" companion of a picture source (ADR-0096): a Display
    /// gets the system sound without RBS's own sound, a Window its application's
    /// sound. The companion joins the picture's last history step when `join` is set
    /// (adding both is one undo step), else it is a step of its own. It goes in the
    /// scene of the picture's first item, named after the picture.
    fn add_companion(&mut self, picture: SourceId, join: bool) -> Result<SourceId, Refusal> {
        let source = self
            .collection
            .source(picture)
            .ok_or_else(|| Refusal::NotFound("This source is not in the scene any more.".into()))?
            .clone();
        if self
            .collection
            .sources()
            .any(|s| s.companion_of() == Some(picture))
        {
            return Err(Refusal::Conflict(format!(
                "{} already has its sound.",
                source.name
            )));
        }
        let scene = self
            .collection
            .placements_of(picture)
            .first()
            .map(|(scene, _)| *scene)
            .ok_or_else(|| Refusal::Invalid("The source is not placed in any scene.".into()))?;
        let (kind, target, name, exclude) = match SourceKind::of(&source.type_id) {
            Some(SourceKind::Screen) => {
                if !self.adapters.sources.can_exclude_own_sound() {
                    return Err(Refusal::Failed(
                        "This system cannot leave RBS's own sound out of the system sound.".into(),
                    ));
                }
                let target = self
                    .targets(SourceKind::SystemAudio)
                    .into_iter()
                    .find(|t| t.is_default)
                    .ok_or_else(|| {
                        Refusal::Failed("There is no sound output to capture.".into())
                    })?;
                (
                    SourceKind::SystemAudio,
                    target,
                    "System sound".to_owned(),
                    true,
                )
            }
            Some(SourceKind::Window) => {
                let app = source.hints.get(HINT_APP).cloned().ok_or_else(|| {
                    Refusal::Failed("This window's application is not known.".into())
                })?;
                let target = self
                    .targets(SourceKind::ApplicationAudio)
                    .into_iter()
                    .find(|t| t.name == app)
                    .ok_or_else(|| {
                        Refusal::Failed(format!("{app} is not playing sound that can be captured."))
                    })?;
                (
                    SourceKind::ApplicationAudio,
                    target,
                    format!("{} sound", source.name),
                    false,
                )
            }
            Some(SourceKind::Camera) => {
                let target = self.camera_microphone(&source).ok_or_else(|| {
                    Refusal::Failed("No microphone belongs with this camera.".into())
                })?;
                (
                    SourceKind::AudioInput,
                    target,
                    format!("{} mic", source.name),
                    false,
                )
            }
            _ => {
                return Err(Refusal::Invalid(
                    "Only a Display, a Window or a Camera can capture audio too.".into(),
                ));
            }
        };
        let (mut settings, layout) = self.new_source(kind, &target).map_err(Refusal::Failed)?;
        if exclude {
            settings.insert(keys::EXCLUDE_OWN_PROCESS.into(), SettingValue::Bool(true));
        }
        let name = unique_name(&self.collection, &name);
        let (id, undo_source) = self
            .collection
            .add_source(&name, kind.type_id(), settings.clone())
            .map_err(|e| Refusal::of(&e))?;
        let mut hints = hints_of(&target);
        hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
        let hinted = self
            .collection
            .set_source_settings_and_hints(id, settings, hints);
        let placed = hinted.and_then(|undo_hints| {
            self.collection
                .add_item(
                    scene,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map(|(_, undo_item)| (undo_hints, undo_item))
        });
        match placed {
            Ok((undo_hints, undo_item)) => {
                let records = vec![undo_source, undo_hints, undo_item];
                if join {
                    self.history.extend_last(EditKind::Add, &name, records);
                } else {
                    self.history.record(EditKind::Add, &name, 0, records);
                }
                Ok(id)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Removes the companion of `picture` as one undo step.
    fn remove_companion(&mut self, picture: SourceId) -> Result<(), Refusal> {
        let id = self
            .collection
            .sources()
            .find(|s| s.companion_of() == Some(picture))
            .map(|s| s.id)
            .ok_or_else(|| Refusal::NotFound("This source has no companion sound.".into()))?;
        self.delete(id).map(|_| ()).map_err(Refusal::Failed)
    }

    /// Adds a sample covering the canvas: a background at the bottom of the scene
    /// being edited, a frame on top (ADR-0074). `frame-thin` and `frame-thick` are a
    /// transparent colour with a border, so the thickness stays editable.
    fn add_sample(&mut self, id: &str) -> Result<SourceId, Refusal> {
        let sample = rbs_sources::still::SAMPLES
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no sample picture {id:?}.")))?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let mut layout = Layout::new(
            [0.0, 0.0],
            [canvas.size.width as f32, canvas.size.height as f32],
        );
        let name = unique_name(&self.collection, sample.name);
        let (type_id, settings) =
            if let Some((width, radius)) = rbs_sources::still::plain_frame(sample.id) {
                layout = layout.with_style(
                    ItemStyle::new(Shape::Rectangle { radius })
                        .with_border(Some(Border::new(width, [255, 255, 255, 255]))),
                );
                let mut settings = Settings::new();
                settings.insert(
                    keys::COLOR.into(),
                    SettingValue::Text("#00000000".to_owned()),
                );
                // The border is drawn in the item's box, so the picture itself stays tiny.
                settings.insert(keys::WIDTH.into(), SettingValue::Int(2));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(2));
                (SourceKind::Color.type_id(), settings)
            } else {
                let mut settings = Settings::new();
                settings.insert(
                    keys::SAMPLE.into(),
                    SettingValue::Text(sample.id.to_owned()),
                );
                (SourceKind::Image.type_id(), settings)
            };
        let (source, undo_source) = self
            .collection
            .add_source(&name, type_id, settings)
            .map_err(|e| Refusal::of(&e))?;
        let index = (sample.kind == rbs_sources::still::SampleKind::Background).then_some(0);
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(&layout),
            index,
        ) {
            Ok((_, undo_item)) => {
                self.history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Adds a colour shape, centred on the canvas being edited. The picture is a tiny
    /// texture; the shape is the item's outline, so the colour fills the box.
    fn add_stamp(&mut self, kind: u8) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let short = (canvas.size.width.min(canvas.size.height) as f32 * 0.28).clamp(48.0, 480.0);
        let (base, w, h, shape) = match kind {
            0 => ("Circle", short, short, Shape::Circle),
            1 => ("Square", short, short, Shape::Rectangle { radius: 0.0 }),
            2 => (
                "Rectangle",
                short * 1.6,
                short,
                Shape::Rectangle { radius: 0.0 },
            ),
            3 => (
                "Rounded rectangle",
                short * 1.6,
                short,
                Shape::Rectangle { radius: 32.0 },
            ),
            4 => ("Ellipse", short * 1.6, short, Shape::Ellipse),
            5 => ("Triangle", short, short, Shape::Triangle),
            6 => ("Arrow", short * 1.8, short * 0.9, Shape::Arrow),
            _ => return Err(Refusal::Invalid("There is no shape like that.".into())),
        };
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        )
        .with_style(ItemStyle::new(shape));
        let mut settings = Settings::new();
        settings.insert(keys::COLOR.into(), SettingValue::Text("#2f6fed".to_owned()));
        // The shape is the item's box. The texture only carries the fill.
        settings.insert(keys::WIDTH.into(), SettingValue::Int(2));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(2));
        self.add_still(base, SourceKind::Color, settings, layout, None)
    }

    /// Adds a PNG file as an image source, fitted inside half the canvas and centred.
    fn add_image_file(&mut self, path: &std::path::Path) -> Result<SourceId, Refusal> {
        let (pw, ph, _) = rbs_sources::still::decode_png(path).map_err(Refusal::Invalid)?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let max_w = canvas.size.width as f32 * 0.5;
        let max_h = canvas.size.height as f32 * 0.5;
        let scale = (max_w / pw as f32).min(max_h / ph as f32);
        let w = (pw as f32 * scale).max(1.0);
        let h = (ph as f32 * scale).max(1.0);
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        );
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.into(),
            SettingValue::Text(path.display().to_string()),
        );
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Image");
        self.add_still(base, SourceKind::Image, settings, layout, None)
    }

    /// Adds a local media file as a source, centred at half the canvas.
    fn add_browser_page(&mut self, url: &str) -> Result<SourceId, Refusal> {
        if self.browser_tree.is_none() {
            return Err(Refusal::Invalid(
                "Browser sources need rbs-browser-helper next to the app.".into(),
            ));
        }
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".into()))?;
        let (w, h) = {
            let width = i64::from(canvas.size.width).min(1920) & !1;
            let height = i64::from(canvas.size.height).min(1080) & !1;
            (width.max(2), height.max(2))
        };
        let mut settings = Settings::new();
        settings.insert(keys::URL.into(), SettingValue::Text(url.trim().to_owned()));
        settings.insert(keys::WIDTH.into(), SettingValue::Int(w));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(h));
        rbs_sources::validate_settings(&SourceKind::Browser.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let layout = Layout::fit(canvas.size.width, canvas.size.height, w as u32, h as u32);
        self.add_still("Page", SourceKind::Browser, settings, layout, None)
    }

    fn add_media_file(&mut self, path: &std::path::Path) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let w = canvas.size.width as f32 * 0.5;
        let h = canvas.size.height as f32 * 0.5;
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        );
        let path_text = path.display().to_string();
        let mut settings = Settings::new();
        settings.insert(keys::PATH.into(), SettingValue::Text(path_text));
        rbs_sources::validate_settings(&SourceKind::Media.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Media");
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, SourceKind::Media.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(&layout),
            None,
        ) {
            Ok((_, undo_item)) => {
                self.history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// The shared end of adding a still source: one source, one item, one undo step.
    fn add_still(
        &mut self,
        base: &str,
        kind: SourceKind,
        settings: Settings,
        layout: Layout,
        index: Option<usize>,
    ) -> Result<SourceId, Refusal> {
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, kind.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(&layout),
            index,
        ) {
            Ok((_, undo_item)) => {
                self.history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Sets a colour source's fill. One undo step; the picture is built again.
    fn set_source_fill(&mut self, id: SourceId, color: &str) -> Result<bool, Refusal> {
        let mut patch = std::collections::BTreeMap::new();
        patch.insert(
            keys::COLOR.to_owned(),
            serde_json::Value::String(color.to_owned()),
        );
        self.patch_source_settings(id, &patch)
    }

    /// Turns a colour source's fill on or off. Off clears the inside and, when the
    /// shape has no border, adds one in the fill colour so the shape stays visible.
    /// Both changes are one undo step. The stored colour is kept.
    fn set_source_filled(&mut self, id: SourceId, on: bool) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Color) {
            return Err(Refusal::Invalid("Fill is for a colour source.".into()));
        }
        let was_on = !matches!(
            source.settings.get(keys::FILLED),
            Some(SettingValue::Bool(false))
        );
        if was_on == on {
            return Ok(false);
        }
        let name = source.name.clone();
        let type_id = source.type_id.clone();
        let stroke = {
            let text = match source.settings.get(keys::COLOR) {
                Some(SettingValue::Text(t)) => t.as_str(),
                _ => "",
            };
            let mut color = rbs_sources::still::parse_color(text).unwrap_or([255, 255, 255, 255]);
            color[3] = 255;
            color
        };
        let mut settings = source.settings.clone();
        if on {
            settings.remove(keys::FILLED);
        } else {
            settings.insert(keys::FILLED.to_owned(), SettingValue::Bool(false));
        }
        rbs_sources::validate_settings(&type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let placed = if on {
            None
        } else {
            self.item_of(id).ok().map(|(_, item)| {
                let layout = self.shown_layout(item);
                let own = item
                    .overrides
                    .contains_key(&self.canvas)
                    .then_some(self.canvas);
                (item.id, layout, own)
            })
        };
        let undo_settings = self
            .collection
            .set_source_settings(id, settings)
            .map_err(|e| Refusal::of(&e))?;
        let mut records = vec![undo_settings];
        let mut kind = EditKind::SourceSettings;
        if let Some((item, mut layout, own)) = placed
            && layout.style.border.is_none()
        {
            layout.style = layout.style.with_border(Some(Border::new(4.0, stroke)));
            if !layout.style.is_valid() {
                let _ = self.collection.undo(records.remove(0));
                return Err(Refusal::Invalid(
                    "The outline could not be turned on.".into(),
                ));
            }
            match self.save_live_layout(item, own, layout) {
                Ok(undo) => {
                    records.push(undo);
                    kind = EditKind::Style;
                }
                Err(e) => {
                    let _ = self.collection.undo(records.remove(0));
                    return Err(Refusal::of(&e));
                }
            }
        }
        self.history.record(kind, &name, 0, records);
        Ok(true)
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
        let program = self.scene;
        let preview = self.preview();
        let result = self.undo_or_redo_records(back);
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

    /// The program scene's item that places `id`, and its index (0 is the bottom).
    fn item_of(&self, id: SourceId) -> Result<(usize, &SceneItem), String> {
        let gone = || "This source is not in the scene any more.".to_owned();
        let find = |scene: SceneId| {
            self.collection.scene(scene).and_then(|s| {
                s.items
                    .iter()
                    .enumerate()
                    .find(|(_, i)| i.content == ItemContent::Source(id))
            })
        };
        // A global source's item is in the canvas's global scene (ADR-0065).
        find(self.edited())
            .or_else(|| self.globals().and_then(find))
            .ok_or_else(gone)
    }

    /// The scene that holds an item.
    fn scene_of(&self, item: ItemId) -> Option<SceneId> {
        self.collection.item(item).map(|(s, _)| s)
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
    fn move_source_to(&mut self, id: SourceId, to: SourceId) -> Result<bool, String> {
        if id == to {
            return Ok(false);
        }
        let (_, item) = self.item_of(id)?;
        let item = item.id;
        let (index, other) = self.item_of(to)?;
        // Layers move within one scene: global sources among themselves.
        if self.scene_of(item) != self.scene_of(other.id) {
            return Ok(false);
        }
        let undo = self
            .collection
            .move_item(item, index)
            .map_err(|e| edit_error(&e))?;
        let name = self.name_of(id);
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// One place or to an end in the canvas's scene list. `Ok(false)` at that end.
    fn move_scene(
        &mut self,
        id: SceneId,
        direction: rbs_api::request::LayerDirection,
    ) -> Result<bool, String> {
        use rbs_api::request::LayerDirection::*;
        let undo = match direction {
            Up => self.collection.move_scene(self.canvas, id, true),
            Down => self.collection.move_scene(self.canvas, id, false),
            Front => self.collection.move_scene_to_end(self.canvas, id, true),
            Back => self.collection.move_scene_to_end(self.canvas, id, false),
        }
        .map_err(|e| edit_error(&e))?;
        let Some(undo) = undo else {
            return Ok(false);
        };
        let name = self
            .collection
            .scene(id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Puts scene `id` where scene `to` is in the scene list.
    fn move_scene_to(&mut self, id: SceneId, to: SceneId) -> Result<bool, String> {
        let undo = self
            .collection
            .move_scene_to(self.canvas, id, to)
            .map_err(|e| edit_error(&e))?;
        let Some(undo) = undo else {
            return Ok(false);
        };
        let name = self
            .collection
            .scene(id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Releases the device (`active` false) or keeps it open. `Ok(false)` when it
    /// already is. Hiding a source does not call this.
    fn set_source_active(&mut self, id: SourceId, active: bool) -> Result<bool, String> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| format!("There is no source {}.", id.get()))?;
        if source.standby() == !active {
            return Ok(false);
        }
        let mut hints = source.hints.clone();
        if active {
            hints.remove(STANDBY_HINT);
        } else {
            hints.insert(STANDBY_HINT.to_owned(), "1".to_owned());
        }
        let settings = source.settings.clone();
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_settings_and_hints(id, settings, hints)
            .map_err(|e| edit_error(&e))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        Ok(true)
    }

    fn move_source(
        &mut self,
        id: SourceId,
        direction: rbs_api::request::LayerDirection,
    ) -> Result<bool, String> {
        use rbs_api::request::LayerDirection::*;
        let (index, item) = self.item_of(id)?;
        let item = item.id;
        let Some(scene) = self.scene_of(item).and_then(|s| self.collection.scene(s)) else {
            return Ok(false);
        };
        let pictures: Vec<usize> = scene
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| self.has_picture(it))
            .map(|(i, _)| i)
            .collect();
        if !pictures.contains(&index) {
            return Ok(false);
        }
        let to = match direction {
            Up => pictures.iter().copied().find(|i| *i > index),
            Down => pictures.iter().copied().rfind(|i| *i < index),
            Front => pictures.last().copied().filter(|i| *i != index),
            Back => pictures.first().copied().filter(|i| *i != index),
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

    /// Show or hide a source's item in the program scene; `Ok(false)` when it already is.
    fn set_source_visible(&mut self, id: SourceId, visible: bool) -> Result<bool, String> {
        let (_, item) = self.item_of(id)?;
        let item = item.id;
        self.set_item_visible(item, visible)
            .map_err(|e| e.text().to_owned())
    }

    /// Locks or unlocks an item (ADR-0059); `Ok(false)` when it already is.
    fn set_item_locked(&mut self, item: ItemId, locked: bool) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.locked == locked {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_locked(item, locked)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Lock, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Shows or hides an item; `Ok(false)` when it already is.
    fn set_item_visible(&mut self, item: ItemId, visible: bool) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.visible == visible {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_visible(item, visible)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Visibility, &name, 0, vec![undo]);
        // Opening the eye drops a running cue so the deadline cannot hide it later
        // (ADR-0105). Hiding does not start one.
        if visible {
            self.studio.drop_cue(item);
        }
        Ok(true)
    }

    /// Sets or clears `show_for_ms` (ADR-0105). One undo step. `Ok(false)` when the
    /// item already has that duration.
    fn set_item_show_for(
        &mut self,
        item: ItemId,
        show_for_ms: Option<u32>,
    ) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.show_for_ms == show_for_ms {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_show_for(item, show_for_ms)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::ShowFor, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Shows a hidden item until `now + show_for_ms` (ADR-0105). Live: no undo, and
    /// `visible` is not written. `Ok(false)` when the item is already visible or has
    /// no duration. Unknown items are `Err`.
    fn show_item(&mut self, item: ItemId) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        Ok(self.studio.show_cue(item, found.visible, found.show_for_ms))
    }

    /// The name of what an item places.
    fn item_name(&self, item: &SceneItem) -> String {
        match item.content {
            ItemContent::Source(id) => self.name_of(id),
            ItemContent::Scene(id) => self
                .collection
                .scene(id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// A source's volume or mute (ADR-0059): a live control, not on the undo stack,
    /// sent to the mixer without rebinding. `Ok(false)` when nothing changed.
    fn set_source_audio(
        &mut self,
        id: SourceId,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<bool, Refusal> {
        let now = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .audio;
        let mut changed = false;
        if let Some(v) = volume.filter(|v| *v != now.volume) {
            self.collection
                .set_source_volume(id, v)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if let Some(m) = muted.filter(|m| *m != now.muted) {
            self.collection
                .set_source_muted(id, m)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if changed {
            self.studio.set_mix(&self.collection);
        }
        Ok(changed)
    }

    /// The canvas's master volume or mute (ADR-0081): a live control, not on the undo
    /// stack, sent to the mixer without rebinding. `Ok(false)` when nothing changed.
    fn set_master(&mut self, volume: Option<f32>, muted: Option<bool>) -> Result<bool, Refusal> {
        let canvas = self.canvas;
        let now = self
            .collection
            .canvas(canvas)
            .ok_or_else(|| Refusal::NotFound("There is no canvas.".to_owned()))?
            .master;
        let mut changed = false;
        if let Some(v) = volume.filter(|v| *v != now.volume) {
            self.collection
                .set_canvas_master_volume(canvas, v)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if let Some(m) = muted.filter(|m| *m != now.muted) {
            self.collection
                .set_canvas_master_muted(canvas, m)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if changed {
            self.studio.set_mix(&self.collection);
        }
        Ok(changed)
    }

    /// Makes the second canvas and runs it (ADR-0093 decision 8). Everything is tried
    /// on a copy of the collection; the collection and the studio stay as they were when
    /// any step is refused.
    fn add_vertical(&mut self, spec: VerticalCanvas) -> Result<bool, Refusal> {
        if self.vertical.is_some() {
            return Err(Refusal::Conflict(
                "There is a vertical canvas already.".to_owned(),
            ));
        }
        let main = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("There is no canvas.".to_owned()))?;
        let fps = main.frame_rate;
        let mut draft = self.collection.clone();
        let name = ["Vertical", "Vertical output", "Vertical canvas"]
            .into_iter()
            .chain((2..).map(|_| "Vertical 9:16"))
            .find(|n| !draft.canvases().any(|c| c.name == *n))
            .unwrap_or("Vertical");
        let (canvas, _) = draft
            .add_canvas(name, spec.size, fps)
            .map_err(|e| Refusal::of(&e))?;
        // Following needs scenes the new canvas may show (ADR-0093 decision 3): the main
        // canvas's own scenes become shared (U15); the global scene stays where it is.
        let mut shared = Vec::new();
        if spec.follow {
            let own: Vec<_> = draft
                .scenes_for(self.canvas)
                .filter(|s| s.scope == SceneScope::Canvas(self.canvas))
                .filter(|s| !draft.is_global_scene(s.id))
                .map(|s| s.id)
                .collect();
            for id in &own {
                draft
                    .set_scene_scope(*id, SceneScope::Shared)
                    .map_err(|e| Refusal::of(&e))?;
            }
            shared = own;
        }
        // A scene of its own to start from when it does not follow, else the one on air.
        let scene = if draft.scenes_for(canvas).any(|s| s.id == self.scene) {
            self.scene
        } else {
            draft
                .showable_scenes(canvas)
                .into_iter()
                .next()
                .map(|s| s.id)
                .ok_or_else(|| {
                    Refusal::Conflict(
                        "No scene can be shown on a vertical canvas: the scenes belong to the main canvas.".to_owned(),
                    )
                })?
        };
        draft
            .set_program(canvas, Some(scene))
            .map_err(|e| Refusal::of(&e))?;
        if spec.follow {
            draft
                .set_canvas_follow(canvas, Some(self.canvas), Default::default())
                .map_err(|e| Refusal::of(&e))?;
        }
        if let Some(mode) = spec.layout {
            let sizes = &self.sizes;
            draft
                .auto_layout(
                    canvas,
                    self.scene,
                    mode,
                    &|source| sizes.get(&source).copied(),
                    &BTreeMap::new(),
                )
                .map_err(|e| Refusal::of(&e))?;
        }
        let before = std::mem::replace(&mut self.collection, draft);
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        match self.studio.activate_canvas(
            running,
            &self.adapters.sources,
            self.vertical_extra(canvas),
        ) {
            Ok(_) => {
                self.vertical_shared = shared;
                self.vertical = Some(VerticalState {
                    canvas,
                    size: spec.size,
                    follow: spec.follow,
                });
                self.waiting = self.studio.waiting();
                Ok(true)
            }
            Err(e) => {
                self.collection = before;
                Err(Refusal::Failed(format!(
                    "The vertical canvas did not start: {e}"
                )))
            }
        }
    }

    /// The vertical canvas as the studio runs it: with its picture while its panel
    /// shows (ADR-0093 decision 7).
    /// Runs the vertical canvas the collection saved, when none runs yet: the canvas
    /// stays in the document across a restart of RBS or a collection switch, and
    /// without this its outputs, panel picture and Remove would be gone while a new
    /// Add made a second one (ADR-0093 decision 9). A canvas that cannot start is told
    /// once and stays in the document.
    fn restore_vertical(&mut self) {
        if self.vertical.is_some() {
            return;
        }
        let Some(state) = saved_vertical(&self.collection, self.canvas) else {
            return;
        };
        let extra = self.vertical_extra(state.canvas);
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        match self
            .studio
            .activate_canvas(running, &self.adapters.sources, extra)
        {
            Ok(_) => {
                self.vertical = Some(state);
                self.waiting = self.studio.waiting();
            }
            Err(e) => {
                self.vertical_restore_error =
                    Some(format!("The vertical canvas did not start: {e}"));
            }
        }
    }

    fn vertical_extra(&self, canvas: CanvasId) -> ExtraCanvas {
        let extra = ExtraCanvas::new(canvas);
        let picture = self.collection.canvas(canvas);
        match (self.vertical_picture, self.preview_surfaces, picture) {
            (true, Some(_), Some(info)) => {
                extra.with_preview(preview_desc(info.size, info.frame_rate))
            }
            _ => extra,
        }
    }

    /// The vertical canvas's panel opened or closed: its picture starts or stops, also
    /// while its outputs run.
    fn set_vertical_picture(&mut self, on: bool) -> Result<(), String> {
        self.vertical_picture = on;
        let Some(v) = self.vertical else {
            return Ok(());
        };
        let desc = self.vertical_extra(v.canvas).preview;
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        self.studio
            .set_canvas_picture(running, &self.adapters.sources, v.canvas, desc)
            .map(|_| ())
            .map_err(|e| format!("The vertical preview did not start: {e}"))?;
        self.waiting = self.studio.waiting();
        Ok(())
    }

    /// Stops the second canvas and removes it from the collection.
    fn remove_vertical(&mut self) -> Result<bool, Refusal> {
        let Some(state) = self.vertical else {
            return Ok(false);
        };
        if self.outs[slot(OutputCanvas::Vertical)].busy() {
            return Err(Refusal::Conflict(
                "Stop the recording or stream of the vertical canvas first.".to_owned(),
            ));
        }
        let mut draft = self.collection.clone();
        draft
            .remove_canvas(state.canvas)
            .map_err(|e| Refusal::of(&e))?;
        // The canvas leaves the engine while it is still in the collection.
        let run = run_copy_or(&self.collection, self.canvas, self.held.as_ref());
        self.studio
            .deactivate_canvas(
                run.as_ref().unwrap_or(&self.collection),
                &self.adapters.sources,
                state.canvas,
            )
            .map_err(|e| match e {
                RecordError::CanvasBusy(_) => Refusal::Conflict(
                    "Stop the recording or stream of the vertical canvas first.".to_owned(),
                ),
                e => Refusal::Failed(e.to_string()),
            })?;
        // The scenes the add shared go back to the main canvas (the removal is one step).
        for id in std::mem::take(&mut self.vertical_shared) {
            if draft
                .scenes_for(self.canvas)
                .any(|s| s.id == id && s.scope == SceneScope::Shared)
            {
                let _ = draft.set_scene_scope(id, SceneScope::Canvas(self.canvas));
            }
        }
        self.collection = draft;
        self.vertical = None;
        // Steps that wrote vertical layouts would name a canvas that is gone (scene
        // model invariant 4), so the history ends here, as after a canvas change.
        self.history.clear();
        self.forget_vertical_outputs();
        self.waiting = self.studio.waiting();
        Ok(true)
    }

    /// Hides a source from the Mixer or shows it (ADR-0080): a view flag, not on the
    /// undo stack, and the mix is untouched. `Ok(false)` when nothing changed.
    fn set_source_mixer_hidden(&mut self, id: SourceId, hidden: bool) -> Result<bool, Refusal> {
        let now = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .audio;
        if now.mixer_hidden == hidden {
            return Ok(false);
        }
        self.collection
            .set_source_mixer_hidden(id, hidden)
            .map_err(|e| Refusal::of(&e))?;
        Ok(true)
    }

    /// Renames a source (one undo step); `Ok(false)` when it has that name.
    /// Points a source at `target`: its `adapter` and `target` settings change, nothing
    /// else, so name, layout, filters and every scene's item stay. The binder replaces
    /// the node (break before make). One undo step; the same target records nothing.
    fn retarget_source(&mut self, id: SourceId, target: &Target) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        let mut settings = source.settings.clone();
        let same = |key: &str, value: &str| matches!(settings.get(key), Some(SettingValue::Text(t)) if t == value);
        if same(keys::ADAPTER, &target.adapter) && same(keys::TARGET, &target.target) {
            return Ok(false);
        }
        settings.insert(
            keys::ADAPTER.into(),
            SettingValue::Text(target.adapter.clone()),
        );
        settings.insert(
            keys::TARGET.into(),
            SettingValue::Text(target.target.clone()),
        );
        // A target made from an API call has no name: the listed one gives the hints.
        let hints = match (target.name.is_empty(), kind) {
            (true, Some(kind)) => self
                .targets(kind)
                .iter()
                .find(|t| t.adapter == target.adapter && t.target == target.target)
                .map(hints_of)
                .unwrap_or_default(),
            _ => hints_of(target),
        };
        let undo = self
            .collection
            .set_source_settings_and_hints(id, settings, hints)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Retarget, &name, 0, vec![undo]);
        if let Some(kind) = kind {
            self.remember_target(kind, &target.adapter, &target.target, &target.name);
        }
        Ok(true)
    }

    /// What the Source settings window shows for a source.
    fn source_settings_view(&self, id: SourceId) -> Result<SourceSettingsView, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let kind = SourceKind::of(&source.type_id)
            .ok_or_else(|| Refusal::Invalid("This source has no settings window.".to_owned()))?;
        let int = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Int(n)) => u32::try_from(*n).ok(),
            _ => None,
        };
        let text = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let modes = match (kind, text(keys::TARGET)) {
            (SourceKind::Camera, Some(target)) => self
                .adapters
                .sources
                .targets(&kind.type_id())
                .unwrap_or_default()
                .into_iter()
                .find(|t| t.target == target)
                .map(|t| t.modes)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let (cap_has, cap_l, cap_t, cap_r, cap_b) = match source.region {
            Some(r) => (true, r.left, r.top, r.right, r.bottom),
            None => (false, 0, 0, 0, 0),
        };
        let (chrome_on, chrome_t, chrome_r, chrome_b, chrome_l, chrome_fill) = match source.chrome {
            Some(c) => (
                true,
                c.top,
                c.right,
                c.bottom,
                c.left,
                match c.fill {
                    ChromeFill::Cover => 0,
                    ChromeFill::Fit => 1,
                    _ => 0,
                },
            ),
            None => (false, 0, 0, 0, 0, 0),
        };
        let has_work_rect = kind == SourceKind::Screen && self.display_work(id).is_some();
        Ok(SourceSettingsView {
            id,
            name: source.name.clone(),
            kind,
            modes,
            size: int(keys::WIDTH).zip(int(keys::HEIGHT)),
            fps: int(keys::FPS),
            cursor: !matches!(
                source.settings.get(keys::CURSOR),
                Some(SettingValue::Bool(false))
            ),
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
            pic_w: self.sizes.get(&id).map(|(w, _)| *w).unwrap_or(0),
            pic_h: self.sizes.get(&id).map(|(_, h)| *h).unwrap_or(0),
        })
    }

    /// Capture, chrome and timed-show fields for a sources list row.
    fn source_row_extra(&self, source: &Source, item: ItemId) -> RowExtra {
        let (cap_has, cap_l, cap_t, cap_r, cap_b) = match source.region {
            Some(r) => (true, r.left, r.top, r.right, r.bottom),
            None => (false, 0, 0, 0, 0),
        };
        let (chrome_on, chrome_t, chrome_r, chrome_b, chrome_l, chrome_fill) = match source.chrome {
            Some(c) => (
                true,
                c.top,
                c.right,
                c.bottom,
                c.left,
                match c.fill {
                    ChromeFill::Cover => 0,
                    ChromeFill::Fit => 1,
                    _ => 0,
                },
            ),
            None => (false, 0, 0, 0, 0, 0),
        };
        let has_work_rect = source.type_id.as_str() == rbs_scene::DISPLAY_SOURCE_TYPE
            && self.display_work(source.id).is_some();
        let show_for_ms = self
            .collection
            .item(item)
            .and_then(|(_, i)| i.show_for_ms)
            .unwrap_or(0);
        let cue_remaining_ms = self
            .studio
            .cue_remaining_ms(item, self.clock.now())
            .unwrap_or(0);
        (
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
        )
    }

    /// A source's stored settings as JSON values (`source.get_settings`).
    fn source_settings_json(
        &self,
        id: SourceId,
    ) -> Result<BTreeMap<String, serde_json::Value>, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        Ok(source
            .settings
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    SettingValue::Bool(b) => serde_json::Value::Bool(*b),
                    SettingValue::Int(n) => serde_json::Value::from(*n),
                    SettingValue::Float(f) => serde_json::Value::from(*f),
                    SettingValue::Text(t) => serde_json::Value::String(t.clone()),
                    _ => serde_json::Value::Null,
                };
                (k.clone(), v)
            })
            .collect())
    }

    /// Changes a source's stored settings by a patch (`source.set_settings`, ADR-0077): a
    /// `null` removes a key; the result is validated for the source's type, nothing is
    /// written when it is refused, and a change is one undo step. The source opens again
    /// with the new settings (filters, layout and scenes stay). `Ok(false)` when nothing
    /// changes.
    fn patch_source_settings(
        &mut self,
        id: SourceId,
        patch: &BTreeMap<String, serde_json::Value>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let (name, type_id) = (source.name.clone(), source.type_id.clone());
        let mut settings = source.settings.clone();
        for (key, value) in patch {
            match value {
                serde_json::Value::Null => {
                    settings.remove(key);
                }
                serde_json::Value::Bool(b) => {
                    settings.insert(key.clone(), SettingValue::Bool(*b));
                }
                serde_json::Value::Number(n) => {
                    let v = match n.as_i64() {
                        Some(i) => SettingValue::Int(i),
                        None => SettingValue::Float(n.as_f64().unwrap_or(f64::NAN)),
                    };
                    if matches!(v, SettingValue::Float(f) if !f.is_finite()) {
                        return Err(Refusal::Invalid(format!(
                            "{key:?} must be a finite number."
                        )));
                    }
                    settings.insert(key.clone(), v);
                }
                serde_json::Value::String(t) => {
                    settings.insert(key.clone(), SettingValue::Text(t.clone()));
                }
                _ => {
                    return Err(Refusal::Invalid(format!(
                        "{key:?} must be a flag, a number, text or null."
                    )));
                }
            }
        }
        rbs_sources::validate_settings(&type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        if self
            .collection
            .source(id)
            .is_some_and(|s| s.settings == settings)
        {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_source_settings(id, settings)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Tries a source again with its stored settings. Only when a closed and reopened
    /// window is found by its hints is something edited: the source is re-pointed (one
    /// "retarget" undo step) and the caller applies the collection, which replaces the
    /// node; otherwise it is no edit and no undo step.
    fn reload_source(&mut self, id: SourceId) -> Result<Reload, Refusal> {
        if self.collection.source(id).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                id.get()
            )));
        }
        // A window that was closed and opened again has a new id: when the stored one
        // is gone, exactly one window of the same application with the same title is
        // taken instead (ADR-0077 decision 3b); otherwise nothing is guessed.
        if let Some(target) = self.window_by_hints(id)
            && self.retarget_source(id, &target)?
        {
            return Ok(Reload::Retargeted);
        }
        Ok(Reload::Restarted(self.studio.reload_source(id)))
    }

    /// The window a window source's hints name, when its stored target is no longer
    /// listed and exactly one listed window has the same application and title.
    fn window_by_hints(&self, id: SourceId) -> Option<Target> {
        let source = self.collection.source(id)?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Window) {
            return None;
        }
        let text = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Text(t)) => Some(t.as_str()),
            _ => None,
        };
        let (adapter, stored) = (text(keys::ADAPTER)?, text(keys::TARGET)?);
        let (app, title) = (source.hints.get(HINT_APP)?, source.hints.get(HINT_TITLE)?);
        match_by_hints(
            self.targets(SourceKind::Window),
            adapter,
            stored,
            app,
            title,
        )
    }

    fn rename_source(&mut self, id: SourceId, name: &str) -> Result<bool, Refusal> {
        let old = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .name
            .clone();
        if old == name.trim() {
            return Ok(false);
        }
        let undo = self
            .collection
            .rename_source(id, name)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::RenameSource, &old, 0, vec![undo]);
        Ok(true)
    }

    fn set_glide(&mut self, duration_ms: u32, easing: u8) -> Result<bool, Refusal> {
        let glide = rbs_scene::CollectionGlide {
            duration_ms,
            easing: match easing {
                0 => rbs_scene::GlideEasing::Linear,
                _ => rbs_scene::GlideEasing::EaseInOut,
            },
        };
        if self.collection.glide() == glide {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_glide(glide)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Glide, "", 0, vec![undo]);
        Ok(true)
    }

    fn set_source_glide(
        &mut self,
        id: SourceId,
        custom: bool,
        duration_ms: u32,
        easing: u8,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let glide = custom.then_some(rbs_scene::SourceGlide {
            duration_ms,
            easing: match easing {
                0 => rbs_scene::GlideEasing::Linear,
                _ => rbs_scene::GlideEasing::EaseInOut,
            },
        });
        if source.glide == glide {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_source_glide(id, glide)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Glide, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Sets or clears a source region (ADR-0103). One undo step, not an item crop.
    /// A sound-only source is refused. When the picture size is known, a region
    /// that leaves no pixel is refused. Setting a region also places the item box
    /// with Cover so a canvas-sized region fills the canvas (settings Apply).
    fn set_capture_region(
        &mut self,
        id: SourceId,
        region: Option<SourceRegion>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id).is_some_and(|kind| kind.is_audio()) {
            return Err(Refusal::Invalid("A sound source has no picture.".into()));
        }
        if let Some(region) = region
            && let Some(picture) = self.sizes.get(&id).copied()
            && region.size(picture).is_none()
        {
            return Err(Refusal::Invalid("The region leaves no picture.".into()));
        }
        if source.region == region {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_region(id, region)
            .map_err(|e| Refusal::of(&e))?;
        let mut steps = vec![undo];
        if region.is_some()
            && let Ok(Some(fit)) = self.fit_item_to_visible(id, FitMode::Fill)
        {
            steps.push(fit);
        }
        self.history.record(EditKind::Region, &name, 0, steps);
        Ok(true)
    }

    /// Places the edited scene's item for `id` using the visible picture size
    /// (region ∩ crop). `None` when size or canvas is unknown.
    fn fit_item_to_visible(
        &mut self,
        id: SourceId,
        mode: FitMode,
    ) -> Result<Option<Undo>, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let picture = self.sizes.get(&id).copied().ok_or_else(|| {
            Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
        })?;
        let visible = visible_size(picture, source.region, Crop::default())
            .ok_or_else(|| Refusal::Invalid("The region leaves no picture.".into()))?;
        let item = self.item_of(id).map_err(Refusal::NotFound)?.1.id;
        let (_, old, own) = self.crop_target(item).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .map(|c| (c.size.width, c.size.height))
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let layout = old
            .fitted(canvas, visible, mode)
            .ok_or_else(|| Refusal::Invalid("The region leaves no picture.".into()))?;
        if layout == old {
            return Ok(None);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        Ok(Some(undo))
    }

    /// Writes display chrome and the region those insets describe (ADR-0104).
    /// The item box stays. `None` clears chrome and the region.
    fn set_display_chrome(
        &mut self,
        id: SourceId,
        chrome: Option<DisplayChrome>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if source.type_id.as_str() != rbs_scene::DISPLAY_SOURCE_TYPE {
            return Err(Refusal::Invalid(
                "Only a display source can hide the taskbar or Dock.".into(),
            ));
        }
        if let Some(chrome) = chrome
            && let Some(picture) = self.sizes.get(&id).copied()
            && chrome.region().size(picture).is_none()
        {
            return Err(Refusal::Invalid("The region leaves no picture.".into()));
        }
        // Chrome equal but region cleared (e.g. Apply after chrome wrote insets) still
        // needs a write so the cut comes back (ADR-0104).
        let region_matches = match chrome {
            None => source.region.is_none(),
            Some(c) => source.region == Some(c.region()),
        };
        if source.chrome == chrome && region_matches {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_display_chrome(id, chrome)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Region, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Places the source's item with the stored chrome fill (ADR-0104). The region
    /// and the capture node stay.
    fn fit_display_chrome(&mut self, id: SourceId) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let chrome = source
            .chrome
            .ok_or_else(|| Refusal::Invalid("Chrome is off.".into()))?;
        let name = source.name.clone();
        let picture = self.sizes.get(&id).copied().ok_or_else(|| {
            Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
        })?;
        let item = self.item_of(id).map_err(Refusal::NotFound)?.1.id;
        let (_, old, own) = self.crop_target(item).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .map(|c| (c.size.width, c.size.height))
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let layout = place_chrome(chrome, canvas, picture, old)
            .ok_or_else(|| Refusal::Invalid("The region leaves no picture.".into()))?;
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Transform, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Copies chrome insets from the display's work area. A no-op, with no undo
    /// step, when the adapter did not report `work_rect` (ADR-0104).
    fn reset_display_chrome(&mut self, id: SourceId) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if source.type_id.as_str() != rbs_scene::DISPLAY_SOURCE_TYPE {
            return Err(Refusal::Invalid(
                "Only a display source can hide the taskbar or Dock.".into(),
            ));
        }
        let fill = source.chrome.map(|c| c.fill).unwrap_or(ChromeFill::Cover);
        let Some((work, picture)) = self.display_work(id) else {
            return Ok(false);
        };
        let Some(chrome) = DisplayChrome::from_work(
            picture,
            Some((work.x, work.y, work.width, work.height)),
            fill,
        ) else {
            return Ok(false);
        };
        if source.chrome == Some(chrome) {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_display_chrome(id, Some(chrome))
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Region, &name, 0, vec![undo]);
        Ok(true)
    }

    /// The listed display's work area and the picture size it belongs to.
    fn display_work(&self, id: SourceId) -> Option<(rbs_capture::WorkRect, (u32, u32))> {
        let source = self.collection.source(id)?;
        let target_id = match source.settings.get(keys::TARGET) {
            Some(SettingValue::Text(text)) => text.as_str(),
            _ => return None,
        };
        let adapter = match source.settings.get(keys::ADAPTER) {
            Some(SettingValue::Text(text)) => Some(text.as_str()),
            _ => None,
        };
        let found = self
            .adapters
            .sources
            .targets(&source.type_id)
            .ok()?
            .into_iter()
            .find(|target| {
                target.target == target_id && adapter.is_none_or(|id| target.adapter.as_str() == id)
            })?;
        let work = found.work_rect?;
        let picture = self.sizes.get(&id).copied().or(found.size)?;
        Some((work, picture))
    }

    /// Places a source the collection has on top of the program scene (one step).
    fn add_existing(&mut self, id: SourceId) -> Result<ItemId, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        if self.global_item(id).is_some() {
            return Err(Refusal::Conflict(format!(
                "{name} is a global source: it is already in every scene."
            )));
        }
        // The same box as the source has elsewhere, else the whole canvas.
        let layout = self
            .collection
            .placements_of(id)
            .first()
            .and_then(|(_, item)| self.collection.item(*item))
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| {
                let c = self.collection.canvas(self.canvas);
                let (w, h) = c.map_or((1.0, 1.0), |c| (c.size.width as f32, c.size.height as f32));
                if kind.is_some_and(SourceKind::is_audio) {
                    Layout::new([0.0, 0.0], [1.0, 1.0])
                } else {
                    Layout::new([0.0, 0.0], [w, h])
                }
            });
        let (item, undo) = self
            .collection
            .add_item(
                self.edited(),
                ItemContent::Source(id),
                self.stored_layout(&layout),
                None,
            )
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Add, &name, 0, vec![undo]);
        Ok(item)
    }

    /// The scenes the canvas may show, in panel order.
    fn panel_scenes(&self) -> Vec<SceneId> {
        self.collection
            .showable_scenes(self.canvas)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    /// Adds an empty shared scene (one undo step); it does not go on air.
    fn add_scene(&mut self, name: Option<&str>) -> Result<SceneId, Refusal> {
        let name = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_owned(),
            Some(_) => return Err(Refusal::Invalid("A scene needs a name.".to_owned())),
            None => unique_scene_name(&self.collection, "Scene"),
        };
        let (id, undo) = self
            .collection
            .add_scene(&name, SceneScope::Shared)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::AddScene, &name, 0, vec![undo]);
        Ok(id)
    }

    /// Copies a scene (one undo step); it does not go on air.
    fn duplicate_scene(&mut self, id: SceneId) -> Result<SceneId, Refusal> {
        let base = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        let name = unique_scene_name(&self.collection, &base);
        let (new_id, undo) = self
            .collection
            .duplicate_scene(id, &name)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::AddScene, &name, 0, vec![undo]);
        Ok(new_id)
    }

    /// Removes a scene (one undo step). The last scene the canvas may show stays; the
    /// scene on air is first switched away from, outside the undo stack (ADR-0059).
    fn remove_scene(&mut self, id: SceneId) -> Result<(), Refusal> {
        let name = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        let panel = self.panel_scenes();
        let Some(at) = panel.iter().position(|s| *s == id) else {
            return Err(Refusal::NotFound(format!(
                "The canvas cannot show the scene {name:?}."
            )));
        };
        if panel.len() < 2 {
            return Err(Refusal::Conflict(
                "The last scene cannot be removed.".to_owned(),
            ));
        }
        if self.scene == id {
            let next = if at > 0 { panel[at - 1] } else { panel[at + 1] };
            self.switch_scene(next)?;
        }
        // Studio mode keeps a preview scene (ADR-0061): the program takes its place.
        if self.preview() == Some(id) {
            self.collection
                .set_preview(self.canvas, Some(self.scene))
                .map_err(|e| Refusal::of(&e))?;
        }
        let undo = self
            .collection
            .remove_scene(id)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::RemoveScene, &name, 0, vec![undo]);
        Ok(())
    }

    /// Renames a scene (one undo step); `Ok(false)` when it has that name.
    fn rename_scene(&mut self, id: SceneId, name: &str) -> Result<bool, Refusal> {
        let old = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        if old == name.trim() {
            return Ok(false);
        }
        let undo = self
            .collection
            .rename_scene(id, name)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::RenameScene, &old, 0, vec![undo]);
        Ok(true)
    }

    /// Puts a scene on air: a live action, not on the undo stack (ADR-0021).
    /// `Ok(false)` when it is on air already.
    fn switch_scene(&mut self, id: SceneId) -> Result<bool, Refusal> {
        if !self.listed(id) {
            return Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                id.get()
            )));
        }
        if self.scene == id {
            return Ok(false);
        }
        self.collection
            .set_program(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        self.scene = id;
        Ok(true)
    }

    /// The preview scene while the canvas is in studio mode (ADR-0061).
    fn preview(&self) -> Option<SceneId> {
        self.collection.canvas(self.canvas).and_then(|c| c.preview)
    }

    /// The scene source and item edits act on: the preview scene in studio mode, else
    /// the program scene (ADR-0061 decision 4).
    fn edited(&self) -> SceneId {
        self.into.or(self.preview()).unwrap_or(self.scene)
    }

    /// Runs `f` with source and item additions going to `scene` (an API call's
    /// `scene`, ADR-0061 decision 8); `None` keeps them in the program scene, as in
    /// protocol 1.6.
    fn placing_into<T>(
        &mut self,
        scene: Option<u32>,
        f: impl FnOnce(&mut Self) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let scene = match scene {
            None => self.scene,
            Some(raw) => {
                let id = SceneId::from_raw(raw);
                if !self.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {raw}"),
                    ));
                }
                id
            }
        };
        let old = self.into.replace(scene);
        let done = f(self);
        self.into = old;
        done
    }

    /// Enters or leaves studio mode (ADR-0061): a live action. Entering puts the
    /// program scene in preview. `Ok(false)` when it is in that mode already.
    fn set_studio_mode(&mut self, on: bool) -> Result<bool, Refusal> {
        if self.preview().is_some() == on {
            return Ok(false);
        }
        let scene = on.then_some(self.scene);
        self.collection
            .set_preview(self.canvas, scene)
            .map_err(|e| Refusal::of(&e))?;
        Ok(true)
    }

    /// Makes a scene the preview scene (studio mode only; a live action).
    /// `Ok(false)` when it is the preview scene already.
    fn set_preview_scene(&mut self, id: SceneId) -> Result<bool, Refusal> {
        if !self.listed(id) {
            return Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                id.get()
            )));
        }
        let Some(preview) = self.preview() else {
            return Err(Refusal::Conflict(STUDIO_MODE_OFF.to_owned()));
        };
        if preview == id {
            return Ok(false);
        }
        self.collection
            .set_preview(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        Ok(true)
    }

    /// The transition (a cut, ADR-0061 decision 3): the preview scene goes on air and
    /// stays in preview, so the program is held from now on (ADR-0063); with the swap
    /// setting the old program scene goes to preview instead (ADR-0098). A live action. When both are the same scene,
    /// the edits made since the program was held go on air (ADR-0063); `Ok(false)` when
    /// there is nothing to put on air.
    fn transition(&mut self) -> Result<bool, Refusal> {
        let Some(preview) = self.preview() else {
            return Err(Refusal::Conflict(STUDIO_MODE_OFF.to_owned()));
        };
        let program = self.scene;
        if preview == program {
            // `apply` holds the scene as it is now.
            return Ok(self.held.take().is_some());
        }
        self.collection
            .set_program(self.canvas, Some(preview))
            .map_err(|e| Refusal::of(&e))?;
        self.scene = preview;
        if self.swap_after_transition {
            let _ = self.collection.set_preview(self.canvas, Some(program));
        }
        Ok(true)
    }

    /// Holds the program scene while it is also the preview scene (ADR-0063), and lets
    /// it go otherwise. A copy is taken only when the hold starts, so later edits to
    /// the scene reach the program only through a transition.
    fn sync_held(&mut self) {
        self.held = next_held(self.held.take(), &self.collection, self.canvas);
    }

    /// The collection the studio runs: the user's own, or with the program held
    /// (ADR-0063) a run collection with the copy on air. A copy that no longer fits
    /// (a duplicate name, say) is taken again from the scene as it is now; if even a
    /// fresh copy does not fit, nothing is held.
    fn run_collection(&mut self) -> Option<SceneCollection> {
        self.sync_held();
        let held = self.held.as_ref()?;
        if let Some(run) = run_copy(&self.collection, self.canvas, held) {
            return Some(run);
        }
        // A fresh copy of the same scene, still on air; whatever is previewed, the hold
        // goes on (ADR-0098).
        self.held = copy_of(&self.collection, held.from);
        let run = run_copy(&self.collection, self.canvas, self.held.as_ref()?);
        if run.is_none() {
            self.held = None;
        }
        run
    }

    /// Keeps a program scene on the canvas and [`Host::scene`] on it: after an undo or
    /// redo removed the scene on air, the first scene the canvas may show goes on air
    /// (a new one if there is none), outside the undo stack (ADR-0059).
    fn keep_program(&mut self) {
        let program = self
            .collection
            .canvas(self.canvas)
            .and_then(|c| c.program)
            .filter(|s| self.collection.scene(*s).is_some());
        let scene = match program {
            Some(s) => s,
            None => {
                let first = self.panel_scenes().first().copied();
                let scene = match first {
                    Some(s) => s,
                    None => {
                        let name = unique_scene_name(&self.collection, "Scene");
                        match self.collection.add_scene(&name, SceneScope::Shared) {
                            Ok((s, _)) => s,
                            Err(_) => return,
                        }
                    }
                };
                if self
                    .collection
                    .set_program(self.canvas, Some(scene))
                    .is_err()
                {
                    return;
                }
                scene
            }
        };
        self.scene = scene;
    }

    /// The scenes and sources as the UI and `scene.list` show them.
    fn scene_list(&self) -> SceneList {
        let mut sources: Vec<SourceEntry> = self
            .collection
            .sources()
            .map(|s| SourceEntry {
                id: s.id,
                name: s.name.clone(),
                kind: SourceKind::of(&s.type_id),
                global: self.global_item(s.id).is_some(),
                glide: s.glide,
            })
            .collect();
        sources.sort_by_key(|s| s.name.to_lowercase());
        SceneList {
            scenes: self
                .collection
                .showable_scenes(self.canvas)
                .into_iter()
                .map(|s| SceneRow {
                    id: s.id,
                    name: s.name.clone(),
                })
                .collect(),
            program: self.scene,
            preview: self.preview(),
            master: self
                .collection
                .canvas(self.canvas)
                .map(|c| c.master)
                .unwrap_or_default(),
            glide: self.collection.glide(),
            sources,
        }
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
    ) -> Result<bool, Refusal> {
        let valid = position.iter().chain(size.iter()).all(|v| v.is_finite())
            && size.iter().all(|v| *v != 0.0);
        if !valid {
            return Err(Refusal::Invalid(
                "Position and size must be numbers, and the size cannot be 0.".into(),
            ));
        }
        let (_, item) = self.item_of(id).map_err(Refusal::NotFound)?;
        // The layout the canvas shows: its own if the item has one, else the default.
        let mut layout = self.shown_layout(item);
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
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(id);
        self.history
            .record(EditKind::Place, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// The vertical canvas, when its boxes can be edited: it shows the scene being
    /// edited, which is the scene on air outside studio mode (it follows the main
    /// program and shows no fallback).
    fn vertical_edits(&self) -> Option<CanvasId> {
        let v = self.vertical?;
        let shown = self.vertical_shown()?;
        (self.preview().is_none() && shown == self.edited()).then_some(v.canvas)
    }

    /// The scene the vertical canvas shows now. A following canvas shows its leader's
    /// program scene (or its fallback), not its own `program`, which is only what it
    /// showed before it followed.
    fn vertical_shown(&self) -> Option<SceneId> {
        let v = self.vertical?;
        let canvas = self.collection.canvas(v.canvas)?;
        if canvas.follow.is_some() {
            self.collection
                .followed_programs(&BTreeMap::new())
                .get(&v.canvas)
                .copied()
        } else {
            canvas.program
        }
    }

    /// Arranges `scene` (default: the one the vertical canvas shows) for the vertical
    /// canvas, one undo step (`canvas.auto_layout`, ADR-0093 decision 4). The number of
    /// layouts written.
    fn auto_layout_vertical(
        &mut self,
        mode: AutoLayout,
        scene: Option<SceneId>,
    ) -> Result<u32, Refusal> {
        let v = self
            .vertical
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        let scene = match scene {
            Some(s) => s,
            None => self
                .vertical_shown()
                .ok_or_else(|| Refusal::NotFound("The vertical canvas shows no scene.".into()))?,
        };
        let shown = self
            .collection
            .scene(scene)
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", scene.get())))?;
        if self.collection.is_global_scene(scene)
            || !self.collection.scenes_for(v.canvas).any(|s| s.id == scene)
        {
            return Err(Refusal::Conflict(
                "This scene cannot be shown on the vertical canvas.".into(),
            ));
        }
        let name = shown.name.clone();
        let count = shown
            .items
            .iter()
            .filter(|i| i.visible)
            .filter(|i| match i.content {
                ItemContent::Source(id) => self.sizes.contains_key(&id),
                _ => false,
            })
            .count() as u32;
        let sizes = &self.sizes;
        let undo = self
            .collection
            .auto_layout(
                v.canvas,
                scene,
                mode,
                &|source| sizes.get(&source).copied(),
                &BTreeMap::new(),
            )
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Transform, &name, 0, vec![undo]);
        Ok(count)
    }

    /// Changes the vertical canvas's size (`canvas.set_vertical_size`, ADR-0093
    /// decision 8): its compositor restarts, layouts stay in its layout space
    /// (ADR-0110), and the undo history stays. `Ok(false)` when it has that size.
    fn set_vertical_size(&mut self, size: Size) -> Result<bool, Refusal> {
        let v = self
            .vertical
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        if v.size == size {
            return Ok(false);
        }
        if self.outs[slot(OutputCanvas::Vertical)].busy() {
            return Err(Refusal::Conflict(
                "Stop the recording or stream of the vertical canvas first.".to_owned(),
            ));
        }
        let fps = self
            .collection
            .canvas(v.canvas)
            .map(|c| c.frame_rate)
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        let mut draft = self.collection.clone();
        draft
            .set_canvas_format(v.canvas, size, fps)
            .map_err(|e| Refusal::of(&e))?;
        let run = run_copy_or(&self.collection, self.canvas, self.held.as_ref());
        self.studio
            .deactivate_canvas(
                run.as_ref().unwrap_or(&self.collection),
                &self.adapters.sources,
                v.canvas,
            )
            .map_err(|e| match e {
                RecordError::CanvasBusy(_) => Refusal::Conflict(
                    "Stop the recording or stream of the vertical canvas first.".to_owned(),
                ),
                e => Refusal::Failed(e.to_string()),
            })?;
        let before = std::mem::replace(&mut self.collection, draft);
        let started = {
            let extra = self.vertical_extra(v.canvas);
            let run = self.run_collection();
            let running = run.as_ref().unwrap_or(&self.collection);
            self.studio
                .activate_canvas(running, &self.adapters.sources, extra)
        };
        match started {
            Ok(_) => {
                self.vertical = Some(VerticalState { size, ..v });
                self.waiting = self.studio.waiting();
                Ok(true)
            }
            Err(e) => {
                // The canvas runs again at the size it had.
                self.collection = before;
                let extra = self.vertical_extra(v.canvas);
                let run = self.run_collection();
                let running = run.as_ref().unwrap_or(&self.collection);
                if self
                    .studio
                    .activate_canvas(running, &self.adapters.sources, extra)
                    .is_err()
                {
                    self.vertical = None;
                }
                self.waiting = self.studio.waiting();
                Err(Refusal::Failed(format!(
                    "The vertical canvas did not start at that size: {e}"
                )))
            }
        }
    }

    /// Places a video source's picture on the vertical canvas (ADR-0093 decision 4):
    /// the box, in that canvas's pixels, becomes the item's own layout there, so the
    /// main canvas does not change (U15). Starts from what the vertical canvas shows.
    /// `Ok(false)` when the box is already there.
    fn set_vertical_box(
        &mut self,
        id: SourceId,
        position: [f32; 2],
        size: [f32; 2],
        gesture: u64,
    ) -> Result<bool, Refusal> {
        let valid = position.iter().chain(size.iter()).all(|v| v.is_finite())
            && size.iter().all(|v| *v != 0.0);
        if !valid {
            return Err(Refusal::Invalid(
                "Position and size must be numbers, and the size cannot be 0.".into(),
            ));
        }
        if self.vertical.is_none() {
            return Err(Refusal::NotFound("There is no vertical canvas.".into()));
        }
        let canvas = self.vertical_edits().ok_or_else(|| {
            Refusal::Conflict(
                "The vertical canvas shows another scene: its boxes are edited while it shows the scene on air outside studio mode."
                    .into(),
            )
        })?;
        let (_, item) = self.item_of(id).map_err(Refusal::NotFound)?;
        if self
            .globals()
            .is_some_and(|g| self.scene_of(item.id) == Some(g))
        {
            return Err(Refusal::Conflict(
                "Global sources are not on the vertical canvas yet.".into(),
            ));
        }
        let mut layout = self
            .collection
            .placed_layout(canvas, item.layout_on(canvas));
        if !item.overrides.contains_key(&canvas)
            && layout.position == position
            && layout.size == size
        {
            return Ok(false);
        }
        layout.position = position;
        layout.size = size;
        // Compared in layout space: a box mapped out and back is not always the same
        // float, and the same box must not make a second undo step (ADR-0110).
        let stored = self.collection.stored_layout(canvas, &layout);
        if item.overrides.get(&canvas) == Some(&stored) {
            return Ok(false);
        }
        let item = item.id;
        let undo = self
            .collection
            .set_item_layout(item, Some(canvas), stored)
            .map_err(|e| Refusal::of(&e))?;
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
        on: OutputCanvas,
    ) -> Result<bool, String> {
        if !style.is_valid() {
            return Err(STYLE_ERROR.into());
        }
        let Some((_, found)) = self.collection.item(item) else {
            return Err("This source is not in the scene any more.".to_owned());
        };
        let found = found.clone();
        let (mut layout, own) = match on {
            OutputCanvas::Main => (
                self.shown_layout(&found),
                found
                    .overrides
                    .contains_key(&self.canvas)
                    .then_some(self.canvas),
            ),
            OutputCanvas::Vertical => {
                let (_, layout, own) = self.crop_target_on(item, on).map_err(|e| e.text())?;
                (layout, own)
            }
        };
        if layout.style == style {
            return Ok(false);
        }
        layout.style = style;
        let undo = self
            .save_live_layout(item, own, layout)
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
        self.crop_target_on(item, OutputCanvas::Main)
    }

    /// The canvas a window edit of `on` changes: the studio's canvas, or the vertical
    /// canvas while it shows the scene being edited (ADR-0093 decision 4).
    fn edit_canvas(&self, on: OutputCanvas) -> Result<CanvasId, Refusal> {
        match on {
            OutputCanvas::Main => Ok(self.canvas),
            OutputCanvas::Vertical => {
                if self.vertical.is_none() {
                    return Err(Refusal::NotFound("There is no vertical canvas.".into()));
                }
                self.vertical_edits().ok_or_else(|| {
                    Refusal::Conflict(
                        "The vertical canvas shows another scene: its sources are edited while it shows the scene on air outside studio mode."
                            .into(),
                    )
                })
            }
        }
    }

    /// The width and height of canvas `id` in pixels.
    fn canvas_px(&self, id: CanvasId) -> Option<(u32, u32)> {
        self.collection
            .canvas(id)
            .map(|c| (c.size.width, c.size.height))
    }

    /// As [`Self::crop_target`], on the canvas `on` names. On the vertical canvas the
    /// layout is always written as that canvas's own (U15); global sources are not on
    /// it yet.
    fn crop_target_on(
        &self,
        item: ItemId,
        on: OutputCanvas,
    ) -> Result<(SourceId, Layout, Option<CanvasId>), CropRefusal> {
        if on == OutputCanvas::Vertical {
            let canvas = self.edit_canvas(on).map_err(|e| match e {
                Refusal::NotFound(t) => CropRefusal::NotFound(t),
                e => CropRefusal::Conflict(e.text().into()),
            })?;
            let (scene, found) = self
                .collection
                .item(item)
                .ok_or_else(|| CropRefusal::NotFound(format!("no item {}", item.get())))?;
            if self.globals() == Some(scene) {
                return Err(CropRefusal::Conflict(
                    "Global sources are not on the vertical canvas yet.".into(),
                ));
            }
            let ItemContent::Source(source) = found.content else {
                return Err(CropRefusal::Invalid(
                    "crop is for source items, not nested scenes".into(),
                ));
            };
            let layout = self
                .collection
                .placed_layout(canvas, found.layout_on(canvas));
            return Ok((source, layout, Some(canvas)));
        }
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
        Ok((source, self.shown_layout(found), own))
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
            .save_live_layout(item, own, layout)
            .map_err(|e| CropRefusal::Failed(edit_error(&e)))?;
        let name = self.name_of(source);
        self.history
            .record(EditKind::Crop, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A new crop for an item's layout on the studio's canvas (ADR-0058); `Ok(false)`
    /// when it already has it.
    fn set_item_crop(
        &mut self,
        item: ItemId,
        crop: Crop,
        keep_scale: bool,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, CropRefusal> {
        let (source, old, own) = self.crop_target_on(item, on)?;
        let size = self.sizes.get(&source).copied();
        let region = self.collection.source(source).and_then(|s| s.region);
        let layout = if keep_scale {
            let size = size.ok_or(CropRefusal::NoPicture)?;
            if crop == old.crop {
                return Ok(false);
            }
            recrop_keeping(old, size, region, crop)
                .ok_or_else(|| CropRefusal::Invalid(NO_AREA.into()))?
        } else {
            if size.is_some_and(|picture| visible_size(picture, region, crop).is_none()) {
                return Err(CropRefusal::Invalid(NO_AREA.into()));
            }
            old.with_crop(crop)
        };
        self.write_crop(item, source, own, old, layout, gesture)
    }

    /// A transform command from the control API: the edit, the rebuild when something
    /// changed and the answer.
    fn api_transform(
        &mut self,
        item: u32,
        op: TransformOp,
        canvas: OutputCanvas,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let changed = self
            .transform_item(ItemId::from_raw(item), op, 0, canvas)
            .map_err(|e| refused(&e))?;
        if changed {
            self.apply(notify);
        }
        Ok(Response::Done(Empty {}))
    }

    /// Flips, turns, fits or centres an item (ADR-0084) on the layout the canvas shows,
    /// as one undo step; `Ok(false)` when nothing changes. A locked item and an
    /// operation that needs a picture size the source has not reported are refused.
    fn transform_item(
        &mut self,
        item: ItemId,
        op: TransformOp,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, Refusal> {
        let locked = self.collection.item(item).is_some_and(|(_, i)| i.locked);
        let target = self.edit_canvas(on)?;
        let (source, old, own) = self.crop_target_on(item, on).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        if locked {
            return Err(Refusal::Conflict(
                "The item is locked. Unlock it to change its transform.".into(),
            ));
        }
        let canvas = self
            .canvas_px(target)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let picture = || {
            let capture = self.sizes.get(&source).copied().ok_or_else(|| {
                Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
            })?;
            let region = self.collection.source(source).and_then(|s| s.region);
            visible_size(capture, region, old.crop).ok_or_else(|| Refusal::Invalid(NO_AREA.into()))
        };
        let layout = match op {
            TransformOp::FlipHorizontal => old.flipped_horizontally(),
            TransformOp::FlipVertical => old.flipped_vertically(),
            TransformOp::Turn(degrees) => old.turned(degrees),
            TransformOp::SetAngle(degrees) => old.angled(degrees),
            TransformOp::ResetRotation => old.unturned(),
            TransformOp::Center(axis) => old.centred(canvas, axis),
            TransformOp::Fit(mode) => old
                .fitted(canvas, picture()?, mode)
                .ok_or_else(|| Refusal::Invalid("The picture or the canvas is empty.".into()))?,
            TransformOp::Reset => old
                .transform_reset(picture()?)
                .ok_or_else(|| Refusal::Invalid("The picture is empty.".into()))?,
        };
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(source);
        self.history
            .record(EditKind::Transform, &name, gesture, vec![undo]);
        Ok(true)
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

    /// Remembers the picture size of every running video source (ADR-0058 decision 2)
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
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        match self.studio.update(running, &self.adapters.sources) {
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
        let scenes = self.scene_list();
        if self.scenes_shown.as_ref() != Some(&scenes) {
            self.scenes_shown = Some(scenes.clone());
            let listed = api_scenes(&scenes);
            self.events
                .changed(StateKey::topic(Topic::SceneChanged), || {
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
            self.rows = rows.clone();
            notify(Update::Sources(rows));
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
                    id: s.id,
                    item: item.id,
                    companion_of: s.companion_of(),
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
                    filters: filters::rows(s, |f| self.studio.audio_filter_meter(f).cloned()),
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

/// Frames per second, rounded to the nearest whole number.
fn rounded_rate(fps: FrameRate) -> u32 {
    let rate = fps.per_second();
    (u64::from(rate.num()) + u64::from(rate.den()) / 2)
        .checked_div(u64::from(rate.den()))
        .map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX))
}

fn failed(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Failed, message)
}

impl ApiTarget<'_> {
    fn run(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        // An output started now would run with what the switch replaces.
        if host.switch_pending.is_some()
            && matches!(
                command,
                C::RecordStart(_) | C::RecordStartToFile(_) | C::StreamStart(_)
            )
        {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a switch of output profiles or scene collections is still being made; try again",
            ));
        }
        match command {
            C::StudioGet => Ok(Response::Studio(api::Snapshot {
                seq: host.events.seq_map([Topic::CanvasChanged]),
                data: api::StudioInfo {
                    canvas: host.api_canvas(),
                    kinds: host.kinds.iter().filter_map(|k| api_kind(*k)).collect(),
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
                data: host.heard.iter().map(api_source).collect(),
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
                let id = if p.global {
                    // A global source (ADR-0065): in every scene, so no scene is named.
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_global(kind, &target).map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        let id = h.add(kind, &target).map_err(failed)?;
                        if p.with_audio {
                            // One undo step for the picture and its sound; a refused
                            // companion takes the picture back out.
                            if let Err(e) = h.add_companion(id, true) {
                                let _ = h.delete(id);
                                return Err(refused(&e));
                            }
                        }
                        Ok(id)
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceSetCompanionAudio(p) => {
                let picture = SourceId::from_raw(p.source);
                if host.collection.source(picture).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                }
                let has = host
                    .collection
                    .sources()
                    .any(|s| s.companion_of() == Some(picture));
                if p.enabled != has {
                    if p.enabled {
                        host.add_companion(picture, false)
                            .map_err(|e| refused(&e))?;
                    } else {
                        host.remove_companion(picture).map_err(|e| refused(&e))?;
                    }
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
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
                notify(Update::Deleted {
                    name,
                    from_scene: false,
                });
                Ok(Response::Done(Empty {}))
            }
            C::SceneList => Ok(Response::Scenes(api::Snapshot {
                seq: host.events.seq_map([Topic::SceneChanged]),
                data: api_scenes(&host.scene_list()),
            })),
            C::SceneAdd(p) => {
                let name = p.name.as_deref();
                let id = host.add_scene(name).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SceneAdded(SceneAdded { scene: id.get() }))
            }
            C::SceneRemove(p) => {
                let done = host.remove_scene(SceneId::from_raw(p.scene));
                // A switch made before a refusal still reaches the studio.
                host.apply(notify);
                done.map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SceneRename(p) => {
                let changed = host
                    .rename_scene(SceneId::from_raw(p.scene), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSources(p) => {
                let id = SceneId::from_raw(p.scene);
                if !host.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {}", p.scene),
                    ));
                }
                Ok(Response::SceneSources(SceneSourceList {
                    scene: p.scene,
                    sources: host.rows_of(id).iter().map(api_source).collect(),
                }))
            }
            C::CanvasSetPreview(p) => {
                let changed = match p.scene {
                    None => host.set_studio_mode(false),
                    Some(raw) => {
                        let id = SceneId::from_raw(raw);
                        if host.preview().is_none() {
                            if !host.listed(id) {
                                return Err(refused(&Refusal::NotFound(format!(
                                    "There is no scene {raw}."
                                ))));
                            }
                            host.set_studio_mode(true)
                                .and_then(|_| host.set_preview_scene(id))
                                .map(|_| true)
                        } else {
                            host.set_preview_scene(id)
                        }
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransition => {
                let changed = host.transition().map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSwitch(p) => {
                let changed = host
                    .switch_scene(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceRename(p) => {
                let changed = host
                    .rename_source(SourceId::from_raw(p.source), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetVolume(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMuted(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMonitor(p) => {
                let changed = host
                    .set_source_monitor(
                        SourceId::from_raw(p.source),
                        monitor::scene_mode(p.monitor),
                    )
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::MonitorStatus => Ok(Response::MonitorStatus(api::Snapshot {
                seq: {
                    host.monitor_api_changed();
                    host.events.seq_map([Topic::MonitorChanged])
                },
                data: host.api_monitor(),
            })),
            C::MonitorDevices => Ok(Response::MonitorDevices(host.api_monitor_devices())),
            C::MonitorSetDevice(p) => {
                host.set_monitor_device(p.device).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetVolume(p) => {
                host.set_monitor_volume(p.volume).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetSolo(p) => {
                host.set_monitor_solo(&p.sources).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMixerHidden(p) => {
                let changed = host
                    .set_source_mixer_hidden(SourceId::from_raw(p.source), p.hidden)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAddVertical(p) => {
                use rbs_api::request::VerticalLayout as L;
                let (width, height) = (p.width.unwrap_or(1080), p.height.unwrap_or(1920));
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(width) || !fits(height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let spec = VerticalCanvas {
                    size: Size::new(width, height),
                    follow: p.follow.unwrap_or(true),
                    layout: p.layout.map(|l| match l {
                        L::Fit => AutoLayout::Fit,
                        L::Fill => AutoLayout::Fill,
                        L::Stack => AutoLayout::Stack,
                    }),
                };
                let done = host.add_vertical(spec).map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAutoLayout(p) => {
                use rbs_api::request::VerticalLayout as L;
                if p.canvas != OutputCanvas::Vertical {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "canvas must be \"vertical\" (arranging the main canvas is not in this version)",
                    ));
                }
                let mode = match p.mode {
                    L::Fit => AutoLayout::Fit,
                    L::Fill => AutoLayout::Fill,
                    L::Stack => AutoLayout::Stack,
                };
                let items = host
                    .auto_layout_vertical(mode, p.scene.map(SceneId::from_raw))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::AutoLayoutDone(rbs_api::request::AutoLayoutDone {
                    items,
                }))
            }
            C::CanvasSetVerticalSize(p) => {
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(p.width) || !fits(p.height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let done = host
                    .set_vertical_size(Size::new(p.width, p.height))
                    .map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasRemoveVertical(_) => {
                if host.vertical.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        "there is no vertical canvas",
                    ));
                }
                let done = host.remove_vertical().map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasSetMasterVolume(p) => {
                let changed = host
                    .set_master(Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasSetMasterMuted(p) => {
                let changed = host
                    .set_master(None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterTypes(p) => Ok(Response::FilterTypes(filters::api_types(p.domain))),
            C::FilterAdd(p) => {
                let settings = p.settings.unwrap_or_default();
                let id = host
                    .add_filter(SourceId::from_raw(p.source), &p.type_id, &settings)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::FilterAdded(rbs_api::request::FilterAdded {
                    filter: id.get(),
                }))
            }
            C::FilterRemove(p) => {
                host.remove_filter(FilterId::from_raw(p.filter))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::FilterMove(p) => {
                if host
                    .move_filter(FilterId::from_raw(p.filter), p.index as usize)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetEnabled(p) => {
                if host
                    .set_filter_enabled(FilterId::from_raw(p.filter), p.enabled)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetSettings(p) => {
                if host
                    .patch_filter(FilterId::from_raw(p.filter), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetSyncOffset(p) => {
                if host
                    .set_source_sync_offset(SourceId::from_raw(p.source), p.offset_ms)
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceRetarget(p) => {
                let target = Target {
                    name: String::new(),
                    adapter: p.adapter,
                    target: p.target,
                    is_default: false,
                    size: None,
                    app: None,
                    recent: None,
                    picker: false,
                    mic: None,
                    sound: false,
                };
                if host
                    .retarget_source(SourceId::from_raw(p.source), &target)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SettingsRecentTargets(p) => {
                let kind = app_kind(p.kind);
                let targets = host
                    .recent_targets(kind)
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
                    .collect();
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets,
                }))
            }
            C::SettingsClearRecentTargets(p) => {
                host.clear_recent_targets(app_kind(p.kind));
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetSettings(p) => {
                let settings = host
                    .source_settings_json(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                Ok(Response::SourceSettings(SourceSettingsInfo {
                    source: p.source,
                    settings,
                }))
            }
            C::SourceSetSettings(p) => {
                if host
                    .patch_source_settings(SourceId::from_raw(p.source), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceReload(p) => {
                if host
                    .reload_source(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                    == Reload::Retargeted
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemRemove(p) => {
                let (_, deleted) = host
                    .remove_from_scene(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemRemoved(ItemRemoved {
                    source_deleted: deleted,
                }))
            }
            C::SceneMove(p) => match host.move_scene(SceneId::from_raw(p.scene), p.direction) {
                Ok(true) => {
                    host.apply(notify);
                    Ok(Response::Done(Empty {}))
                }
                Ok(false) => Ok(Response::Done(Empty {})),
                Err(e) => Err(ApiError::new(ErrorKind::Conflict, e)),
            },
            C::SourceSetActive(p) => {
                match host.set_source_active(SourceId::from_raw(p.source), p.active) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::NotFound, e)),
                }
            }
            C::LookList => Ok(Response::Looks(LookList {
                looks: rbs_scene::look::built_in()
                    .into_iter()
                    .map(|l| LookInfo {
                        id: l.id.to_owned(),
                        name: l.name.to_owned(),
                        group: api_look_group(l.group),
                    })
                    .collect(),
            })),
            C::LookApply(p) => {
                let changed = host
                    .apply_look(SourceId::from_raw(p.source), &p.look)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceAddSample(p) => {
                let id = host.add_sample(&p.sample).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::ItemSetBox(p) => {
                let source = SourceId::from_raw(p.source);
                let changed = match p.canvas {
                    OutputCanvas::Main => {
                        host.set_source_box(source, p.position, p.size, p.gesture)
                    }
                    OutputCanvas::Vertical => {
                        host.set_vertical_box(source, p.position, p.size, p.gesture)
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::BoxPlaced(BoxPlaced {
                    seq: p.seq,
                    ok: true,
                }))
            }
            C::ItemMove(p) => {
                let source = host
                    .collection
                    .item(ItemId::from_raw(p.item))
                    .and_then(|(_, it)| match it.content {
                        ItemContent::Source(id) => Some(id),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            "This source is not in the scene any more.",
                        )
                    })?;
                match host.move_source(source, p.direction) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::Conflict, e)),
                }
            }
            C::GlobalList => Ok(Response::GlobalSources(GlobalSourceList {
                sources: host.global_rows().iter().map(api_source).collect(),
            })),
            C::GlobalAdd(p) => {
                let item = host
                    .make_global(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::GlobalRemove(p) => {
                let done = host
                    .unmake_global(SourceId::from_raw(p.source), p.keep)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(match done {
                    Some(item) => Response::ItemAdded(ItemAdded { item: item.get() }),
                    None => Response::ItemRemoved(ItemRemoved {
                        source_deleted: host
                            .collection
                            .source(SourceId::from_raw(p.source))
                            .is_none(),
                    }),
                })
            }
            C::ItemAdd(p) => {
                let item = host.placing_into(p.scene, |h| {
                    h.add_existing(SourceId::from_raw(p.source))
                        .map_err(|e| refused(&e))
                })?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::ItemSetVisible(p) => {
                let changed = host
                    .set_item_visible(ItemId::from_raw(p.item), p.visible)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetLocked(p) => {
                let changed = host
                    .set_item_locked(ItemId::from_raw(p.item), p.locked)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetShowFor(p) => {
                let changed = host
                    .set_item_show_for(ItemId::from_raw(p.item), p.show_for_ms)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemShow(p) => {
                let started = host
                    .show_item(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                if started {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemFlip(p) => {
                let op = match p.axis {
                    Axis::Horizontal => TransformOp::FlipHorizontal,
                    Axis::Vertical => TransformOp::FlipVertical,
                };
                host.api_transform(p.item, op, p.canvas, notify)
            }
            C::ItemRotate(p) => {
                if p.degrees % 90 != 0 || p.degrees == 0 || p.degrees.abs() > 360 {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "degrees must be a non-zero multiple of 90 from -360 to 360",
                    ));
                }
                host.api_transform(
                    p.item,
                    TransformOp::Turn(p.degrees as f32),
                    p.canvas,
                    notify,
                )
            }
            C::ItemFit(p) => {
                let mode = match p.mode {
                    FitKind::Fit => FitMode::Fit,
                    FitKind::Fill => FitMode::Fill,
                    FitKind::Stretch => FitMode::Stretch,
                };
                host.api_transform(p.item, TransformOp::Fit(mode), p.canvas, notify)
            }
            C::ItemAlign(p) => {
                let axis = match p.axis {
                    None => CenterAxis::Both,
                    Some(AlignAxis::Both) => CenterAxis::Both,
                    Some(AlignAxis::Horizontal) => CenterAxis::Horizontal,
                    Some(AlignAxis::Vertical) => CenterAxis::Vertical,
                };
                host.api_transform(p.item, TransformOp::Center(axis), p.canvas, notify)
            }
            C::ItemResetTransform(p) => {
                host.api_transform(p.item, TransformOp::Reset, p.canvas, notify)
            }
            C::ItemSetCrop(p) => {
                let [l, t, r, b] = p.crop;
                let item = ItemId::from_raw(p.item);
                match host.set_item_crop(item, Crop::new(l, t, r, b), p.keep_scale, 0, p.canvas) {
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
                            CropRefusal::Conflict(_) => ErrorKind::Conflict,
                            CropRefusal::NoPicture | CropRefusal::Failed(_) => ErrorKind::Failed,
                        },
                        e.text(),
                    )),
                }
            }
            C::ItemSetStyle(p) => {
                let style = scene_style(&p.style)
                    .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
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
                if p.canvas == OutputCanvas::Vertical
                    && host.collection.item(item).map(|(scene, _)| scene) == host.globals()
                {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "Global sources are not on the vertical canvas yet.",
                    ));
                }
                if host
                    .set_item_style(item, style, 0, p.canvas)
                    .map_err(failed)?
                {
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
            C::OutputSetWithVertical(p) => {
                let flags = host.vertical_flags.set(p.kind, p.on);
                if flags != host.vertical_flags {
                    host.vertical_flags = flags;
                    notify(Update::VerticalFlags(flags));
                    host.settings_changed();
                }
                Ok(Response::Done(Empty {}))
            }
            C::OutputProfiles => Ok(Response::Profiles(ProfileList {
                profiles: vec![host.profile.to_api_listing()],
            })),
            C::RecordStart(p) => {
                check_profile(&p.profile)?;
                match p.canvas.unwrap_or_default().single() {
                    Some(one) => {
                        let c = host.api_canvas_of(Some(one))?;
                        let path = host.recording_file(c);
                        self.record(c, path, false)
                    }
                    None => self.start_all(api::OutputKind::Record),
                }
            }
            C::RecordStartToFile(p) => {
                check_profile(&p.profile)?;
                let c = host.api_canvas_of(p.canvas)?;
                let path = PathBuf::from(&p.path);
                if !path.is_absolute() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!("{} is not a full path", p.path),
                    ));
                }
                self.record(c, path, true)
            }
            C::RecordStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Record);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].recording.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
                }
                host.stop_recording(c, None, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::RecordStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].record_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::RecordStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::RecordState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::RecordCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::RecordCanvasState, c),
                        data: api::CanvasRecordState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::StreamStart(p) => {
                let Some(one) = p.target().single() else {
                    return self.start_all(api::OutputKind::Stream);
                };
                let c = host.api_canvas_of(Some(one))?;
                self.stream(c)
            }
            C::StreamStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Stream);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].streaming.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing streams"));
                }
                host.end_stream_asked(c, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::StreamStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].stream_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::StreamStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::StreamState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::StreamCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::StreamCanvasState, c),
                        data: api::CanvasStreamState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::VcamStatus => Ok(Response::VcamStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.camera_changed();
                    host.events.seq_map([Topic::VcamChanged])
                },
                data: host.api_camera(true),
            })),
            C::VcamStart => {
                if host.camera.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera already sends",
                    ));
                }
                if host.camera_factory.is_none() {
                    return Err(failed(
                        "the virtual camera is not available on this computer",
                    ));
                }
                let Some(setup) = &host.camera_setup else {
                    return Err(failed("the studio is still starting; try again"));
                };
                if setup.busy {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera is being changed in Settings; try again",
                    ));
                }
                match &setup.state {
                    Some(InstallState::NotInstalled) => {
                        return Err(failed(
                            "the virtual camera is not added: add it in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::NeedsUpdate { .. }) => {
                        return Err(failed(
                            "the virtual camera needs an update: add it again in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::Unsupported { reason }) => {
                        return Err(failed(format!(
                            "the virtual camera is not available on this computer: {reason}"
                        )));
                    }
                    // Not read yet: starting says whether it is added.
                    _ => {}
                }
                let settings = VirtualCameraSettings::new(setup.width, setup.height, setup.fps);
                host.start_camera_asked(settings, notify, false)
                    .map_err(failed)?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::VcamStop => {
                if host.camera.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera does not send",
                    ));
                }
                host.stop_camera(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplayStatus => Ok(Response::ReplayStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.replay_api_changed();
                    host.events.seq_map([Topic::ReplayChanged])
                },
                data: host.api_replay(),
            })),
            C::ReplayArm => {
                if host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is already armed",
                    ));
                }
                host.arm_replay(notify).map_err(failed)?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplayDisarm => {
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                host.disarm_replay(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplaySave(p) => {
                if let Some(seconds) = p.seconds
                    && !(1..=host.api_replay().length_secs).contains(&seconds)
                {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "seconds must be 1 to {} (the buffer's length)",
                            host.api_replay().length_secs
                        ),
                    ));
                }
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                // The file is written on a worker: the answer says the clip was
                // requested, and `replay.changed` carries where it went.
                host.save_replay(p.seconds)
                    .map_err(|why| ApiError::new(ErrorKind::Conflict, why))?;
                host.replay_changed(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
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
                    output_size_for_canvas(profile.output_size, host.main_canvas_size())?;
                    // A vertical recording without settings of its own runs on these.
                    let uses = host.outs[0].recording.is_some()
                        || (host.vertical_profile.is_none()
                            && host.outs[slot(OutputCanvas::Vertical)].recording.is_some());
                    if uses && profile != host.profile {
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
                        output_size_for_canvas(stream.output_size, host.main_canvas_size())?;
                        if host.outs[0].streaming.is_some() && stream != *current {
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
                        if host.any_recording() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "stop the recording before changing the canvas",
                            ));
                        }
                        if host.any_streaming() {
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
            C::ProfileList => host.library_list(false),
            C::CollectionList => host.library_list(true),
            C::ProfileSwitch(r) => host.library_switch(false, r.id, notify),
            C::CollectionSwitch(r) => host.library_switch(true, r.id, notify),
            C::CollectionSetGlide(p) => {
                let easing = match p.easing {
                    api::GlideEasing::Linear => 0,
                    api::GlideEasing::EaseInOut => 1,
                };
                let changed = host
                    .set_glide(p.duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetGlide(p) => {
                let (custom, duration_ms, easing) = match p.glide {
                    None => (false, 0, 0),
                    Some(g) => (
                        true,
                        g.duration_ms,
                        match g.easing {
                            api::GlideEasing::Linear => 0,
                            api::GlideEasing::EaseInOut => 1,
                        },
                    ),
                };
                let changed = host
                    .set_source_glide(SourceId::from_raw(p.source), custom, duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetRegion(p) => {
                let region = p.region.map(SourceRegion::from_array);
                if host
                    .set_capture_region(SourceId::from_raw(p.source), region)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetRegion(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                if SourceKind::of(&source.type_id).is_some_and(|kind| kind.is_audio()) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "A sound source has no picture.",
                    ));
                }
                Ok(Response::SourceRegion(SourceRegionInfo {
                    source: p.source,
                    region: source.region.map(SourceRegion::to_array),
                }))
            }
            C::SourceSetChrome(p) => {
                let chrome = p.chrome.map(|spec| {
                    let fill = match spec.fill {
                        rbs_api::request::ChromeFill::Cover => ChromeFill::Cover,
                        rbs_api::request::ChromeFill::Fit => ChromeFill::Fit,
                    };
                    DisplayChrome::new(spec.top, spec.right, spec.bottom, spec.left).with_fill(fill)
                });
                if host
                    .set_display_chrome(SourceId::from_raw(p.source), chrome)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetChrome(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                Ok(Response::SourceChrome(SourceChromeInfo {
                    source: p.source,
                    chrome: source.chrome.map(|chrome| ChromeSpec {
                        top: chrome.top,
                        right: chrome.right,
                        bottom: chrome.bottom,
                        left: chrome.left,
                        fill: match chrome.fill {
                            ChromeFill::Cover => rbs_api::request::ChromeFill::Cover,
                            ChromeFill::Fit => rbs_api::request::ChromeFill::Fit,
                            _ => rbs_api::request::ChromeFill::Cover,
                        },
                    }),
                }))
            }
            C::SourceFitChrome(p) => {
                if host
                    .fit_display_chrome(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceResetChrome(p) => {
                if host
                    .reset_display_chrome(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Answered by the endpoint; never reaches a target.
            // `tokens.*` are answered before (`WithTokens`) when the API was attached
            // with the settings.
            C::EventsSubscribe(_) | C::TokensList | C::TokensCreate(_) | C::TokensRevoke(_) => {
                Err(rbs_api::control::unsupported(&command))
            }
        }
    }

    /// Starts recording canvas `c` with its saved settings into `path`.
    fn record(
        &mut self,
        c: OutputCanvas,
        path: PathBuf,
        explicit: bool,
    ) -> Result<Response, ApiError> {
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
        if host.outs[slot(c)].recording.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a recording is already running",
            ));
        }
        let request = host.profile_request(c, path).map_err(|why| match c {
            OutputCanvas::Main => failed(why),
            OutputCanvas::Vertical => failed(format!("the vertical recording: {why}")),
        })?;
        let profile = (!explicit).then(|| API_PROFILE_ID.to_owned());
        let result = host.start_recording(c, &request, profile);
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
                notify(recording_update(c, update));
                Ok(Response::RecordStarted(started))
            }
            Err(why) => Err(failed(why)),
        }
    }
}

impl ApiTarget<'_> {
    /// `record.start` or `stream.start` with `canvas: "all"` (ADR-0111 decision 3): the
    /// main canvas first, whose refusal fails the call; then the vertical one, whose
    /// refusal is only its entry. One step on the control thread.
    fn start_all(&mut self, kind: api::OutputKind) -> Result<Response, ApiError> {
        let mut outputs = Vec::new();
        for c in self.host.output_canvases() {
            let out = &self.host.outs[slot(c)];
            let (running, ending) = match kind {
                api::OutputKind::Record => (out.recording.is_some(), out.ending),
                api::OutputKind::Stream => (out.streaming.is_some(), false),
            };
            let entry = |outcome| api::OutputEntry {
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
            };
            if ending {
                let e = ApiError::new(ErrorKind::Conflict, "the recording is still ending");
                if c == OutputCanvas::Main {
                    return Err(e);
                }
                outputs.push(api::OutputEntry {
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                });
                continue;
            }
            if running {
                outputs.push(entry(api::OutputOutcome::Running));
                continue;
            }
            if kind == api::OutputKind::Stream
                && c != OutputCanvas::Main
                && self
                    .host
                    .stream_profile_of(c)
                    .is_none_or(|p| p.server.is_empty())
            {
                outputs.push(entry(api::OutputOutcome::NotConfigured));
                continue;
            }
            let started = match kind {
                api::OutputKind::Record => {
                    let path = self.host.recording_file(c);
                    self.record(c, path, false)
                }
                api::OutputKind::Stream => self.stream(c),
            };
            match started {
                Ok(Response::RecordStarted(r)) => outputs.push(api::OutputEntry {
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(Response::StreamStarted(r)) => outputs.push(api::OutputEntry {
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(_) => outputs.push(entry(api::OutputOutcome::Started)),
                Err(e) if c == OutputCanvas::Main => return Err(e),
                Err(e) => outputs.push(api::OutputEntry {
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                }),
            }
        }
        if !outputs
            .iter()
            .any(|o| o.outcome == api::OutputOutcome::Started)
        {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                match kind {
                    api::OutputKind::Record => "a recording is already running",
                    api::OutputKind::Stream => "a stream is already running",
                },
            ));
        }
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `record.stop` or `stream.stop` with `canvas: "all"`: stops every running output
    /// of that kind; `conflict` only when none ran.
    fn stop_all(&mut self, kind: api::OutputKind) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let mut outputs = Vec::new();
        for c in host.output_canvases() {
            let out = &host.outs[slot(c)];
            let running = match kind {
                api::OutputKind::Record => out.recording.is_some() && !out.ending,
                api::OutputKind::Stream => out.streaming.is_some(),
            };
            let outcome = if running {
                match kind {
                    api::OutputKind::Record => host.stop_recording(c, None, notify),
                    api::OutputKind::Stream => host.end_stream_asked(c, notify),
                }
                api::OutputOutcome::Stopped
            } else {
                api::OutputOutcome::NotRunning
            };
            outputs.push(api::OutputEntry {
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
            });
        }
        if !outputs
            .iter()
            .any(|o| o.outcome == api::OutputOutcome::Stopped)
        {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                match kind {
                    api::OutputKind::Record => "nothing records",
                    api::OutputKind::Stream => "nothing streams",
                },
            ));
        }
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// Goes live on canvas `c` with its saved stream settings and the key in the key
    /// store.
    fn stream(&mut self, c: OutputCanvas) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if host.outs[slot(c)].streaming.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a stream is already running",
            ));
        }
        // The vertical stream's texts name it, so a client sees which one is missing.
        let which = match c {
            OutputCanvas::Main => "",
            OutputCanvas::Vertical => " for the vertical stream",
        };
        if host.stream_profile.is_none() {
            return Err(failed("the studio is still starting; try again"));
        }
        let Some(profile) = host.stream_profile_of(c) else {
            return Err(failed(format!(
                "no server is saved{which}: enter it in Settings, Stream"
            )));
        };
        if profile.server.is_empty() {
            return Err(failed(format!(
                "no server is saved{which}: enter it in Settings, Stream"
            )));
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
            failed(format!(
                "the saved stream settings{which} cannot be used: see Settings, Stream"
            ))
        })?;
        let result = host.start_stream(c, &request);
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
                notify(stream_update(
                    c,
                    StreamUpdate::Started {
                        url,
                        video_encoder,
                        audio_encoder,
                    },
                ));
                // The stream runs: a panic while its recording starts must not turn
                // the answer into an error (the round recovers the host).
                if rbs_crash::contain(|| host.record_with_stream(c, notify)).is_err() {
                    host.caught = true;
                }
                host.publish(notify);
                Ok(Response::StreamStarted(started))
            }
            Ok(StreamUpdate::NoKey) => Err(failed(format!(
                "no stream key is saved{which}: enter it in Settings, Stream"
            ))),
            Ok(_) => Err(failed("the stream did not start")),
            Err(why) => Err(hide(why)),
        }
    }
}

/// `invalid_params` when an explicit size is not the canvas-aspect fit (ADR-0102
/// decisions 4 and 7). Absent stays the canvas. Even sides and the 2..=7680 range
/// are already refused by the profile's own check.
fn output_size_for_canvas(
    size: Option<crate::record_profile::OutputSize>,
    canvas: (u32, u32),
) -> Result<(), ApiError> {
    let Some(size) = size else {
        return Ok(());
    };
    if output_size_matches(size.pair(), canvas) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorKind::InvalidParams,
            format!(
                "Output size {} x {} does not keep this canvas's shape.",
                size.width, size.height
            ),
        ))
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

/// The control API's name of a kind (protocol 1.34 names the picture kinds too).
fn api_kind(k: SourceKind) -> Option<api::SourceKind> {
    Some(match k {
        SourceKind::Screen => api::SourceKind::Screen,
        SourceKind::Window => api::SourceKind::Window,
        SourceKind::Camera => api::SourceKind::Camera,
        SourceKind::AudioInput => api::SourceKind::AudioInput,
        SourceKind::SystemAudio => api::SourceKind::SystemAudio,
        SourceKind::ApplicationAudio => api::SourceKind::ApplicationAudio,
        SourceKind::Image => api::SourceKind::Image,
        SourceKind::Color => api::SourceKind::Color,
        SourceKind::Media => api::SourceKind::Media,
        SourceKind::Browser => api::SourceKind::Browser,
    })
}

fn app_kind(k: api::SourceKind) -> SourceKind {
    match k {
        api::SourceKind::Screen => SourceKind::Screen,
        api::SourceKind::Window => SourceKind::Window,
        api::SourceKind::Camera => SourceKind::Camera,
        api::SourceKind::AudioInput => SourceKind::AudioInput,
        api::SourceKind::SystemAudio => SourceKind::SystemAudio,
        api::SourceKind::ApplicationAudio => SourceKind::ApplicationAudio,
        api::SourceKind::Image => SourceKind::Image,
        api::SourceKind::Color => SourceKind::Color,
        api::SourceKind::Media => SourceKind::Media,
        api::SourceKind::Browser => SourceKind::Browser,
    }
}

/// The control API's name of a look group. A group added after protocol 1.34 is `other`.
fn api_look_group(group: rbs_scene::look::LookGroup) -> rbs_api::request::LookGroup {
    match group {
        rbs_scene::look::LookGroup::Color => rbs_api::request::LookGroup::Color,
        rbs_scene::look::LookGroup::Effect => rbs_api::request::LookGroup::Effect,
        rbs_scene::look::LookGroup::Shape => rbs_api::request::LookGroup::Shape,
        _ => rbs_api::request::LookGroup::Other,
    }
}

/// The control API's form of a style (ADR-0053).
fn api_style(s: &ItemStyle) -> api::ItemStyle {
    let (shape, radius) = match s.shape {
        Shape::Rectangle { radius } => (api::ShapeKind::Rectangle, radius),
        Shape::Circle => (api::ShapeKind::Circle, 0.0),
        Shape::Ellipse => (api::ShapeKind::Ellipse, 0.0),
        Shape::Triangle => (api::ShapeKind::Triangle, 0.0),
        Shape::Arrow => (api::ShapeKind::Arrow, 0.0),
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
        api::ShapeKind::Triangle => Shape::Triangle,
        api::ShapeKind::Arrow => Shape::Arrow,
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
        visible: row.visible,
        locked: row.locked,
        volume: row.audio.volume,
        muted: row.audio.muted,
        mixer_hidden: row.audio.mixer_hidden,
        monitor: monitor::api_mode(row.audio.monitor),
        companion_of: row.companion_of.map(|c| c.get()),
        filters: row.filters.iter().map(filters::api_row).collect(),
        sync_offset_ms: row.audio.sync_offset_ms,
        glide: row.glide.map(api_glide_source),
        id: row.id.get(),
        name: row.name.clone(),
        kind: row.kind.and_then(api_kind),
        status: match &row.status {
            SourceStatus::Running => api::SourceStatus::Running,
            SourceStatus::Idle => api::SourceStatus::Idle,
            SourceStatus::Failed(why) => api::SourceStatus::Failed {
                reason: why.clone(),
            },
            // The typed state of protocol 1.20 (ADR-0077, decision 7).
            SourceStatus::Unavailable { fault, detail } => api::SourceStatus::Unavailable {
                fault: api_fault(*fault),
                detail: detail.clone(),
            },
        },
    }
}

fn api_glide(g: rbs_scene::CollectionGlide) -> api::Glide {
    api::Glide {
        duration_ms: g.duration_ms,
        easing: api_easing(g.easing),
    }
}

fn api_glide_source(g: rbs_scene::SourceGlide) -> api::Glide {
    api::Glide {
        duration_ms: g.duration_ms,
        easing: api_easing(g.easing),
    }
}

fn api_easing(e: rbs_scene::GlideEasing) -> api::GlideEasing {
    match e {
        rbs_scene::GlideEasing::Linear => api::GlideEasing::Linear,
        // New variants map to today's default so an older app still talks.
        _ => api::GlideEasing::EaseInOut,
    }
}

/// The control API's name for a source fault.
fn api_fault(fault: rbs_core::fault::SourceFault) -> api::SourceFault {
    use rbs_core::fault::SourceFault as F;
    match fault {
        F::TargetGone => api::SourceFault::TargetGone,
        F::PermissionDenied => api::SourceFault::PermissionDenied,
        F::Busy => api::SourceFault::Busy,
        _ => api::SourceFault::Other,
    }
}

/// `scene.list` and `scene.changed` (protocol 1.6).
fn api_scenes(list: &SceneList) -> api::SceneList {
    api::SceneList {
        scenes: list
            .scenes
            .iter()
            .map(|s| api::SceneInfo {
                id: s.id.get(),
                name: s.name.clone(),
            })
            .collect(),
        program: list.program.get(),
        preview: list.preview.map(SceneId::get),
        master: api::Master {
            volume: list.master.volume,
            muted: list.master.muted,
        },
        glide: api_glide(list.glide),
        sources: list
            .sources
            .iter()
            .map(|s| api::SourceEntry {
                id: s.id.get(),
                name: s.name.clone(),
                kind: s.kind.and_then(api_kind),
                global: s.global,
                glide: s.glide.map(api_glide_source),
            })
            .collect(),
    }
}

/// A control API error for a refused edit.
fn refused(e: &Refusal) -> ApiError {
    ApiError::new(
        match e {
            Refusal::NotFound(_) => ErrorKind::NotFound,
            Refusal::Invalid(_) => ErrorKind::InvalidParams,
            Refusal::Conflict(_) => ErrorKind::Conflict,
            Refusal::Failed(_) => ErrorKind::Failed,
        },
        e.text(),
    )
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
/// they never meet one, and the same for every hold of the same scene, so the program's
/// compositor and layers keep their nodes from one hold to the next. A copy's item has
/// the id of the item it copies plus this.
const HELD_IDS: u32 = 0xF000_0000;

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
fn held_copy(collection: &SceneCollection, canvas: CanvasId) -> Option<Held> {
    let c = collection.canvas(canvas)?;
    let program = c.program?;
    if c.preview != Some(program) {
        return None;
    }
    copy_of(collection, program)
}

/// A copy of `program` as it is now, to hold on air (ADR-0063).
fn copy_of(collection: &SceneCollection, program: SceneId) -> Option<Held> {
    let scene = collection.scene(program)?;
    let mut held = scene.clone();
    held.id = SceneId::from_raw(HELD_IDS);
    // Unique in the run collection; read only if a failure names the scene.
    held.name = format!("{} (on air)", scene.name);
    while collection.scenes().any(|s| s.name == held.name) {
        held.name.push('*');
    }
    for item in &mut held.items {
        item.id = ItemId::from_raw(HELD_IDS.checked_add(item.id.get())?);
    }
    Some(Held {
        from: program,
        scene: held,
    })
}

/// What is held after a change (ADR-0063, ADR-0098): nothing outside studio mode; the
/// copy kept while its scene is on air, whatever the preview scene is, less the items
/// whose source or nested scene is gone from the collection; a new copy when the
/// program scene is also the preview scene and nothing of it is held (the hold starts:
/// studio mode entered, a transition, or an undo or redo put another scene on air);
/// nothing otherwise. Only a transition (which drops the copy) or another scene on air
/// ends a hold, so previewing another scene never puts edits on air.
fn next_held(held: Option<Held>, collection: &SceneCollection, canvas: CanvasId) -> Option<Held> {
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
        _ if preview == program => held_copy(collection, canvas),
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
    scenes.push(held.scene.clone());
    SceneCollection::from_parts(
        canvases,
        collection.sources().cloned().collect(),
        scenes,
        collection.id_counters(),
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn a_held_program_keeps_the_scene_as_it_was_and_is_never_listed() {
        let (mut col, canvas, scene, _) = held_collection();
        // Outside studio mode, or with another scene in preview: nothing is held.
        col.set_preview(canvas, None).unwrap();
        assert!(held_copy(&col, canvas).is_none());
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        assert!(held_copy(&col, canvas).is_none());

        // The program scene in preview too: the program shows a copy of it.
        col.set_preview(canvas, Some(scene)).unwrap();
        let held = held_copy(&col, canvas).unwrap();
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
        let held = next_held(Some(held), &col, canvas).unwrap();
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
        assert_eq!(held_copy(&col, canvas).unwrap().scene.id, held.scene.id);
        // Leaving studio mode lets the copy go.
        col.set_preview(canvas, None).unwrap();
        assert!(next_held(Some(held), &col, canvas).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn previewing_another_scene_keeps_the_program_held() {
        // As after a transition that keeps the preview scene (ADR-0098): the program
        // scene is in preview and held, and its items are edited.
        let (mut col, canvas, scene, _) = held_collection();
        let held = held_copy(&col, canvas).unwrap();
        let item = col.scene(scene).unwrap().items[0].id;
        col.remove_item(item).unwrap();
        // Another scene goes to preview: the edit still waits for a transition.
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        let held = next_held(Some(held), &col, canvas).unwrap();
        assert_eq!(held.from, scene);
        let run = run_copy(&col, canvas, &held).unwrap();
        let program = run.canvas(canvas).unwrap().program.unwrap();
        assert_eq!(run.scene(program).unwrap().items.len(), 1, "as it was");
        assert_eq!(run.canvas(canvas).unwrap().preview, Some(other));
        // A copy that must be taken again (it no longer fits) is of the held scene, also
        // with another scene in preview.
        let again = copy_of(&col, held.from).unwrap();
        assert_eq!(again.from, scene);
        assert!(run_copy(&col, canvas, &again).is_some());
        // Without a hold, another scene in preview starts none.
        assert!(next_held(None, &col, canvas).is_none());
        // The other scene on air (a transition): the copy goes; it is in preview too,
        // so the hold starts again with it.
        col.set_program(canvas, Some(other)).unwrap();
        let next = next_held(Some(held), &col, canvas).unwrap();
        assert_eq!(next.from, other);
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn a_held_copy_never_outlives_its_scene_or_its_sources() {
        let (mut col, canvas, scene, src) = held_collection();
        let held = held_copy(&col, canvas).unwrap();
        // A source deleted while held leaves the copy, which still fits.
        let item = col.scene(scene).unwrap().items[0].id;
        col.remove_item(item).unwrap();
        col.delete_source(src).unwrap();
        let kept = next_held(Some(held.clone()), &col, canvas).unwrap();
        assert!(kept.scene.items.is_empty());
        assert!(run_copy(&col, canvas, &kept).is_some());
        // Another scene on air in studio mode with itself in preview (as after an
        // undo removed the held scene): a new copy of that scene.
        let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
        col.set_program(canvas, Some(other)).unwrap();
        col.set_preview(canvas, Some(other)).unwrap();
        let next = next_held(Some(held), &col, canvas).unwrap();
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
