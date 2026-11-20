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
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, SendError, Sender, TryRecvError};
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
pub mod filters;
mod interact;
pub use interact::{InteractState, MAX_INTERACT};
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
mod text_resize;

use crate::canvas_profile::CanvasProfile;
use crate::edit_history::{EditKind, History, HistoryError, StepInfo};
use crate::monitor_settings::MonitorSettings;
use crate::recent_targets::{Recent, RecentTargets};
use crate::record_profile::{
    API_PROFILE_ID, LimitPolicy, RecordProfile, SeparateFiles, output_size_matches,
};
use crate::remote_control::ApiTokens;
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
/// This cap remains for a round that still has to look, and the unit test of
/// [`round_cap`].
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
    /// What the mix hears while the program scene, in Own levels, has an entry for the
    /// source (ADR-0091); `None` when `audio` is what is heard.
    pub heard: Option<SourceAudio>,
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
    /// A text source's stored settings (ADR-0129 decision 13: the Text tab shows
    /// them); `None` for every other kind.
    pub text: Option<Settings>,
    /// A browser source's stored settings (the Page tab shows them, ADR-0109,
    /// ADR-0132, ADR-0149); `None` for every other kind.
    pub browser: Option<Settings>,
    /// Why a browser source is not running, as the control API says it
    /// (`browser_runtime_missing`, `browser_runtime_mismatch`, `browser_cap`).
    pub browser_reason: Option<&'static str>,
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
    /// A live pause holds the item's scroll run on the main canvas, then on the
    /// vertical one (ADR-0150 decision 12: Properties shows Resume).
    pub scroll_paused: [bool; 2],
    /// Which feeds are marked to record separately, and the picture's form
    /// (ADR-0086, ADR-0117).
    pub record_separately: RecordSeparately,
    /// The other sources of its audio device whose sync offset differs, by name
    /// (ADR-0126 decision 9); empty for every other source.
    pub device_peers: Vec<String>,
    /// How the latency of its running audio filters is compensated (ADR-0130 decision
    /// 4); `None` for a source the studio does not run.
    pub latency: Option<rbs_record::AudioLatency>,
    /// The stored playback settings of a media file (ADR-0108); `None` for every other
    /// kind. The play state and cursor travel in [`Update::Media`].
    pub media: Option<MediaSettings>,
    /// The policy in force for a source whose type offers a choice of `run`
    /// (ADR-0152); `None` for every other type, where Properties shows nothing.
    pub activity: Option<ActivityChoice>,
}

/// A source's activity policy in force, as Properties shows it (ADR-0152).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityChoice {
    /// The lowest level at which it runs.
    pub run: rbs_scene::activity::Run,
    /// What it does when it comes back; `None` for a type without one.
    pub on_return: Option<rbs_scene::activity::OnReturn>,
}

impl ActivityChoice {
    fn of(source: &Source) -> Option<Self> {
        let traits = rbs_sources::activity_traits(&source.type_id);
        if traits.runs.len() < 2 {
            return None;
        }
        let effective = traits.effective(Some(&source.activity));
        Some(Self {
            run: effective.run,
            on_return: effective.on_return,
        })
    }
}

/// A media file's stored settings, as Properties shows them (ADR-0108).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSettings {
    /// The file's path.
    pub path: String,
    /// Play again at the end (default on).
    pub looped: bool,
    /// Where the play range starts, in milliseconds of media time (default 0).
    pub start_ms: i64,
    /// Where the play range ends; `None` for the end of the file.
    pub end_ms: Option<i64>,
}

impl MediaSettings {
    fn of(source: &Source) -> Option<Self> {
        if source.type_id.as_str() != types::MEDIA_SOURCE {
            return None;
        }
        let flag = |key: &str| !matches!(source.settings.get(key), Some(SettingValue::Bool(false)));
        Some(Self {
            path: match source.settings.get(keys::PATH) {
                Some(SettingValue::Text(path)) => path.clone(),
                _ => String::new(),
            },
            looped: flag(keys::LOOP),
            start_ms: match source.settings.get(keys::START_MS) {
                Some(SettingValue::Int(ms)) => (*ms).max(0),
                _ => 0,
            },
            end_ms: match source.settings.get(keys::END_MS) {
                Some(SettingValue::Int(ms)) => Some(*ms),
                _ => None,
            },
        })
    }
}

/// The play state of one media source for the UI (ADR-0108 decision 10): sent while
/// any plays, and when one changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTick {
    /// The source.
    pub id: SourceId,
    /// 0 playing, 1 paused, 2 cued, 3 ended, 4 standby, 5 unavailable.
    pub state: u8,
    /// The cursor, in steps of [`MEDIA_TICK`].
    pub cursor_ms: u32,
    /// The file's length; 0 while unknown.
    pub duration_ms: u32,
}

/// How often the play state is sent while a media file plays.
const MEDIA_TICK: Duration = Duration::from_millis(250);

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
            && self.record_separately == other.record_separately
            && self.device_peers == other.device_peers
            && self.latency == other.latency
            && self.media == other.media
            && self.activity == other.activity
            && self.picture == other.picture
            && self.visible == other.visible
            && self.active == other.active
            && self.locked == other.locked
            && self.audio == other.audio
            && self.heard == other.heard
            && self.filters == other.filters
            && self.global == other.global
            && self.also_in == other.also_in
            && self.in_globals == other.in_globals
            && self.target == other.target
            && self.glide == other.glide
            && self.fill == other.fill
            && self.filled == other.filled
            && self.text == other.text
            && self.browser == other.browser
            && self.browser_reason == other.browser_reason
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
            && self.scroll_paused == other.scroll_paused
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
    /// on the undo stack, and it reaches the mixer without rebinding the scene. The
    /// window's fader: while the program scene keeps its own levels and places the
    /// source, that scene's level changes instead (ADR-0091 decision 5).
    SetSourceVolume {
        /// The source.
        id: SourceId,
        /// From 0 to 1.
        volume: f32,
    },
    /// Mute or unmute a source's sound (ADR-0059). A live control: not on the undo
    /// stack. In Own levels, as [`Command::SetSourceVolume`], the scene's mute.
    SetSourceMuted {
        /// The source.
        id: SourceId,
        /// Muted.
        muted: bool,
    },
    /// The Mixer panel is shown (`true`) or hidden. Meters stay on while a control
    /// client subscribes to `audio.levels` (ADR-0143 R2).
    SetMetersUi(bool),
    /// Probe video and audio encoders once and publish the list. Sent before output
    /// settings are shown.
    EnsureEncoders,
    /// Turns a scene's Own levels on (`own` true, copying what is heard) or puts it
    /// back in Shared levels (ADR-0091, `scene.audio.own` / `scene.audio.shared`). One
    /// undo step; a refusal comes back as [`Update::Error`].
    SetSceneOwnLevels {
        /// The scene.
        scene: SceneId,
        /// Own levels on.
        own: bool,
    },
    /// Removes a scene's own level for one source (`scene.audio.reset_source`).
    ResetSceneLevel {
        /// The scene.
        scene: SceneId,
        /// The source.
        source: SourceId,
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
    /// Arranges the scene the vertical canvas shows (`canvas.auto_layout`, protocol
    /// 1.44). One undo step; a refusal comes back as [`Update::Error`].
    AutoLayoutVertical(rbs_api::request::VerticalLayout),
    /// The vertical canvas's size (`canvas.set_vertical_size`, protocol 1.44): its
    /// compositor restarts, [`Update::Vertical`] answers.
    SetVerticalSize(Size),
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
    /// Show loudness on or off (`loudness.set_enabled`, ADR-0092), saved.
    SetLoudnessShown(bool),
    /// Restart the loudness measurement (`loudness.reset`).
    ResetLoudness,
    /// The loudness target (LUFS) and true-peak limit (dBTP), saved (`settings.set`
    /// `loudness`, protocol 1.59).
    SetLoudnessSettings {
        /// LUFS.
        target: f32,
        /// dBTP.
        true_peak_limit: f32,
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
    /// Add the voice chain to a sound source (ADR-0130 decision 10; one undo step).
    AddVoiceChain(SourceId),
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
    /// Make a media file the stinger (ADR-0140): a new media source that no scene
    /// places, and the transition set to Stinger with it, one undo step.
    SetStingerFile(std::path::PathBuf),
    /// Add a browser page (ADR-0109), fitted to the canvas, one undo step.
    AddBrowser {
        /// `http`, `https`, or `file` URL.
        url: String,
    },
    /// Add a text source showing `text` (ADR-0129), its box fitted to the text, one
    /// undo step (Add source › Text).
    AddText {
        /// What it shows.
        text: String,
    },
    /// Fit a text source's box to its text (ADR-0129 decision 13, Fit box to text):
    /// the item keeps its centre and its scale. One undo step.
    FitTextBox(SourceId),
    /// Read the installed font families for the font picker ([`Update::TextFonts`]).
    ListTextFonts,
    /// Start, pause or reset a text source's timer (`text.timer`).
    TextTimer {
        /// The countdown or stopwatch.
        source: SourceId,
        /// What to do.
        action: rbs_record::TimerAction,
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
    /// Open the Measure sync offset window for a source (ADR-0126); answered with
    /// [`Update::SyncMeasure`].
    OpenSyncMeasure(SourceId),
    /// Start measuring in that window against a picture source.
    StartSyncMeasure(SourceId),
    /// The Measure window closed: a measurement it started is cancelled.
    CloseSyncMeasure,
    /// Give the other sources of a source's audio device its offset (ADR-0126
    /// decision 9).
    ApplySyncToDeviceSources(SourceId),
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
        /// Patches that share a non-zero gesture (one drag of a field) are one undo
        /// step; 0 never joins.
        gesture: u64,
    },
    /// Try a source that is not running again with its stored settings (ADR-0077).
    ReloadSource(SourceId),
    /// A message to a browser source's running page: reload it, or run a script
    /// (ADR-0132 decision 5, ADR-0149 decision 3). A reload of a page that does not
    /// run is [`Command::ReloadSource`].
    BrowserControl {
        /// The browser source.
        id: SourceId,
        /// What to send.
        message: rbs_browser_ipc::control::Control,
    },
    /// Forget sign-in on the Page tab (ADR-0132 decision 10): the page restarts with a
    /// fresh profile and the kept one is deleted.
    ForgetBrowserSignIn(SourceId),
    /// Play, pause, seek or restart a media file (ADR-0108 decision 10): a live action,
    /// not an undo step.
    Media {
        /// The media source.
        id: SourceId,
        /// What to do.
        op: MediaOp,
    },
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
    /// The collection's transition (ADR-0128): kind and each kind's settings. One undo
    /// step.
    SetTransition(rbs_scene::CollectionTransition),
    /// Properties' "Runs" and "When it comes back" (ADR-0152): the source's `run`
    /// and `on_return`, its linger kept. A value equal to the type's default is
    /// stored as absent. One undo step; a value the type does not allow is refused.
    SetSourceActivity {
        /// The source.
        id: SourceId,
        /// The lowest level at which it runs.
        run: rbs_scene::activity::Run,
        /// What it does when it comes back; `None` for a type without one.
        on_return: Option<rbs_scene::activity::OnReturn>,
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
    /// Copy a scene with copies of its sources (ADR-0133 decision 4, one undo step).
    DuplicateSceneWithCopies(SceneId),
    /// Duplicate source (ADR-0133 decision 3, one undo step).
    DuplicateSource(SourceId),
    /// Copy attributes of a source (ADR-0133 decision 6): the attribute clipboard.
    CopyAttributes(SourceId),
    /// Ask for what the Reset source (`paste` false) or Paste attributes dialog shows
    /// ([`Update::Attributes`]).
    OpenAttributes {
        /// The source right-clicked.
        source: SourceId,
        /// Paste attributes.
        paste: bool,
    },
    /// Reset source: the chosen groups back to their defaults (one undo step plus the
    /// live groups).
    ResetSource {
        /// The source.
        source: SourceId,
        /// The groups.
        groups: Vec<rbs_scene::AttributeGroup>,
    },
    /// Paste attributes: the chosen groups of the clipboard onto the items (one undo
    /// step plus the live groups).
    PasteAttributes {
        /// The items pasted onto.
        items: Vec<ItemId>,
        /// The groups.
        groups: Vec<rbs_scene::AttributeGroup>,
        /// Add the filters after the target's own instead of replacing them.
        append: bool,
    },
    /// Remove a scene (one undo step). Refused for the last scene the canvas may show;
    /// the scene on air is first switched away from (not undone with the removal).
    RemoveScene(SceneId),
    /// Set or clear a scene's transition into it (ADR-0135; one undo step).
    SetSceneTransition {
        /// The scene.
        id: SceneId,
        /// The override; `None` uses the collection's.
        transition: Option<rbs_scene::SceneTransition>,
    },
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
    /// The T-bar's value (ADR-0135): applied once per round, the latest only.
    TransitionProgress(f32),
    /// Rehearse on Preview (ADR-0135 decision 11): the transition on the Preview
    /// picture only. Nothing happens outside studio mode, while a transition runs or
    /// holds, during a rehearsal, and for Cut or a duration of 0.
    Rehearse,
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
    /// Record the source's feeds as files of their own from the next recording on,
    /// or stop (ADR-0086, ADR-0117, `source.set_record_separately`). One undo step.
    SetRecordSeparately {
        /// The source.
        id: SourceId,
        /// What changes.
        patch: RecordPatch,
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
    /// Sets or clears the scroll of a video source's picture (ADR-0150) on the layout
    /// the canvas shows; refused with [`Update::Error`] when out of range.
    SetSourceScroll {
        /// Edits with the same number other than 0 that follow each other are one undo
        /// step (one field drag); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// The scroll, or `None` to stop scrolling.
        scroll: Option<Scroll>,
        /// The canvas whose layout changes, as for [`Command::SetSourceStyle`].
        canvas: OutputCanvas,
    },
    /// A live call on a source's scroll run (ADR-0150 decision 11): not saved, not
    /// undone.
    ScrollLive {
        /// The source.
        id: SourceId,
        /// The canvas whose run it acts on.
        canvas: OutputCanvas,
        /// Pause, resume, restart or a speed.
        control: ScrollControl,
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
    /// The output profile's stream destinations (ADR-0127 decision 1): the next Go
    /// live of each canvas sends to every enabled, configured one. `true`: the section
    /// did not load, so streaming counts as available (ADR-0148 decision 1).
    SetStreamDestinations(Destinations, bool),
    /// Whether Record and Go live also act on the vertical outputs (ADR-0111), as the
    /// output profile saved them. Sent at start and after each change.
    SetVerticalFlags(VerticalFlags),
    /// The output profile's Separate files settings: whether Record and
    /// `record.start` without `iso` write the separate files (ADR-0086 decision 4),
    /// and the hardware limit policy (ADR-0087 decision 5). Sent at start and after
    /// each change.
    SetSeparateFiles(SeparateFiles),
    /// The vertical canvas's panel shows (`true`) or not (ADR-0093 decision 7): its
    /// picture runs only while it shows, so a closed panel costs nothing. Kept while
    /// there is no vertical canvas, for the next one.
    SetVerticalPicture(bool),
    /// Open projector `id` (chosen by the UI) of a canvas (ADR-0125 decision 2): a
    /// picture of that canvas at its full size, only while it is open. Answered with
    /// [`Update::Projector`]. An `id` already open is moved to `canvas`.
    OpenProjector {
        /// The UI's id for it.
        id: u32,
        /// Whose program it shows.
        canvas: OutputCanvas,
    },
    /// Close projector `id`: its picture goes. Unknown ids change nothing.
    CloseProjector(u32),
    /// Open the Interact window of a browser source (ADR-0132 decision 6): its page is
    /// held at Shown and the UI gets an [`Update::Interact`].
    OpenInteract(SourceId),
    /// Close the Interact window of a browser source.
    CloseInteract(SourceId),
    /// Input from a focused Interact window to its page (never logged).
    InteractInput {
        /// The browser source.
        source: SourceId,
        /// The event, in page pixels.
        event: rbs_browser_ipc::input::InputEvent,
    },
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
    /// A manual marker in every running recording (`marker.add`, ADR-0089 decision
    /// 1); answered with [`Update::Markers`], or [`Update::Error`] when refused.
    AddMarker,
    /// Remove these markers, each a recording's canvas and the marker's index (the
    /// marker notice's Undo); answered with [`Update::Markers`].
    RemoveMarkers(Vec<(OutputCanvas, u32)>),
    /// Pause every running recording, or resume them all when none runs unpaused
    /// (`record.pause_toggle`, ADR-0124); answered with [`Update::RecordPause`], or
    /// [`Update::Error`] when refused.
    PauseRecording,
    /// Hold every live stream, or resume them all when every one is held
    /// (`stream.hold_toggle`, ADR-0124); answered with [`Update::StreamHold`], or
    /// [`Update::Error`] when refused.
    HoldStream,
    /// Break (`output.break_toggle`, ADR-0124 decision 15): hold every live stream and
    /// pause every recording, or resume them all.
    Break,
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
    /// Save the key of stream destination `id` (`None` removes it) under its account
    /// ([`crate::stream_destinations::key_account`], ADR-0127 decision 1); answered
    /// with [`Update::DestinationKey`].
    SaveDestinationKey(u32, Option<Secret>),
    /// Start destination `.0` again while its canvas streams, from the live chip's
    /// list (ADR-0127 decision 17): `stream.destination.start`.
    StartDestination(u32),
    /// Stop destination `.0` while the others go on: `stream.destination.stop`.
    StopDestination(u32),
    /// Remove the key saved for a deleted output profile's account (ADR-0051 decision
    /// 5). A failure is reported with [`Update::Error`].
    RemoveStreamKey(String),
    /// Save an imported stream key under a new output profile's account (ADR-0131
    /// decision 5). A failure is reported with [`Update::Error`].
    SaveImportedKey(String, Secret),
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
    /// Read the OS permission statuses ([`Update::Permissions`], ADR-0141). Fast, never
    /// prompts.
    ReadPermissions,
    /// Show the OS prompt for a permission kind on a worker thread (ADR-0141 decision
    /// 4); [`Update::Permissions`] follows when it is answered. Ignored while a prompt
    /// for the kind is showing.
    RequestPermission(PermissionKind),
    /// Read every add-on's status for Settings › Add-ons ([`Update::AddOns`],
    /// ADR-0145). Reads each add-on's folder; starts nothing.
    ListAddOns,
    /// Download, cancel, update or remove an add-on of the catalog (ADR-0145 decision
    /// 9); progress and the end arrive as [`Update::AddOnChanged`], a refusal as
    /// [`Update::Error`].
    AddOn {
        /// The add-on's id.
        id: String,
        /// What to do.
        action: AddOnAction,
    },
}

/// What [`Command::AddOn`] does: the commands of an add-on's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOnAction {
    /// Download (also Retry): `addon.install`.
    Install,
    /// Update: install the catalog's version, then clean older ones.
    Update,
    /// Cancel a running install.
    Cancel,
    /// Remove every version.
    Remove,
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
    /// `None`: audio only, no picture encoder.
    pub video: Option<VideoSettings>,
    /// Audio bitrate in bit/s. Zero for PCM and FLAC.
    pub audio_bitrate_bps: u64,
    /// Sample width when `video` is `None`: PCM 16, 24 or 32 (float), FLAC 16 or 24.
    /// `None` is 16-bit.
    pub bit_depth: Option<u16>,
    /// Encoded picture size (ADR-0102). `None`: this output's canvas.
    pub size: Option<(u32, u32)>,
    /// Write the separate files of the sources marked to record separately
    /// (ADR-0086). Only the main canvas's recording has them.
    pub iso: bool,
    /// Write an editor project when it stops (ADR-0089 decision 10). Also what makes
    /// the master keep its timing for the manifest; `None` adds no work (P13).
    pub editor_project: Option<crate::record_profile::EditorProject>,
    /// The reaction-time offset and the automatic markers (ADR-0089 decision 3).
    pub markers: crate::record_profile::MarkerSettings,
}

/// A change to a source's separate-file marks (`source.set_record_separately`): an
/// absent field keeps the source's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordPatch {
    /// The main feed: the picture of a picture source, the sound otherwise.
    pub enabled: Option<bool>,
    /// The picture.
    pub video: Option<bool>,
    /// The sound.
    pub audio: Option<bool>,
    /// The picture's form; `Image` only for a still source.
    pub video_as: Option<VideoAs>,
}

impl RecordPatch {
    /// The main feed on or off (the Properties switch, protocol 1.46).
    pub fn enabled(on: bool) -> Self {
        Self {
            enabled: Some(on),
            ..Self::default()
        }
    }

    fn from_api(p: &rbs_api::request::SetRecordSeparately) -> Self {
        Self {
            enabled: p.enabled,
            video: p.video,
            audio: p.audio,
            video_as: p.video_as.map(|form| match form {
                rbs_api::request::VideoAs::Auto => VideoAs::Auto,
                rbs_api::request::VideoAs::Video => VideoAs::Video,
                rbs_api::request::VideoAs::Image => VideoAs::Image,
            }),
        }
    }
}

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

/// The markers of a running recording and where its manifest goes (ADR-0089
/// decisions 1 and 5). The recording's state on the control thread owns them.
#[derive(Debug, Clone)]
struct RecordingMarkers {
    log: rbs_manifest::MarkerLog,
    /// The manifest to write: the start manifest of a recording folder, or the
    /// master alone for a plain recording.
    manifest: rbs_manifest::RecordingManifest,
    /// Its folder and file name.
    dir: PathBuf,
    name: String,
    /// A recording folder's manifest exists from the start; a plain recording
    /// writes one only when it has markers.
    folder: bool,
    /// The manifest changed since start for another reason than markers (the
    /// pictures of image tracks, the stop fields), so the stop writes it.
    dirty: bool,
    /// What each file holds, once the recording stopped (ADR-0089 decision 6): the
    /// master's, then each separate file's by name.
    ended: Option<(Option<rbs_record::MediaSpan>, SeparateMedia)>,
    /// The editor project to write after the stop rewrite (ADR-0089 decision 10).
    project: Option<crate::record_profile::EditorProject>,
    /// The automatic markers this recording takes (ADR-0089 decision 3).
    settings: crate::record_profile::MarkerSettings,
    /// The program scene last marked (or on air at the start), for scene markers.
    scene: Option<SceneId>,
    /// When the recording stopped, from `t0` (master time): the end of a pause still
    /// open then (ADR-0124 decision 10).
    stopped_ns: u64,
    /// When the marker flush runs (host clock, ns): armed by the first marker not
    /// yet on disk, `None` while nothing waits (ADR-0089 decision 5).
    flush_at: Option<u64>,
    /// A flush wrote the manifest during the recording.
    flushed: bool,
    /// The master is Matroska: a clean stop adds the markers as chapters
    /// (ADR-0089 decision 14).
    matroska: bool,
}

impl RecordingMarkers {
    /// The markers' state for a recording that started at `t0_ns` into `path`, or into
    /// the recording folder of `iso`.
    fn new(
        path: &Path,
        t0_ns: u64,
        t0_utc: String,
        iso: Option<&IsoFiles>,
        project: Option<crate::record_profile::EditorProject>,
        settings: crate::record_profile::MarkerSettings,
        scene: Option<SceneId>,
    ) -> Option<Self> {
        let log = rbs_manifest::MarkerLog::new(t0_ns, settings.lead()).ok()?;
        if let Some(iso) = iso {
            return Some(Self {
                log,
                manifest: iso.manifest.clone(),
                dir: iso.folder.clone(),
                name: rbs_manifest::FOLDER_MANIFEST.to_owned(),
                folder: true,
                dirty: false,
                ended: None,
                project,
                settings,
                scene,
                stopped_ns: 0,
                flush_at: None,
                flushed: false,
                matroska: false,
            });
        }
        let file = path.file_name()?.to_str()?;
        let name = rbs_manifest::manifest_name_for(file)?;
        let mut manifest = rbs_manifest::RecordingManifest::new(&t0_utc, t0_ns);
        manifest
            .files
            .push(rbs_manifest::ManifestFile::listed("master", file));
        Some(Self {
            log,
            manifest,
            dir: path.parent()?.to_path_buf(),
            name,
            folder: false,
            dirty: false,
            ended: None,
            project,
            settings,
            scene,
            stopped_ns: 0,
            flush_at: None,
            flushed: false,
            matroska: false,
        })
    }

    /// Keeps what the closed files hold, for the stop rewrite in [`Self::write`].
    fn set_ended(&mut self, master: Option<rbs_record::MediaSpan>, separate: SeparateMedia) {
        self.ended = Some((master, separate));
        self.dirty = true;
    }

    /// The stop rewrite (ADR-0089 decision 6): each file's offset, length and format,
    /// and `complete = true`. A file that wrote nothing keeps no timing. When a file
    /// began before `t0` (a plain recording's first audio packet before the moment its
    /// markers count from), `t0` moves to it so no offset is negative.
    fn apply_ended(&mut self) {
        let Some((master, separate)) = self.ended.take() else {
            return;
        };
        let span_of = |file: &rbs_manifest::ManifestFile| {
            if file.role == "master" {
                master
            } else {
                separate
                    .iter()
                    .find(|(name, _)| *name == file.file_name)
                    .and_then(|(_, span)| *span)
            }
        };
        let earliest = self
            .manifest
            .files
            .iter()
            .filter_map(span_of)
            .map(|s| s.first_ns)
            .min();
        if let Some(first) = earliest {
            self.manifest.move_origin(first);
        }
        let t0 = self.manifest.t0_ns;
        let timings: Vec<_> = self
            .manifest
            .files
            .iter()
            .map(|f| span_of(f).map(|s| s.timing(t0)))
            .collect();
        for (file, timing) in self.manifest.files.iter_mut().zip(timings) {
            if file.role != "image" {
                file.timing = timing;
            }
        }
        // Without the master's timing (a take that did not keep it, or wrote nothing)
        // the manifest stays incomplete, so no project is ever guessed.
        if master.is_some() {
            self.manifest.complete();
        }
    }

    /// Writes the editor project next to the manifest, when the setting asked for
    /// one and the stop rewrite made the manifest complete. `Ok(None)`: none asked.
    fn write_project(&self) -> Result<Option<String>, String> {
        let Some(project) = self.project else {
            return Ok(None);
        };
        let format = match project {
            crate::record_profile::EditorProject::Fcpxml => rbs_project::Format::Fcpxml,
            crate::record_profile::EditorProject::Xmeml => rbs_project::Format::Xmeml,
        };
        let mut options = rbs_project::Options::default();
        options.pause_markers = self.settings.pause_markers;
        rbs_project::export_with(&self.dir, &self.name, format, false, options)
            .map(|done| {
                done.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .map_err(|e| e.to_string())
    }

    /// The markers as chapters inside a Matroska master (ADR-0089 decision 14), after
    /// the stop rewrite of a clean stop. Only when the master's timing is known (the
    /// span is tracked for separate files and the editor project setting): a chapter
    /// is at `t_ns - start_offset_ns`, from the file's first sample. A marker before it
    /// is on it; one after the master's end is left out (the manifest keeps it). A file
    /// that cannot take chapters is left as it is; only a read or write failure is an
    /// error.
    fn write_chapters(&self) -> Result<(), String> {
        if !self.matroska || self.log.markers().is_empty() {
            return Ok(());
        }
        let Some((file, timing)) = self
            .manifest
            .files
            .iter()
            .find(|f| f.role == "master")
            .and_then(|f| f.timing.map(|t| (f, t)))
        else {
            return Ok(());
        };
        let mut chapters: Vec<rbs_mux_matroska::Chapter<'_>> = self
            .log
            .markers()
            .iter()
            .map(|m| rbs_mux_matroska::Chapter {
                start_ns: m.t_ns.saturating_sub(timing.start_offset_ns),
                title: &m.label,
            })
            .filter(|c| c.start_ns <= timing.duration_ns)
            .collect();
        if chapters.is_empty() {
            return Ok(());
        }
        chapters.sort_by_key(|c| c.start_ns);
        let mut media = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join(&file.file_name))
            .map_err(|e| e.to_string())?;
        match rbs_mux_matroska::append_chapters(&mut media, &chapters) {
            Ok(()) => Ok(()),
            Err(rbs_mux_matroska::ChapterError::Io(e)) => Err(e.to_string()),
            // Written by another Matroska adapter, or by an output that could not
            // seek: the file takes no chapters, and the manifest keeps the markers.
            Err(_) => Ok(()),
        }
    }

    /// The recording paused (`true`) or resumed at `at_ns` on the master clock: later
    /// markers and the manifest are in file time. A pause changes the manifest. A
    /// pause cut at a shared output records how far its join may be off.
    fn paused(&mut self, pause: bool, at_ns: u64, join_error: Option<Duration>) {
        if pause {
            self.log
                .pause_cut(at_ns, join_error.map(|e| e.as_nanos() as u64));
        } else {
            self.log.resume(at_ns);
        }
        self.dirty = true;
    }

    /// Writes the manifest with the markers, at stop. Nothing to write is `Ok`.
    fn write(&mut self) -> Result<(), String> {
        // A plain recording writes a manifest only when it has markers or writes an
        // editor project (ADR-0089 decision 5, owner choice 1).
        if !self.folder && self.log.markers().is_empty() && self.project.is_none() {
            // A flush wrote one for markers since taken back: it goes.
            if self.flushed {
                self.flushed = false;
                let _ = std::fs::remove_file(self.dir.join(&self.name));
            }
            return Ok(());
        }
        if !self.dirty && !self.log.has_pending() {
            return Ok(());
        }
        self.manifest.markers = self.log.markers().to_vec();
        self.manifest.pauses = self.log.pauses().rows(self.stopped_ns);
        self.apply_ended();
        rbs_manifest::write(&self.dir, &self.name, &self.manifest).map_err(|e| e.to_string())?;
        self.log.mark_flushed();
        self.dirty = false;
        Ok(())
    }

    /// The marker flush (ADR-0089 decision 5): the manifest as it stands, with the
    /// markers so far, rewritten while the recording runs, so a crash loses at most
    /// the last 5 seconds of markers. `now_ns` is the host clock. The stop still
    /// rewrites it whole.
    fn flush(&mut self, now_ns: u64) -> Result<(), String> {
        self.flush_at = None;
        if !self.log.has_pending() {
            return Ok(());
        }
        if !self.folder && self.log.markers().is_empty() && self.project.is_none() {
            // Every marker was taken back: a plain recording keeps no manifest.
            self.log.mark_flushed();
            if self.flushed {
                self.flushed = false;
                return std::fs::remove_file(self.dir.join(&self.name)).map_err(|e| e.to_string());
            }
            return Ok(());
        }
        self.manifest.markers = self.log.markers().to_vec();
        let ran = now_ns.saturating_sub(self.log.t0_ns());
        self.manifest.pauses = self.log.pauses().rows(ran);
        rbs_manifest::write(&self.dir, &self.name, &self.manifest).map_err(|e| e.to_string())?;
        self.log.mark_flushed();
        self.flushed = true;
        // The stop rewrite still runs: an open pause and the stop fields change.
        self.dirty = true;
        Ok(())
    }

    /// Lists the pictures each image track wrote, with its counters, for the stop
    /// write (ADR-0117 decision 6).
    fn set_images(&mut self, tracks: &[rbs_record::IsoImageTrack]) {
        for track in tracks {
            let source = rbs_manifest::SourceRef::new(track.source.get());
            let Some(file) = self
                .manifest
                .files
                .iter_mut()
                .find(|f| f.role == "image" && f.source == Some(source))
            else {
                continue;
            };
            file.images = track.files.entries();
            file.image_counts = Some(rbs_manifest::ImageCounts {
                coalesced: track
                    .stats
                    .coalesced
                    .load(std::sync::atomic::Ordering::Relaxed),
                over_cap: track
                    .stats
                    .over_cap
                    .load(std::sync::atomic::Ordering::Relaxed),
            });
            self.dirty = true;
        }
    }
}

fn marker_color(c: Option<rbs_api::request::MarkerColor>) -> Option<rbs_manifest::MarkerColor> {
    use rbs_api::request::MarkerColor as A;
    use rbs_manifest::MarkerColor as M;
    c.map(|c| match c {
        A::Blue => M::Blue,
        A::Red => M::Red,
        A::Orange => M::Orange,
        A::Yellow => M::Yellow,
        A::Green => M::Green,
        A::Purple => M::Purple,
    })
}

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
        /// The names of the sources written as files of their own (ADR-0086), in
        /// track order; empty for a recording of one file.
        separate: Vec<String>,
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

/// What the studio tells the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// An add-on's status changed (ADR-0145): an install's progress or end, a clean
    /// or a removal. Settings › Add-ons asks for the whole list when it opens.
    AddOnChanged(api::AddOnChanged),
    /// Every add-on, answering [`Command::ListAddOns`].
    AddOns(api::AddOnList),
    /// Show this add-on in Settings › Add-ons: Add Source › Browser was chosen while
    /// the browser engine is missing, and no source was added (ADR-0145 decision 8).
    OpenAddOn(String),
    /// The play state of every media source the studio runs (ADR-0108 decision 10).
    Media(Vec<MediaTick>),
    /// The monitor output (ADR-0097): device, volume, whether it is wanted and plays,
    /// its latency, solo and problem. Sent once at the start and when it changes.
    Monitor(api::MonitorStatus),
    /// A held T-bar ended other than by going to 0 (ADR-0135 decision 14): the rail's
    /// T-bar goes back to 0.
    TBarReset,
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
    /// The loudness meter for the Mixer (ADR-0092 decision 7): whether it is shown,
    /// the running meter's values (`None` while off or without audio), the target and
    /// the true-peak limit. Sent once at the start and when any of them changes.
    Loudness(LoudnessView),
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
    /// Video encoders after the one probe, and why recording is unavailable.
    Encoders {
        /// Best first. Empty when none probed successfully.
        encoders: Vec<EncoderInfo>,
        /// `None` when this computer can record video.
        cannot_record: Option<String>,
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
    /// A projector's picture (ADR-0125): where the UI connects, and whether the
    /// projector is open, suspended or closed by the studio. Sent when either changes.
    Projector {
        /// The UI's id.
        id: u32,
        /// Where to connect, once the picture runs.
        picture: Option<PreviewEndpoint>,
        /// Its state.
        state: ProjectorState,
    },
    /// An Interact window's picture (ADR-0132 decision 6): where the UI connects, or
    /// that the studio closed it. Sent when either changes.
    Interact {
        /// The browser source.
        source: SourceId,
        /// Where to connect, once the picture runs.
        picture: Option<PreviewEndpoint>,
        /// Its state.
        state: InteractState,
    },
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
    /// The installed font families, sorted ([`Command::ListTextFonts`]); empty when
    /// no text renderer is installed.
    TextFonts(Vec<String>),
    /// What the Measure sync offset window shows; `None` closes it (ADR-0126).
    SyncMeasure(Option<SyncMeasureView>),
    /// What the attribute clipboard holds (ADR-0133); `None` while it is empty.
    AttributesClipboard(Option<rbs_api::types::AttributesClipboard>),
    /// What the Reset source or Paste attributes dialog shows (ADR-0133).
    Attributes(AttributesView),
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
    /// [`Command::SaveDestinationKey`] done for destination `.0`, as
    /// [`Update::StreamKey`].
    DestinationKey(u32, Result<bool, String>),
    /// A control API client changed the recording settings; the UI shows and saves
    /// them.
    Profile(RecordProfile),
    /// The control API changed whether Record and Go live also act on the vertical
    /// outputs (`output.set_with_vertical`, ADR-0111); the UI saves them in the profile.
    VerticalFlags(VerticalFlags),
    /// A control API client changed the stream settings (not the server or the key);
    /// the UI shows and saves them.
    StreamProfile(StreamProfile),
    /// A control API client turned a stream destination on or off
    /// (`stream.destination.set_enabled`, protocol 1.77); the UI saves the section.
    StreamDestinations(Destinations),
    /// The destinations of a canvas's running stream changed state, counters or error
    /// (ADR-0127 decision 17): the live chip's list; empty once the stream ends.
    LiveDestinations(OutputCanvas, Vec<api::LiveDestination>),
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
    /// The running recordings' manual marker counts (main, vertical; `None` while
    /// that canvas does not record), after a marker was added or removed through the
    /// window or the control API. `placed` is the marker just added (ADR-0089
    /// decision 10: the window's notice).
    Markers {
        /// Manual markers of each canvas's recording.
        counts: [Option<u32>; 2],
        /// The marker just added, if this update is for one.
        placed: Option<MarkerNote>,
    },
    /// The running recordings' pause states (main, vertical; `None` while that canvas
    /// does not record), after a pause or resume through the window or the control
    /// API (ADR-0124).
    RecordPause([Option<RecordPause>; 2]),
    /// The live streams' hold states (main, vertical; `None` while that canvas does
    /// not stream), after a hold or resume through the window or the control API
    /// (ADR-0124).
    StreamHold([Option<StreamHold>; 2]),
    /// [`Command::SwitchCollection`] with this `seq` is done: `Ok` the new collection
    /// runs, `Err` why the one before still runs.
    CollectionSwitched {
        /// The request's number.
        seq: u64,
        /// How it went.
        result: Result<(), String>,
    },
    /// `import.obs.run` wrote a new scene collection and output profile (control API
    /// 1.82): the UI reloads its libraries, and its shortcuts when `shortcuts`.
    ObsImported {
        /// Hotkeys were added to `shortcuts.toml`.
        shortcuts: bool,
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
    /// The OS permission statuses (ADR-0141), answering [`Command::ReadPermissions`]
    /// and sent after every permission request. `waiting` is the kind whose OS prompt
    /// is showing now, if any.
    Permissions {
        /// Every kind with an installed adapter, in page order.
        rows: Vec<(PermissionKind, PermissionInfo)>,
        /// The kind being asked for now.
        waiting: Option<PermissionKind>,
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
    /// The calibration run's clock, pacer, file system and wall time (ADR-0087 step
    /// D1); `None`: the real ones. Tests inject a test clock and a fake sink.
    pub calibration_setup: Option<calibration::CalibrationSetup>,
    /// The add-on catalog, where add-ons are kept and how they are fetched (ADR-0145;
    /// [`crate::add_ons::AddOnsSetup::system`] in the app). `None`: `addon.list`
    /// shows the built-in catalog as not installed and installs are refused.
    pub add_ons: Option<crate::add_ons::AddOnsSetup>,
    /// The browser runner (tests); `None`: [`crate::browser_tree::resolve_runner`].
    pub browser_runner: Option<crate::browser_tree::Runner>,
    /// A step to panic at once (tests only, ADR-0055).
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fault: Option<Fault>,
}

/// Reads a recording folder's free space for the estimate ([`HostConfig::read_free_space`]).
#[derive(Clone)]
pub struct FreeSpaceReader(pub rbs_record::estimate::ReadFreeSpace);

impl std::fmt::Debug for FreeSpaceReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FreeSpaceReader")
    }
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
            calibration_setup: None,
            add_ons: None,
            browser_runner: None,
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

/// The control thread's command channel. Sending also unparks that thread once it
/// has bound itself (ADR-0154). A bare channel send does not wake a `park`.
pub(crate) struct ControlOut {
    tx: Sender<Msg>,
    wake: OnceLock<Arc<rbs_graph::runtime::ControlWake>>,
}

impl ControlOut {
    // The same shape as `Sender::send`: the error hands the message back, so it is as
    // large as `Msg`. Callers only test it; nothing moves it around.
    #[allow(clippy::result_large_err)]
    fn send(&self, msg: Msg) -> Result<(), SendError<Msg>> {
        let sent = self.tx.send(msg);
        if let Some(wake) = self.wake.get() {
            wake.unpark();
        }
        sent
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
        let tx = Arc::new(ControlOut {
            tx,
            wake: OnceLock::new(),
        });
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
        // An idle control thread is parked with no timeout. Dropping the sender
        // disconnects it, but a parked thread does not notice until it is unparked,
        // and the UI thread would wait on the join forever.
        let wake = self.tx.as_ref().and_then(|out| out.wake.get().cloned());
        self.tx = None;
        if let Some(wake) = wake {
            wake.unpark();
        }
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
    browser: Arc<crate::browser_tree::RunnerBrowserTree>,
    /// The installed browser engine the tree passes to each runner (ADR-0145).
    engine_slot: Arc<crate::browser_tree::EngineSlot>,
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
                let _ = out.wake.set(host.studio.control_wake());
            }
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
    // One wake for the soonest timed-show deadline. An empty cue map adds nothing
    // (ADR-0105, P13).
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

/// The main display for the Video settings row at startup (ADR-0102 decision 8).
///
/// On macOS this is the Core Graphics framebuffer and point width. It does not
/// list screen targets. Other systems keep the default screen target's pixel
/// size and scale.
fn startup_main_display(sources: &rbs_sources::SourceTypes) -> Option<MainDisplay> {
    #[cfg(target_os = "macos")]
    {
        // Listing screens would open ScreenCaptureKit shareable content.
        let _ = sources;
        rbs_capture_sck::main_display().map(|(width, height, scale)| MainDisplay {
            size: (width, height),
            scale,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let screens = sources
            .targets(&SourceKind::Screen.type_id())
            .unwrap_or_default();
        screens
            .iter()
            .find(|t| t.is_default)
            .or_else(|| screens.first())
            .and_then(|t| {
                t.size.map(|size| MainDisplay {
                    size,
                    scale: t.scale,
                })
            })
    }
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
    // The collection's canvas, which may differ from `config.size` (only the preview's
    // aspect ratio is chosen from that).
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
    let engine_slot = Arc::new(crate::browser_tree::EngineSlot::default());
    if collection
        .sources()
        .any(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
    {
        engine_slot.set(add_ons.browser_engine());
    }
    let mut tree = crate::browser_tree::RunnerBrowserTree::new(
        config
            .browser_runner
            .or_else(crate::browser_tree::resolve_runner),
        Arc::clone(&engine_slot),
    );
    if let Some(dir) = profiles.clone() {
        tree = tree.with_profiles(dir);
    }
    // The Windows GPU page path (`with_adapter`) stays off until its sync is fixed:
    // the helper waited on the CPU inside CEF's paint callback and froze Interact.
    // Pages use the shared-memory path meanwhile (stability plan WP-A).
    let browser = Arc::new(tree);
    let browser_tree: Option<Arc<dyn BrowserTree>> =
        Some(Arc::clone(&browser) as Arc<dyn BrowserTree>);
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
    let display = startup_main_display(&adapters.sources);
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
                    rbs_crash::errln!("rbs: text timers were not read: {why}");
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
        match c {
            Command::ListTargets(kind) => {
                let targets = self.targets(kind);
                notify(Update::Targets(kind, targets));
            }
            Command::AddSourceWithAudio { ref kind, .. } | Command::AddSource { ref kind, .. }
                if self.prompt_first(*kind).is_some() =>
            {
                if let Some(p) = self.prompt_first(*kind) {
                    self.ask_permission(p, Some(c), notify);
                }
            }
            Command::ReadPermissions => self.send_permissions(notify),
            Command::ListAddOns => notify(Update::AddOns(self.addon_list())),
            Command::AddOn { id, action } => self.addon_command(&id, action, notify),
            Command::RequestPermission(kind) => self.ask_permission(kind, None, notify),
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
                    Ok((name, removed)) => {
                        self.apply(notify);
                        notify(Update::Deleted {
                            name,
                            from_scene: !removed.source_deleted,
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
                let call = if self.levels_take(id) {
                    rbs_api::Command::SceneAudioSet(rbs_api::request::SetSceneLevel {
                        scene: self.scene.get(),
                        source: id.get(),
                        volume: Some(volume),
                        muted: None,
                    })
                } else {
                    rbs_api::Command::SourceSetVolume(rbs_api::request::SetSourceVolume {
                        source: id.get(),
                        volume,
                    })
                };
                self.run_ui_api(call, notify);
            }
            Command::SetSourceMuted { id, muted } => {
                let call = if self.levels_take(id) {
                    rbs_api::Command::SceneAudioSet(rbs_api::request::SetSceneLevel {
                        scene: self.scene.get(),
                        source: id.get(),
                        volume: None,
                        muted: Some(muted),
                    })
                } else {
                    rbs_api::Command::SourceSetMuted(rbs_api::request::SetSourceMuted {
                        source: id.get(),
                        muted,
                    })
                };
                self.run_ui_api(call, notify);
            }
            Command::SetMetersUi(on) => {
                self.meters_ui = on;
                self.sync_meters();
            }
            Command::EnsureEncoders => {
                self.ensure_encoders();
                self.publish_encoders(notify);
            }
            Command::SetSceneOwnLevels { scene, own } => {
                let scene = rbs_api::request::SceneRef { scene: scene.get() };
                self.run_ui_api(
                    if own {
                        rbs_api::Command::SceneAudioOwn(scene)
                    } else {
                        rbs_api::Command::SceneAudioShared(scene)
                    },
                    notify,
                );
            }
            Command::ResetSceneLevel { scene, source } => {
                self.run_ui_api(
                    rbs_api::Command::SceneAudioResetSource(rbs_api::request::SceneSourceRef {
                        scene: scene.get(),
                        source: source.get(),
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
            Command::AutoLayoutVertical(mode) => {
                self.run_ui_api(
                    rbs_api::Command::CanvasAutoLayout(rbs_api::request::CanvasAutoLayout {
                        canvas: OutputCanvas::Vertical,
                        mode,
                        scene: None,
                    }),
                    notify,
                );
            }
            Command::SetVerticalSize(size) => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetVerticalSize(rbs_api::request::SetVerticalSize {
                        width: size.width,
                        height: size.height,
                    }),
                    notify,
                );
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
            Command::SetLoudnessShown(on) => {
                self.run_ui_api(
                    rbs_api::Command::LoudnessSetEnabled(rbs_api::request::SetLoudnessEnabled {
                        on,
                    }),
                    notify,
                );
            }
            Command::ResetLoudness => {
                self.run_ui_api(rbs_api::Command::LoudnessReset, notify);
            }
            Command::SetLoudnessSettings {
                target,
                true_peak_limit,
            } => {
                self.run_ui_api(
                    rbs_api::Command::SettingsSet(rbs_api::request::SettingsPatch {
                        loudness: Some(api::LoudnessSettings {
                            target,
                            true_peak_limit,
                        }),
                        ..Default::default()
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
            Command::AddVoiceChain(source) => {
                let done = self.add_voice_chain(source);
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
            Command::SetStingerFile(path) => match self.set_stinger_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::FitTextBox(id) => {
                let done = self.fit_text_box(id);
                self.answer(done, notify);
            }
            Command::ListTextFonts => self.list_text_fonts(notify),
            Command::TextTimer { source, action } => match self.studio.text_timer(source, action) {
                Ok(_) => self.save_text_timers(),
                Err(rbs_record::TimerRefused::NotATimer) => notify(Update::Error(
                    "Only a countdown for a length or a stopwatch has a timer.".into(),
                )),
                Err(_) => {}
            },
            Command::AddText { text } => match self.add_text(None, &text, false) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            // Without its engine Browser leads to Settings › Add-ons and adds nothing
            // (ADR-0145 decision 8).
            Command::AddBrowser { .. } if self.browser_needs_engine() => {
                notify(Update::OpenAddOn(crate::add_ons::BROWSER_ENGINE.to_owned()));
            }
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
            Command::OpenSyncMeasure(id) => self.open_sync_measure_window(id, notify),
            Command::StartSyncMeasure(reference) => {
                self.start_sync_measure_window(reference, notify);
            }
            Command::CloseSyncMeasure => self.close_sync_measure_window(notify),
            Command::ApplySyncToDeviceSources(id) => {
                let done = self.apply_to_device_sources(id).map(|n| n > 0);
                self.answer_live(done, notify);
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
            Command::SetSourceSettings { id, patch, gesture } => {
                let done = self.patch_source_settings_in(id, &patch, gesture);
                self.answer(done, notify);
            }
            Command::Media { id, op } => {
                if let Err(e) = self.media_command(id.get(), op) {
                    notify(Update::Error(e.text().to_owned()));
                }
                self.media_sent = None;
            }
            Command::ReloadSource(id) => match self.reload_source(id) {
                Ok(Reload::Retargeted) => self.answer(Ok(true), notify),
                Ok(Reload::Restarted(done)) => self.answer_live(Ok(done), notify),
                Err(e) => self.answer_live(Err(e), notify),
            },
            Command::ForgetBrowserSignIn(id) => {
                let done = self
                    .forget_sign_in(id, true)
                    .map(|()| true)
                    .map_err(|e| Refusal::Conflict(e.message));
                self.answer(done, notify);
            }
            Command::BrowserControl { id, message } => {
                let browser = self
                    .collection
                    .source(id)
                    .is_some_and(|s| s.type_id.as_str() == types::BROWSER_SOURCE);
                if !browser {
                    let e = Refusal::NotFound(format!("There is no browser source {}.", id.get()));
                    self.answer_live(Err(e), notify);
                } else if self.studio.browser_control(id, &message) {
                    self.answer_live(Ok(true), notify);
                } else if matches!(message, rbs_browser_ipc::control::Control::Reload { .. }) {
                    match self.reload_source(id) {
                        Ok(Reload::Retargeted) => self.answer(Ok(true), notify),
                        Ok(Reload::Restarted(done)) => self.answer_live(Ok(done), notify),
                        Err(e) => self.answer_live(Err(e), notify),
                    }
                } else {
                    // No page runs: nothing to run the script in.
                    self.answer_live(Ok(false), notify);
                }
            }
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
            Command::SetTransition(t) => {
                let done = self.put_transition(t);
                self.answer(done, notify);
            }
            Command::SetSourceActivity { id, run, on_return } => {
                let done = self.set_source_choice(id, run, on_return);
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
            Command::DuplicateSceneWithCopies(id) => {
                let p = rbs_api::request::DuplicateScene {
                    scene: id.get(),
                    sources: rbs_api::types::DuplicateSources::Copied,
                };
                let done = self.api_scene_duplicate(p, notify);
                self.answer_api(done, notify);
            }
            Command::DuplicateSource(id) => {
                let done = self.api_source_duplicate(in_scene(id), notify);
                self.answer_api(done, notify);
            }
            Command::CopyAttributes(id) => {
                let done = self.api_copy_attributes(in_scene(id), notify);
                self.answer_api(done, notify);
            }
            Command::OpenAttributes { source, paste } => {
                match self.attributes_view(source, paste) {
                    Ok(view) => notify(Update::Attributes(view)),
                    Err(e) => notify(Update::Error(e.text().to_owned())),
                }
            }
            Command::ResetSource { source, groups } => {
                let p = rbs_api::request::ResetSource {
                    source: source.get(),
                    scene: None,
                    canvas: OutputCanvas::default(),
                    groups: Some(groups.iter().map(|g| g.id().to_owned()).collect()),
                };
                let done = self.api_source_reset(p, notify);
                self.answer_api(done, notify);
            }
            Command::PasteAttributes {
                items,
                groups,
                append,
            } => {
                let p = rbs_api::request::PasteAttributes {
                    items: items.iter().map(|i| i.get()).collect(),
                    groups: groups.iter().map(|g| g.id().to_owned()).collect(),
                    filters: if append {
                        rbs_api::types::FilterPasteMode::Append
                    } else {
                        rbs_api::types::FilterPasteMode::Replace
                    },
                    canvas: OutputCanvas::default(),
                };
                let done = self.api_paste_attributes(p, notify);
                self.answer_api(done, notify);
            }
            Command::RemoveScene(id) => {
                let done = self.remove_scene(id).map(|()| true);
                self.answer(done, notify);
            }
            Command::RenameScene { id, name } => {
                let done = self.rename_scene(id, &name);
                self.answer(done, notify);
            }
            Command::SetSceneTransition { id, transition } => {
                let done = self.set_scene_transition(id, transition);
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
            Command::TransitionProgress(t) => {
                if self.preview().is_some() {
                    self.pending_progress = Some(t.clamp(0.0, 1.0));
                }
            }
            Command::Rehearse => {
                if self.preview().is_some()
                    && let Err(e) = self.studio.rehearse(&self.adapters.sources)
                {
                    notify(Update::Error(format!("The rehearsal could not start: {e}")));
                }
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
            Command::SetRecordSeparately { id, patch } => {
                match self.set_record_separately(id, patch) {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e.text().into())),
                }
            }
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
            Command::SetSourceScroll {
                gesture,
                id,
                scroll,
                canvas,
            } => {
                let edit = if scroll.is_some_and(|s| !s.is_valid()) {
                    Err(SCROLL_ERROR.to_owned())
                } else {
                    self.item_of(id)
                        .map(|(_, item)| item.id)
                        .and_then(|item| self.set_item_scroll(item, scroll, gesture, canvas))
                };
                match edit {
                    Ok(true) => self.apply(notify),
                    Ok(false) => {}
                    Err(e) => notify(Update::Error(e)),
                }
            }
            Command::ScrollLive {
                id,
                canvas,
                control,
            } => {
                let target = self.item_of(id).map(|(_, item)| item.id).and_then(|item| {
                    self.edit_canvas(canvas)
                        .map(|c| (item, c))
                        .map_err(|e| e.text().to_owned())
                });
                match target {
                    Ok((item, c)) => match self.studio.scroll_control(item, c, control) {
                        Ok(_) => self.publish(notify),
                        Err(ScrollRefused::NoScroll) => notify(Update::Error(
                            "This source does not scroll on that canvas.".into(),
                        )),
                        Err(_) => notify(Update::Error(
                            "This source is not on a scene a canvas shows, so its scroll is not running."
                                .into(),
                        )),
                    },
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
            Command::SetVerticalStreamProfile(profile) => {
                self.vertical_stream_profile = profile;
                self.availability_changed();
            }
            Command::SetStreamDestinations(destinations, unloaded) => {
                self.destinations = destinations;
                self.destinations_unloaded = unloaded;
                self.availability_changed();
            }
            Command::SetVerticalFlags(flags) => {
                if flags != self.vertical_flags {
                    self.vertical_flags = flags;
                    self.settings_changed();
                }
            }
            Command::SetSeparateFiles(files) => {
                self.separate_files = files.enabled;
                self.limit_policy = files.limit_policy();
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
            Command::OpenProjector { id, canvas } => {
                self.open_projector(id, canvas, notify);
                self.publish(notify);
            }
            Command::OpenInteract(source) => self.open_interact(source, notify),
            Command::CloseInteract(source) => {
                self.close_interact(source);
                self.publish(notify);
            }
            Command::InteractInput { source, event } => self.interact_input(source, event),
            Command::CloseProjector(id) => {
                self.close_projector(id);
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
            Command::AddMarker => {
                self.run_ui_api(
                    rbs_api::Command::MarkerAdd(rbs_api::request::MarkerAdd::default()),
                    notify,
                );
            }
            Command::RemoveMarkers(placed) => {
                for (canvas, index) in placed {
                    self.run_ui_api(
                        rbs_api::Command::MarkerRemove(rbs_api::request::MarkerRemove {
                            canvas,
                            index,
                        }),
                        notify,
                    );
                }
            }
            Command::PauseRecording => {
                let command = rbs_api::Command::RecordPauseToggle(rbs_api::request::OnCanvases {
                    canvas: Some(api::OutputTarget::All),
                });
                match rbs_crash::contain(|| ApiTarget { host: self, notify }.run(command)) {
                    Ok(Ok(_)) => {}
                    // The window's toast says it in a sentence of its own.
                    Ok(Err(e)) => notify(Update::Error(format!(
                        "The recording could not pause or resume: {}.",
                        e.message
                    ))),
                    Err(_) => {
                        self.caught = true;
                        notify(Update::Error(rbs_crash::CAUGHT_TEXT.to_owned()));
                    }
                }
            }
            Command::HoldStream | Command::Break => {
                let (command, what) = match c {
                    Command::HoldStream => (
                        rbs_api::Command::StreamHoldToggle(rbs_api::request::StreamHold {
                            canvas: Some(api::OutputTarget::All),
                            ..rbs_api::request::StreamHold::default()
                        }),
                        "The stream could not hold or resume",
                    ),
                    _ => (
                        rbs_api::Command::OutputBreakToggle,
                        "The break could not start or end",
                    ),
                };
                match rbs_crash::contain(|| ApiTarget { host: self, notify }.run(command)) {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => notify(Update::Error(format!("{what}: {}.", e.message))),
                    Err(_) => {
                        self.caught = true;
                        notify(Update::Error(rbs_crash::CAUGHT_TEXT.to_owned()));
                    }
                }
            }
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
                    self.availability_changed();
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
            Command::StartDestination(id) => {
                self.run_ui_api(
                    rbs_api::Command::StreamDestinationStart(rbs_api::request::DestinationRef {
                        id,
                    }),
                    notify,
                );
            }
            Command::StopDestination(id) => {
                self.run_ui_api(
                    rbs_api::Command::StreamDestinationStop(rbs_api::request::DestinationRef {
                        id,
                    }),
                    notify,
                );
            }
            Command::SaveDestinationKey(id, key) => {
                let account = crate::stream_destinations::key_account(&self.key_account, id);
                let result = match &key {
                    Some(k) => self.keys.write(&account, k).map(|()| true),
                    None => self.keys.remove(&account).map(|()| false),
                }
                .map_err(|e| format!("{} refused the stream key: {e}", self.keys.place()));
                notify(Update::DestinationKey(id, result));
            }
            Command::SetKeyAccount(account) => self.key_account = account,
            Command::SaveImportedKey(account, key) => {
                if let Err(e) = self.keys.write(&account, &key) {
                    notify(Update::Error(format!(
                        "The imported stream key could not be saved in {}: {e}. Enter it in Settings, Stream.",
                        self.keys.place()
                    )));
                }
            }
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
        // The pages of the collection before go: their Interact windows close.
        self.close_all_interacts(notify);
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
        self.save_text_timers();
        self.note_collection();
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
                // The new collection's timers, not the old one's.
                self.studio.set_text_timers(&[]);
                self.text_timers_restored = false;
                self.saved = Some(loaded);
                self.history.clear();
                // The attribute clipboard belongs to the old collection (ADR-0133).
                if self.attribute_clipboard.take().is_some() {
                    self.clipboard_changed(notify);
                }
                Ok(())
            }
            Err(why) => {
                self.note_collection();
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
            loudness: Some(api::LoudnessSettings {
                target: self.loudness_settings.target,
                true_peak_limit: self.loudness_settings.true_peak_limit,
            }),
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

    /// Pauses (`Some(true)`), resumes (`Some(false)`) or toggles (`None`) the
    /// recording of `c` (ADR-0124, control API 1.61) and publishes its new state.
    /// Answers whether it is paused now; `conflict` when nothing records or the
    /// studio refuses.
    fn pause_recording(&mut self, c: OutputCanvas, pause: Option<bool>) -> Result<bool, ApiError> {
        let o = &self.outs[slot(c)];
        let Some(id) = o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1) else {
            return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
        };
        let pause =
            pause.unwrap_or_else(|| !self.studio.output_pause(id).is_some_and(|p| p.paused));
        if !pause && self.outs[slot(c)].hold.pending.is_some() {
            // The stream's hold waits for this pause (ADR-0124 decision 14), as the
            // studio refuses while the hold is on.
            return Err(ApiError::new(
                ErrorKind::Conflict,
                pause_refusal(rbs_record::PauseRefused::Held),
            ));
        }
        let done = if pause {
            self.studio.pause_output(id)
        } else {
            self.studio.resume_output(id)
        };
        let at = done
            .map_err(|e| ApiError::new(ErrorKind::Conflict, pause_refusal(e)))?
            .as_nanos();
        // Markers and the manifest are in file time (ADR-0124 decision 10).
        let join_error = self.studio.output_pause(id).and_then(|p| p.join_error);
        if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
            markers.paused(pause, at, join_error);
        }
        self.refresh_record_pause(c);
        Ok(pause)
    }

    /// Puts the pause fields of `c`'s running recording into its record state, as of
    /// now (published as a change, never per frame).
    fn refresh_record_pause(&mut self, c: OutputCanvas) {
        let Some(id) = self.outs[slot(c)].recording.as_ref().map(|r| r.1) else {
            return;
        };
        let Some(progress) = self.studio.output_counters(id) else {
            return;
        };
        let mut state = self.outs[slot(c)].record_state.clone();
        if let api::RecordState::Recording {
            paused,
            paused_ms,
            recorded_ms,
            ..
        } = &mut state
        {
            *paused = progress.paused;
            *paused_ms = progress.paused_ns / 1_000_000;
            *recorded_ms = progress.recorded_ns / 1_000_000;
            self.set_record_state(c, state);
        }
    }

    /// Puts canvas `c`'s stream on hold (`Some(true)`) with `picture`, takes it back
    /// to the program (`Some(false)`), or toggles (`None`) (ADR-0124 decisions 12 to
    /// 14). A stream sharing its encoder with the recording pauses that recording
    /// first and holds once the pause has begun; Resume stream resumes only a
    /// recording its hold paused. Returns whether the stream is on hold now.
    fn hold_stream(
        &mut self,
        c: OutputCanvas,
        hold: Option<bool>,
        asked: rbs_api::request::StreamHold,
    ) -> Result<bool, ApiError> {
        let conflict = |m: String| ApiError::new(ErrorKind::Conflict, m);
        let Some(id) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return Err(conflict("nothing streams".into()));
        };
        let hold = hold.unwrap_or_else(|| !self.stream_held(c));
        if hold {
            let ask = self.hold_ask(c, asked)?;
            match self.hold_engine(id, ask) {
                Ok(_) => {
                    self.outs[slot(c)].hold.pending = None;
                    self.hold_own_encodings(c, Some(ask));
                }
                Err(rbs_record::HoldRefused::SharedWithRecording) => {
                    if self.outs[slot(c)].hold.pending == Some(ask) {
                        return Err(conflict("the stream is already on hold".into()));
                    }
                    let paused = self.outs[slot(c)]
                        .recording
                        .as_ref()
                        .and_then(|r| self.studio.output_pause(r.1))
                        .is_some_and(|p| p.paused);
                    if !paused {
                        self.pause_recording(c, Some(true))?;
                        self.outs[slot(c)].hold.paused_recording = true;
                    }
                    self.outs[slot(c)].hold.pending = Some(ask);
                }
                Err(e) => return Err(hold_error(e)),
            }
        } else {
            if !self.stream_held(c) {
                return Err(conflict("the stream is not on hold".into()));
            }
            if self.outs[slot(c)].hold.pending.take().is_none() {
                self.studio
                    .release_hold_scene(id, &self.adapters.sources)
                    .map_err(|e| conflict(hold_refusal(e)))?;
                self.hold_own_encodings(c, None);
            }
            if std::mem::take(&mut self.outs[slot(c)].hold.paused_recording) {
                // Ended meanwhile, or already resumed: nothing to resume.
                let _ = self.pause_recording(c, Some(false));
            }
        }
        self.refresh_stream_hold(c);
        Ok(hold)
    }

    /// The hold `asked` names, with the stream settings' "While on hold" for what it
    /// leaves out (protocols 1.65 and 1.67). A scene or a sound with a picture that
    /// is not a scene is `invalid_params`; a scene hold with no scene named or set is
    /// `conflict`.
    fn hold_ask(
        &self,
        c: OutputCanvas,
        asked: rbs_api::request::StreamHold,
    ) -> Result<HoldAsk, ApiError> {
        use rbs_api::request::HoldPicture as P;
        let profile = self.stream_profile_of(c);
        let picture = asked
            .picture
            .unwrap_or_else(|| profile.map(|p| p.hold_picture).unwrap_or_default());
        if picture != P::Scene {
            if asked.scene.is_some() || asked.sound.is_some() {
                return Err(ApiError::new(
                    ErrorKind::InvalidParams,
                    "scene and sound go only with picture \"scene\"",
                ));
            }
            return Ok(HoldAsk {
                picture: hold_picture(picture),
                scene: None,
                sound: rbs_record::HoldSound::Silence,
            });
        }
        let scene = asked
            .scene
            .or_else(|| profile.and_then(|p| p.hold_scene))
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::Conflict,
                    "no hold scene is set: choose one in Settings, Stream",
                )
            })?;
        let sound = asked
            .sound
            .or_else(|| profile.map(|p| p.hold_sound))
            .unwrap_or_default();
        Ok(HoldAsk {
            picture: HoldPicture::Scene,
            scene: Some(SceneId::from_raw(scene)),
            sound: hold_sound(sound),
        })
    }

    /// Asks the studio for the hold `ask`: a scene's (bound and opened first) or a
    /// picture's. A scene left by a change to another picture leaves at once.
    fn hold_engine(
        &mut self,
        id: OutputId,
        ask: HoldAsk,
    ) -> Result<Timestamp, rbs_record::HoldRefused> {
        match ask.scene {
            Some(scene) => {
                self.studio
                    .hold_output_scene(id, scene, ask.sound, &self.adapters.sources)
            }
            None => {
                let at = self.studio.hold_output(id, ask.picture)?;
                self.studio.settle_hold_scenes(&self.adapters.sources);
                Ok(at)
            }
        }
    }

    /// Canvas `c`'s stream is on hold, or a hold waits for its recording's pause.
    fn stream_held(&self, c: OutputCanvas) -> bool {
        let o = &self.outs[slot(c)];
        o.hold.pending.is_some()
            || o.streaming
                .as_ref()
                .and_then(|s| self.studio.output_hold(s.2))
                .is_some_and(|h| h.picture.is_some())
    }

    /// Holds a stream whose hold waits for its recording's pause, once that pause
    /// has begun. Returns how soon to look again while it still waits.
    fn drive_hold(&mut self, c: OutputCanvas) -> Option<Duration> {
        let ask = self.outs[slot(c)].hold.pending?;
        let id = self.outs[slot(c)].streaming.as_ref()?.2;
        match self.hold_engine(id, ask) {
            Err(rbs_record::HoldRefused::SharedWithRecording) => {
                let paused = self.outs[slot(c)]
                    .recording
                    .as_ref()
                    .and_then(|r| self.studio.output_pause(r.1))
                    .is_some_and(|p| p.paused);
                if paused {
                    // The pause begins within two render periods.
                    return Some(ENDING_WAIT);
                }
                // The recording was resumed meanwhile: the hold is given up.
                self.outs[slot(c)].hold = HoldPlan::default();
            }
            Ok(_) => {
                self.outs[slot(c)].hold.pending = None;
                self.hold_own_encodings(c, Some(ask));
            }
            Err(_) => self.outs[slot(c)].hold.pending = None,
        }
        self.refresh_stream_hold(c);
        None
    }

    /// Holds (`Some`) or releases (`None`) every destination of `c` with its own
    /// encoder chain besides the anchor's, so the whole stream holds (ADR-0127 decision
    /// 13). The anchor's chain, and every destination sharing it, is held already. A
    /// refusal leaves that destination as it is: it is only one of the stream's.
    fn hold_own_encodings(&mut self, c: OutputCanvas, ask: Option<HoldAsk>) {
        let anchor = self.outs[slot(c)].streaming.as_ref().map(|s| s.2);
        let own: Vec<OutputId> = self.outs[slot(c)]
            .destinations
            .iter()
            .filter(|d| d.own)
            .filter_map(|d| d.output)
            .filter(|id| Some(*id) != anchor)
            .collect();
        for id in own {
            match ask {
                Some(ask) => {
                    let _ = self.hold_engine(id, ask);
                }
                None => {
                    let _ = self.studio.release_hold_scene(id, &self.adapters.sources);
                }
            }
        }
    }

    /// Puts the hold fields of `c`'s live stream into its stream state, as of now
    /// (published as a change, never per frame).
    fn refresh_stream_hold(&mut self, c: OutputCanvas) {
        let Some(id) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return;
        };
        let pending = self.outs[slot(c)].hold.pending;
        let hold = self.studio.output_hold(id).unwrap_or_default();
        let mut state = self.outs[slot(c)].stream_state.clone();
        if let api::StreamState::Live {
            held,
            held_ms,
            hold_picture,
            hold_scene,
            hold_sound,
            hold_fallback,
            hold_sound_fallback,
            hold_fallback_reason,
            ..
        } = &mut state
        {
            *held = pending.is_some() || hold.picture.is_some();
            *held_ms = hold.held_ns / 1_000_000;
            // A hold waiting for the recording's pause says what it will send.
            let (picture, scene, sound) = match (hold.picture, pending) {
                (Some(p), _) => (Some(p), hold.scene, hold.sound),
                (None, Some(ask)) => (Some(ask.picture), ask.scene, ask.scene.map(|_| ask.sound)),
                (None, None) => (None, None, None),
            };
            *hold_picture = picture.map(api_hold_picture);
            *hold_scene = scene.map(SceneId::get);
            *hold_sound = sound.map(api_hold_sound);
            *hold_fallback = hold.fallback.is_some();
            *hold_sound_fallback = hold.sound_fallback.is_some();
            *hold_fallback_reason = hold
                .fallback
                .map(|f| format!("{f}: showing the last frame"))
                .or_else(|| {
                    hold.sound_fallback
                        .map(|f| format!("{f}: its sound is silent"))
                });
            self.set_stream_state(c, state);
        }
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
        // Every state says whether streaming is available on the canvas (control API
        // 1.93, ADR-0148 decision 10).
        let state = state.with_available(self.stream_available(c));
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
        Ok(())
    }

    /// Starts recording to `r.path`; the file's folder is made first. `gate` is how
    /// a control API start asked for its separate video files (protocol 1.54).
    fn start_recording(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        profile: Option<String>,
        gate: StartGate,
    ) -> Result<RecordingUpdate, StartError> {
        if let Some(why) = self.calibration_blocks() {
            return Err(StartError::Message(format!("{why}.")));
        }
        if self.outs[slot(c)].recording.is_some() {
            return Err(StartError::Message(
                "A recording is already running.".into(),
            ));
        }
        let canvas = self.output_canvas(c)?;
        if !r.path.is_absolute() {
            return Err(format!("{} is not a full path.", r.path.display()).into());
        }
        if c == OutputCanvas::Main && r.iso && self.marks_separate() {
            return self.start_recording_with_tracks(c, r, profile, gate);
        }
        self.start_master(c, canvas, r, &r.path, profile, None)
    }

    /// The start gate of a main recording with separate video files (ADR-0087
    /// decisions 5 and 7, control API 1.54): the plan of the whole start, judged once
    /// before anything is on disk.
    fn gate_start(
        &mut self,
        r: &RecordRequest,
        video: VideoSettings,
        marked: &[SourceId],
        sound: bool,
        gate: StartGate,
    ) -> rbs_record::estimate::GateDecision {
        use rbs_record::estimate as est;
        let out = self.master_settings(None, r, &r.path);
        let separate = rbs_record::IsoVideoStart::new(String::new(), r.container, video);
        let mut request = rbs_record::StartPlanRequest::default();
        request.outputs = vec![rbs_record::PlannedOutput::new(&out, r.encoder.as_deref())];
        request.separate = Some(&separate);
        request.sound_files = sound;
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        // The profile's recording folder: the take folder is new on every start.
        let folder = r.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let limits = self.estimate_limits(declared, &folder);
        let request = match gate {
            StartGate::Older => est::GateRequest::Older,
            StartGate::Asked {
                acknowledge_heavy,
                policy,
            } => est::GateRequest::Asked {
                acknowledge_heavy,
                policy: match policy {
                    LimitPolicy::Ask => est::GatePolicy::Ask,
                    LimitPolicy::Skip => est::GatePolicy::Skip,
                },
            },
        };
        est::gate(&plan, &limits, marked, request)
    }

    /// The `[[skipped]]` entries of the take manifest for `skipped`.
    fn manifest_skipped(&self, skipped: &[api::SkippedTrack]) -> Vec<rbs_manifest::SkippedEntry> {
        skipped
            .iter()
            .map(|t| rbs_manifest::SkippedEntry {
                source: rbs_manifest::SourceRef::new(t.source),
                source_name: self
                    .collection
                    .source(SourceId::from_raw(t.source))
                    .map(|s| s.name.clone()),
                role: wire_name(&t.role),
                reason: wire_name(&t.reason),
            })
            .collect()
    }

    /// A source of the collection is marked to record its picture or its sound
    /// separately.
    fn marks_separate(&self) -> bool {
        self.collection
            .sources()
            .any(|s| !s.record_separately.is_none())
    }

    /// The main recording with its separate files (ADR-0086 decisions 5 to 7): one
    /// folder named like the file would be, holding `master.<ext>`, one file per track
    /// and `recording.rbs.toml`. The start gate (ADR-0087 decisions 5 and 7) decides
    /// first, before the folder is made, so its refusal leaves nothing on disk; when it
    /// skips every video file and nothing else is marked, the take is one file. The
    /// folder must not exist, so nothing outside it is touched. The sound files open
    /// first; a failure stops what was opened, removes the files this start may have
    /// made and refuses the recording (a request with `on_hardware_limit: "skip"`
    /// skips them instead). A marked source the program mix does not hear is left out
    /// and listed in `skipped` (keeping it running is decision 8, a later step).
    fn start_recording_with_tracks(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        profile: Option<String>,
        gate: StartGate,
    ) -> Result<RecordingUpdate, StartError> {
        let canvas = self.output_canvas(c)?;
        let folder = r.path.with_extension("");
        let iso_plan = rbs_record::plan(&self.collection).ok();
        let marked = |role: rbs_record::IsoRole| -> Vec<SourceId> {
            let mut v: Vec<SourceId> = iso_plan
                .iter()
                .flat_map(|p| p.tracks.iter())
                .filter(|t| t.role == role)
                .map(|t| t.source)
                .collect();
            v.sort();
            v
        };
        let marked_video = marked(rbs_record::IsoRole::Video);
        let has_images = !marked(rbs_record::IsoRole::Image).is_empty();
        let sound = marked(rbs_record::IsoRole::Audio);
        let skip_audio_on_failure = matches!(
            gate,
            StartGate::Asked {
                policy: LimitPolicy::Skip,
                ..
            }
        );
        let video_skip = |sources: &[SourceId], reason| -> Vec<api::SkippedTrack> {
            sources
                .iter()
                .map(|s| api::SkippedTrack {
                    source: s.get(),
                    role: api::TrackRole::Video,
                    reason,
                })
                .collect()
        };
        // The gate: once, before the folder is made and before any file opens.
        let mut pictures = None;
        let mut decided: Vec<api::SkippedTrack> = Vec::new();
        if let Some(video) = r.video.clone()
            && !marked_video.is_empty()
        {
            let decision = self.gate_start(r, video, &marked_video, !sound.is_empty(), gate);
            if let Some((why, judged, tracks)) = decision.refusal {
                return Err(gate_refusal(api_refusal(why), &judged, &tracks, &[]));
            }
            let no_encoder = !decision.no_encoder.is_empty();
            let keep = match gate {
                StartGate::Older if !no_encoder => None,
                _ => Some(decision.keep.clone()),
            };
            if no_encoder {
                decided = video_skip(&marked_video, api::SkipReason::OpenFailed);
            } else if keep.is_some() {
                decided = video_skip(&decision.limit, api::SkipReason::Limit);
            }
            let nothing_left = match &keep {
                Some(k) => k.is_empty(),
                None => decision.count == 0,
            };
            if nothing_left && !has_images && sound.is_empty() {
                // One file, as 1.46 writes (ADR-0087 decision 7): nothing separate is
                // left. Its skipped files are only in the answer and `record.status`.
                let skipped = if decided.is_empty() {
                    video_skip(&marked_video, api::SkipReason::Limit)
                } else {
                    decided
                };
                let started = self.start_master(c, canvas, r, &r.path, profile, None)?;
                if let api::RecordState::Recording { skipped: s, .. } =
                    &mut self.outs[slot(c)].record_state
                {
                    *s = skipped;
                    let state = self.outs[slot(c)].record_state.clone();
                    self.set_record_state(c, state);
                }
                return Ok(started);
            }
            pictures = Some(PictureGate {
                keep,
                count: decision.count,
                no_encoder,
                ask: matches!(
                    gate,
                    StartGate::Asked {
                        policy: LimitPolicy::Ask,
                        ..
                    }
                ),
                judged: decision.estimate,
            });
        }
        if let Some(parent) = folder.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot make the folder {}: {e}", parent.display()))?;
        }
        // `create_dir`, not `create_dir_all`: it fails when the folder exists, so a
        // folder RBS did not make is never written into or cleaned up.
        std::fs::create_dir(&folder).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("{} already exists.", folder.display()),
            _ => format!("Cannot make the folder {}: {e}", folder.display()),
        })?;
        let master_name = format!("{}.{}", rbs_record::MASTER_STEM, container_ext(r.container));
        // Every name this start may write, for the cleanup of a refused start.
        let mut planned: Vec<String> = iso_plan
            .as_ref()
            .map(|p| {
                p.tracks
                    .iter()
                    .map(|t| match t.role {
                        rbs_record::IsoRole::Audio => {
                            format!("{}.{}", t.file_name, container_ext(ContainerId::Wav))
                        }
                        rbs_record::IsoRole::Image => {
                            rbs_record::PngWriterNode::file_name(&t.file_name, 1)
                        }
                        _ => format!("{}.{}", t.file_name, container_ext(r.container)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        planned.push(master_name.clone());
        let t0_ns = self.clock.now().as_nanos();
        let t0_utc = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let mut start = IsoAudioStart::new(
            rbs_transport_file::file_url(&folder),
            ContainerId::Wav,
            master_name.clone(),
            t0_utc.clone(),
            t0_ns,
        );
        // WAV, 24-bit PCM: an editor's format with no generation loss. A profile
        // section for it comes with the Separate files card (ADR-0086 decision 4).
        start.audio.codec = CodecId::Pcm;
        start.audio.bitrate_bps = 0;
        start.audio.bit_depth = Some(24);
        start.skip_unheard = true;
        // The sound files open before the master, so they follow a window made now
        // that the master is then started with (ADR-0124).
        let pause = rbs_record::SharedPause::new();
        start.pause = Some(pause.clone());
        let (mut manifest, mut skipped) = match self.studio.start_iso_audio(&self.adapters, &start)
        {
            Ok(started) => {
                let skipped: Vec<api::SkippedTrack> = started
                    .not_heard
                    .iter()
                    .map(|(source, _)| api::SkippedTrack {
                        source: source.get(),
                        role: api::TrackRole::Audio,
                        reason: api::SkipReason::NotHeard,
                    })
                    .collect();
                (started.manifest, skipped)
            }
            Err(e) => {
                // `on_hardware_limit: "skip"` (ADR-0087 decision 5): the take goes on
                // without its sound files.
                let without_sound = iso_plan
                    .as_ref()
                    .filter(|_| skip_audio_on_failure)
                    .and_then(|plan| {
                        rbs_record::manifest_at_start(plan, &master_name, &t0_utc, t0_ns).ok()
                    });
                let Some(mut manifest) = without_sound else {
                    remove_recording_folder(&folder, &planned);
                    return Err(format!("The separate files could not start: {e}").into());
                };
                manifest.files.retain(|f| f.role != "audio");
                let skipped = sound
                    .iter()
                    .map(|s| api::SkippedTrack {
                        source: s.get(),
                        role: api::TrackRole::Audio,
                        reason: api::SkipReason::OpenFailed,
                    })
                    .collect();
                (manifest, skipped)
            }
        };
        skipped.extend(decided);
        skipped.sort_by_key(|s| s.source);
        // Picture tracks open once the master runs (sources draw only then); until
        // then the manifest lists only the files that are open.
        manifest
            .files
            .retain(|f| f.role != "video" && f.role != "image");
        manifest.skipped = self.manifest_skipped(&skipped);
        if let Err(e) = rbs_record::write_manifest(&folder, &manifest) {
            self.studio.stop_iso_audio();
            remove_recording_folder(&folder, &planned);
            return Err(format!("{e}.").into());
        }
        let tracks = manifest
            .files
            .iter()
            .filter(|f| f.role == "audio")
            .filter_map(|f| {
                f.source.map(|source| api::RecordTrack {
                    file: f.file_name.clone(),
                    role: api::TrackRole::Audio,
                    source: source.get(),
                })
            })
            .collect();
        let iso = IsoFiles {
            tracks,
            skipped,
            manifest: manifest.clone(),
            folder: folder.clone(),
            images: Vec::new(),
            gate: pictures,
            software_stopped: Vec::new(),
            pause,
        };
        let master = folder.join(&master_name);
        match self.start_master(c, canvas, r, &master, profile, Some(iso)) {
            Ok(started) => Ok(started),
            Err(e) => {
                self.studio.stop_iso_audio();
                let left = remove_recording_folder(&folder, &planned);
                Err(match e {
                    StartError::Refused(m, why) if !left.is_empty() => StartError::Refused(
                        format!("{m} These files could not be removed: {}.", left.join(", ")),
                        why,
                    ),
                    e => e,
                })
            }
        }
    }

    /// Opens the separate video files and image tracks of the main recording, once its
    /// master runs (ADR-0086 step 3.5, ADR-0117), as the start gate decided (ADR-0087
    /// decisions 5 and 7). A source with no picture, a file the gate skipped, a
    /// recording with no picture or files that cannot open are listed in `skipped`, and
    /// the manifest is written again with what is open. Only a request with
    /// `on_hardware_limit: "ask"` is refused here, when a file cannot open; the video
    /// files that did open are still open and the caller stops them.
    fn start_separate_pictures(
        &mut self,
        files: &mut IsoFiles,
        r: &RecordRequest,
        master: rbs_record::OutputId,
    ) -> Result<(), StartError> {
        let gate = files.gate.take();
        let Ok(plan) = rbs_record::plan(&self.collection) else {
            return Ok(());
        };
        let pictures: Vec<_> = plan
            .tracks
            .iter()
            .filter(|t| {
                matches!(
                    t.role,
                    rbs_record::IsoRole::Video | rbs_record::IsoRole::Image
                )
            })
            .collect();
        if pictures.is_empty() {
            return Ok(());
        }
        let already = |files: &IsoFiles, source: u32, role: api::TrackRole| {
            files
                .skipped
                .iter()
                .any(|s| s.source == source && s.role == role)
        };
        let skip_all = |files: &mut IsoFiles, reason: api::SkipReason| {
            for t in &pictures {
                let role = api_track_role(t.role);
                if !already(files, t.source.get(), role) {
                    files.skipped.push(api::SkippedTrack {
                        source: t.source.get(),
                        role,
                        reason,
                    });
                }
            }
        };
        // A WAV or FLAC take draws no picture and has no encoder settings to match.
        let Some(video) = r.video.clone() else {
            skip_all(files, api::SkipReason::AudioOnly);
            files.skipped.sort_by_key(|s| s.source);
            files.manifest.skipped = self.manifest_skipped(&files.skipped);
            let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
            return Ok(());
        };
        let mut start = rbs_record::IsoVideoStart::new(
            rbs_transport_file::file_url(&files.folder),
            r.container,
            video,
        );
        start.image_dir = Some(files.folder.clone());
        start.t0 = Timestamp::from_nanos(files.manifest.t0_ns);
        // The video files pause with their master (ADR-0124).
        start.pause_with = Some(master);
        start.skip_no_picture = true;
        // A file whose hardware encoder refuses a session is skipped, never moved to a
        // software encoder (ADR-0087 decision 5).
        start.stop_before_software = true;
        start.skip_unopened = true;
        let ask = gate.as_ref().is_some_and(|g| g.ask);
        let no_encoder = gate.as_ref().is_some_and(|g| g.no_encoder);
        if let Some(g) = &gate {
            start.keep.clone_from(&g.keep);
            start.max_video = g.count;
        }
        let kept_video = || -> Vec<rbs_record::estimate::TrackRef> {
            gate.as_ref()
                .and_then(|g| g.keep.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|source| rbs_record::estimate::TrackRef {
                    source,
                    role: rbs_record::IsoRole::Video,
                })
                .collect()
        };
        let started = match self.studio.start_iso_video(&self.adapters, &start) {
            Ok(started) => started,
            Err(e) => {
                if let Some(g) = gate.as_ref().filter(|g| g.ask) {
                    let mut refusal = gate_refusal(
                        api::RefusalReason::OpenFailed,
                        &g.judged,
                        &kept_video(),
                        &[],
                    );
                    if let StartError::Refused(m, _) = &mut refusal {
                        *m = format!("The separate video files could not open: {e}. {m}");
                    }
                    return Err(refusal);
                }
                skip_all(files, api::SkipReason::OpenFailed);
                files.skipped.sort_by_key(|s| s.source);
                files.manifest.skipped = self.manifest_skipped(&files.skipped);
                let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
                return Ok(());
            }
        };
        let unopened: Vec<&rbs_record::IsoSkipped> = started
            .skipped
            .iter()
            .filter(|s| {
                matches!(
                    s.reason,
                    rbs_record::IsoSkip::EncoderRefused | rbs_record::IsoSkip::SoftwareStopped
                )
            })
            .collect();
        let software: Vec<SourceId> = unopened
            .iter()
            .filter(|s| s.reason == rbs_record::IsoSkip::SoftwareStopped)
            .map(|s| s.source)
            .collect();
        if ask
            && !unopened.is_empty()
            && let Some(g) = &gate
        {
            let tracks: Vec<_> = unopened
                .iter()
                .map(|s| rbs_record::estimate::TrackRef {
                    source: s.source,
                    role: s.role,
                })
                .collect();
            return Err(gate_refusal(
                api::RefusalReason::HardwareLimit,
                &g.judged,
                &tracks,
                &software,
            ));
        }
        files.software_stopped = software;
        for s in &started.skipped {
            let role = api_track_role(s.role);
            if already(files, s.source.get(), role) {
                continue;
            }
            files.skipped.push(api::SkippedTrack {
                source: s.source.get(),
                role,
                reason: match s.reason {
                    rbs_record::IsoSkip::Limit if no_encoder => api::SkipReason::OpenFailed,
                    rbs_record::IsoSkip::Limit => api::SkipReason::Limit,
                    rbs_record::IsoSkip::NoPicture => api::SkipReason::NoPicture,
                    // An encoder that refused, a stop before a software encoder, or a
                    // reason added later to the engine, not yet named here.
                    _ => api::SkipReason::OpenFailed,
                },
            });
        }
        let opened = started
            .tracks
            .iter()
            .map(|t| (t.source, rbs_record::IsoRole::Video, t.file_name.clone()))
            .chain(
                started
                    .images
                    .iter()
                    .map(|t| (t.source, rbs_record::IsoRole::Image, t.file_name.clone())),
            );
        for (source, role, file) in opened {
            files.tracks.push(api::RecordTrack {
                file: file.clone(),
                role: api_track_role(role),
                source: source.get(),
            });
            let mut entry = rbs_manifest::ManifestFile::listed(role.as_str(), &file);
            entry.source = Some(rbs_manifest::SourceRef::new(source.get()));
            entry.source_name = self.collection.source(source).map(|s| s.name.clone());
            files.manifest.files.push(entry);
        }
        files.images = started.images;
        files.tracks.sort_by_key(|t| t.source);
        files.skipped.sort_by_key(|s| s.source);
        files.manifest.skipped = self.manifest_skipped(&files.skipped);
        // The start manifest named only the files open before the master. The same
        // folder took that write a moment ago; if this one fails, the files are still
        // valid and the stop write lists the image tracks.
        let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
        Ok(())
    }

    /// The output settings of a recording of `r` into `path` on `canvas`.
    fn master_settings(
        &self,
        canvas: Option<CanvasId>,
        r: &RecordRequest,
        path: &Path,
    ) -> OutputSettings {
        let mut out = OutputSettings::new(rbs_transport_file::file_url(path), r.container);
        out.canvas = canvas;
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            if r.video.is_none() {
                a.codec = if r.container == ContainerId::Flac {
                    CodecId::Flac
                } else {
                    CodecId::Pcm
                };
                a.bitrate_bps = 0;
                a.bit_depth = r.bit_depth;
            } else {
                a.bitrate_bps = r.audio_bitrate_bps;
            }
            a
        });
        // The size this recording starts with. A later settings change waits for the
        // next start (ADR-0102). `None` stays the canvas, including a side above 7680.
        // An audio-only file has no picture size.
        out.size = if r.video.is_none() { None } else { r.size };
        out
    }

    /// The estimate of a start of the saved recording profile on `c` (or on every
    /// canvas with `all`), next to what runs (`record.estimate`, ADR-0087 decision 7).
    /// A recording that runs on a canvas the start covers is left out, as if it had
    /// stopped. Opens nothing and never waits.
    fn record_estimate(
        &mut self,
        c: OutputCanvas,
        all: bool,
        iso: Option<bool>,
    ) -> Result<api::EstimateAnswer, String> {
        let canvases: Vec<OutputCanvas> = if all { self.output_canvases() } else { vec![c] };
        let mut settings = Vec::new();
        let mut encoders = Vec::new();
        let mut replacing = Vec::new();
        let mut separate = None;
        let mut sound_files = false;
        let mut folder = None;
        let mut replacing_separate = false;
        for one in canvases {
            let path = self.recording_file(one);
            let r = self.profile_request(one, path.clone())?;
            let canvas = self.output_canvas(one)?;
            settings.push(self.master_settings(canvas, &r, &path));
            encoders.push(r.encoder.clone());
            if let Some((_, id)) = self.outs[slot(one)].recording {
                replacing.push(id);
            }
            if one == OutputCanvas::Main {
                folder = path.parent().map(Path::to_path_buf);
                replacing_separate = self.outs[slot(one)].recording.is_some();
                if iso.unwrap_or(self.separate_files) && self.marks_separate() {
                    sound_files = true;
                    separate = r.video.clone().map(|video| {
                        rbs_record::IsoVideoStart::new(String::new(), r.container, video)
                    });
                }
            }
        }
        let mut request = rbs_record::StartPlanRequest::default();
        request.outputs = settings
            .iter()
            .zip(&encoders)
            .map(|(s, e)| rbs_record::PlannedOutput::new(s, e.as_deref()))
            .collect();
        request.separate = separate.as_ref();
        request.sound_files = sound_files;
        request.replacing = replacing;
        request.replacing_separate = replacing_separate;
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        let folder = folder.unwrap_or_default();
        let limits = self.estimate_limits(declared, &folder);
        let estimate = rbs_record::estimate::estimate(&plan, &limits);
        let at_most_now = plan
            .video_template
            .is_some()
            .then(|| rbs_record::estimate::separate_video_limit(&estimate));
        Ok(api_estimate(&estimate, at_most_now))
    }

    /// The estimate of what runs now, for `record.start`'s answer (protocol 1.53): the
    /// plan of what the start decided to open, which now runs.
    fn running_estimate(&mut self, c: OutputCanvas) -> Option<api::EstimateSummary> {
        let folder = self.outs[slot(c)].recording.as_ref().and_then(|(p, _)| {
            // A take with separate files is a folder inside the profile's folder.
            let parent = p.parent()?;
            Some(if self.iso.is_some() && c == OutputCanvas::Main {
                parent.parent().unwrap_or(parent).to_path_buf()
            } else {
                parent.to_path_buf()
            })
        })?;
        let request = rbs_record::StartPlanRequest::default();
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        let limits = self.estimate_limits(declared, &folder);
        let e = rbs_record::estimate::estimate(&plan, &limits);
        let mut a = api_estimate(&e, None);
        if c == OutputCanvas::Main
            && let Some(files) = &self.iso
        {
            add_software_fallback(&mut a.reasons, &files.software_stopped);
        }
        Some(api::EstimateSummary {
            level: a.level,
            unknown: a.unknown,
            reasons: a.reasons,
        })
    }

    /// Starts the recording of `c` into `path` with `r`'s settings.
    fn start_master(
        &mut self,
        c: OutputCanvas,
        canvas: Option<CanvasId>,
        r: &RecordRequest,
        path: &Path,
        profile: Option<String>,
        mut iso: Option<IsoFiles>,
    ) -> Result<RecordingUpdate, StartError> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|e| format!("Cannot make the folder {}: {e}", folder.display()))?;
        }
        if path.exists() {
            return Err(format!("{} already exists.", path.display()).into());
        }
        let mut out = self.master_settings(canvas, r, path);
        // The master keeps its timing only for a take that can become an editor
        // project: one with separate files or with the project setting on (P13).
        out.track_span = r.editor_project.is_some() || iso.is_some();
        // Every recording can be paused (ADR-0124): its export and audio encoder
        // follow a window, and its separate files follow the same one.
        out.pausable = true;
        out.shared_pause = iso.as_ref().map(|f| f.pause.clone());
        let started = self.start_output(&out, r.encoder.as_deref());
        inject!(self, Fault::AfterOutputStart);
        if let Ok((id, _, _)) = &started
            && let Some(files) = iso.as_mut()
        {
            let id = *id;
            if let Err(refusal) = self.start_separate_pictures(files, r, id) {
                // A refusal after the master runs (`on_hardware_limit: "ask"`): stop the
                // video files that opened and the master; the caller stops the sound
                // files and removes the folder. An encoder the master shares with a
                // running stream keeps running for it (ADR-0047).
                self.studio.stop_iso_video();
                let _ = self.studio.stop_output(id);
                return Err(refusal);
            }
        }
        let separate: Vec<String> = iso
            .as_ref()
            .map(|f| {
                f.tracks
                    .iter()
                    .filter_map(|t| {
                        let what = match t.role {
                            api::TrackRole::Video => "video",
                            api::TrackRole::Image => "images",
                            _ => "sound",
                        };
                        self.collection
                            .source(SourceId::from_raw(t.source))
                            .map(|s| format!("{} ({what})", s.name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let started = started.map(|(id, video_encoder, audio_encoder)| {
            (
                id,
                RecordingUpdate::Started {
                    path: path.to_path_buf(),
                    video_encoder,
                    audio_encoder,
                    separate,
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
            self.outs[slot(c)].recording = Some((path.to_path_buf(), *id));
            // Markers count from the separate files' `t0`, or from now for one file.
            let (t0_ns, t0_utc) = iso.as_ref().map_or_else(
                || {
                    (
                        self.clock.now().as_nanos(),
                        chrono::Utc::now()
                            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                            .to_string(),
                    )
                },
                |f| (f.manifest.t0_ns, f.manifest.t0_utc.clone()),
            );
            let scene = self.program_for(c);
            self.outs[slot(c)].markers = RecordingMarkers::new(
                path,
                t0_ns,
                t0_utc,
                iso.as_ref(),
                r.editor_project,
                r.markers,
                scene,
            )
            .map(|mut m| {
                m.matroska = r.container == ContainerId::Matroska;
                m
            });
            // Tracked together with the master, after the last panic point of the
            // start, so recovery never finds files without their master (ADR-0055).
            let (tracks, skipped) = iso
                .as_ref()
                .map(|f| (f.tracks.clone(), f.skipped.clone()))
                .unwrap_or_default();
            if c == OutputCanvas::Main {
                self.iso = iso;
            }
            self.set_record_state(
                c,
                api::RecordState::Recording {
                    profile,
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                    tracks,
                    skipped,
                    paused: false,
                    paused_ms: 0,
                    recorded_ms: 0,
                },
            );
        } else if std::fs::metadata(path).is_ok_and(|m| m.len() == 0) {
            // The output may have made the file before a later node failed.
            let _ = std::fs::remove_file(path);
        }
        started.map(|(_, update)| update).map_err(StartError::from)
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

    /// Scene markers (ADR-0089 decision 3): a recording whose canvas now shows
    /// another program scene than it last marked gets a `scene` marker named after
    /// it. Runs after every change that reaches the studio, so a switch, a transition
    /// and an undo each mark once; nested scenes are not program scenes.
    fn mark_scene_changes(&mut self) {
        let now = self.clock.now().as_nanos();
        for c in OutputCanvas::ALL {
            let program = self.program_for(c);
            let name = program
                .and_then(|id| self.collection.scene(id))
                .map(|s| s.name.clone());
            let Some(state) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            if program == state.scene {
                continue;
            }
            state.scene = program;
            let Some(name) = name.filter(|_| state.settings.scene_changes) else {
                continue;
            };
            // A full log refuses it; the recording goes on.
            let _ = state.log.add_auto(
                now,
                rbs_manifest::MarkerKind::Scene,
                &name,
                rbs_manifest::MarkerColor::Purple,
            );
        }
    }

    /// Stream markers (ADR-0089 decision 3): the recording on the stream's canvas
    /// gets `stream_start` or `stream_end` when its settings ask for them.
    fn mark_stream(&mut self, c: OutputCanvas, started: bool) {
        let now = self.clock.now().as_nanos();
        let Some(state) = self.outs[slot(c)].markers.as_mut() else {
            return;
        };
        if !state.settings.stream_events {
            return;
        }
        let (kind, label, color) = if started {
            (
                rbs_manifest::MarkerKind::StreamStart,
                "Stream started",
                rbs_manifest::MarkerColor::Green,
            )
        } else {
            (
                rbs_manifest::MarkerKind::StreamEnd,
                "Stream ended",
                rbs_manifest::MarkerColor::Red,
            )
        };
        let _ = state.log.add_auto(now, kind, label, color);
    }

    /// `marker.add` (ADR-0089 decisions 1 and 11): a manual marker in every running
    /// recording `p.canvas` names (absent: all). Checked on every target before any
    /// marker is added, so a refusal adds none.
    fn add_marker(
        &mut self,
        p: &rbs_api::request::MarkerAdd,
    ) -> Result<rbs_api::request::MarkersAdded, ApiError> {
        let label = rbs_manifest::checked_label(p.label.as_deref())
            .map_err(|e| ApiError::new(ErrorKind::InvalidParams, e.to_string()))?;
        let targets: Vec<OutputCanvas> = match p.canvas.unwrap_or(api::OutputTarget::All).single() {
            Some(one) => vec![self.api_canvas_of(Some(one))?],
            None => OutputCanvas::ALL.to_vec(),
        };
        let running: Vec<OutputCanvas> = targets
            .into_iter()
            .filter(|c| self.outs[slot(*c)].markers.is_some())
            .collect();
        if running.is_empty() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "no recording is running, so there is nothing to mark",
            ));
        }
        if running.iter().any(|c| {
            self.outs[slot(*c)]
                .markers
                .as_ref()
                .is_some_and(|m| m.log.markers().len() >= rbs_manifest::MAX_MARKERS)
        }) {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                rbs_manifest::MarkerError::Full.to_string(),
            ));
        }
        let now = self.clock.now().as_nanos();
        let color = marker_color(p.color);
        let mut markers = Vec::with_capacity(running.len());
        for c in running {
            let Some(state) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            let m = state
                .log
                .add_manual(now, label.as_deref(), color)
                .map_err(|e| ApiError::new(ErrorKind::Conflict, e.to_string()))?;
            markers.push(rbs_api::request::MarkerPlaced {
                canvas: c,
                index: m.index,
                t_ns: m.t_ns,
                while_paused: m.while_paused,
            });
        }
        Ok(rbs_api::request::MarkersAdded { markers })
    }

    /// `marker.remove` (ADR-0089 decision 11).
    fn remove_marker(&mut self, c: OutputCanvas, index: u32) -> Result<(), ApiError> {
        let c = self.api_canvas_of(Some(c))?;
        let Some(state) = self.outs[slot(c)].markers.as_mut() else {
            return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
        };
        if state.log.remove(index) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::NotFound,
                format!("the recording has no marker {index}"),
            ))
        }
    }

    /// The marker flush timers (ADR-0089 decision 5): a recording with a marker not
    /// yet on disk arms a one-shot 5-second timer; when it is due the manifest is
    /// rewritten. Returns the time to the next one. No marker, no timer (P13).
    fn drive_marker_flush(
        &mut self,
        now: rbs_core::time::Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        let now_ns = now.as_nanos();
        let mut next: Option<u64> = None;
        for c in OutputCanvas::ALL {
            let Some(m) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            if !m.log.has_pending() {
                continue;
            }
            let due = *m
                .flush_at
                .get_or_insert(now_ns.saturating_add(MARKER_FLUSH_NS));
            if now_ns < due {
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            if let Err(e) = m.flush(now_ns) {
                notify(Update::Error(format!(
                    "The markers could not be saved yet: {e}"
                )));
            }
        }
        next.map(|due| Duration::from_nanos(due.saturating_sub(now_ns)))
    }

    /// [`Update::StreamHold`]: each live stream's hold state now.
    fn hold_update(&self) -> Update {
        let state = |c: OutputCanvas| {
            self.outs[slot(c)].streaming.as_ref()?;
            let hold = self.outs[slot(c)]
                .streaming
                .as_ref()
                .and_then(|s| self.studio.output_hold(s.2))
                .unwrap_or_default();
            let note = if hold.fallback.is_some() {
                Some(HoldNote::Picture)
            } else if hold.sound_fallback.is_some() {
                Some(HoldNote::Sound)
            } else {
                None
            };
            Some(StreamHold {
                held: self.stream_held(c),
                note,
            })
        };
        Update::StreamHold([state(OutputCanvas::Main), state(OutputCanvas::Vertical)])
    }

    /// [`Update::RecordPause`]: each running recording's pause state now.
    fn pause_update(&self) -> Update {
        let state = |c: OutputCanvas| {
            let o = &self.outs[slot(c)];
            let id = o.recording.as_ref().filter(|_| !o.ending)?.1;
            let progress = self.studio.output_counters(id)?;
            Some(RecordPause {
                paused: progress.paused,
                recorded_ns: progress.recorded_ns,
            })
        };
        Update::RecordPause([state(OutputCanvas::Main), state(OutputCanvas::Vertical)])
    }

    /// [`Update::Markers`]: each running recording's manual marker count and, after
    /// an add, the marker just placed.
    fn markers_update(&self, added: Option<&rbs_api::request::MarkersAdded>) -> Update {
        let manual = |c: OutputCanvas| {
            self.outs[slot(c)].markers.as_ref().map(|m| {
                let n = m
                    .log
                    .markers()
                    .iter()
                    .filter(|k| k.kind == rbs_manifest::MarkerKind::Manual)
                    .count();
                u32::try_from(n).unwrap_or(u32::MAX)
            })
        };
        let placed = added.and_then(|a| {
            let first = a.markers.first()?;
            let label = self.outs[slot(first.canvas)]
                .markers
                .as_ref()?
                .log
                .markers()
                .iter()
                .find(|m| m.index == first.index)?
                .label
                .clone();
            Some(MarkerNote {
                label,
                t_ns: first.t_ns,
                placed: a.markers.iter().map(|m| (m.canvas, m.index)).collect(),
            })
        });
        Update::Markers {
            counts: [manual(OutputCanvas::Main), manual(OutputCanvas::Vertical)],
            placed,
        }
    }

    /// `project.export` (ADR-0089 decisions 8 and 11): an editor project from a
    /// finished recording in the profile's recordings folder. `recording` is one path
    /// segment: a recording folder's name, a plain recording's file name or its
    /// manifest's name. Runs on the control thread; writing takes milliseconds.
    fn export_project(
        &self,
        recording: &str,
        format: api::ProjectFormat,
    ) -> Result<rbs_api::request::ProjectExported, ApiError> {
        if !rbs_manifest::single_file_name(recording) {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "recording is one name inside the recordings folder",
            ));
        }
        let folder = &self.profile.folder;
        let named = folder.join(recording);
        let (dir, manifest) = if named.is_dir() {
            (named, rbs_manifest::FOLDER_MANIFEST.to_owned())
        } else if recording.ends_with(rbs_manifest::FILE_SUFFIX) {
            (folder.clone(), recording.to_owned())
        } else {
            let Some(name) = rbs_manifest::manifest_name_for(recording) else {
                return Err(ApiError::new(ErrorKind::NotFound, "no such recording"));
            };
            (folder.clone(), name)
        };
        if !dir.join(&manifest).is_file() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "the recording has no manifest: it was recorded without separate files, \
                 markers or the editor project setting",
            ));
        }
        let format = match format {
            api::ProjectFormat::Fcpxml => rbs_project::Format::Fcpxml,
            api::ProjectFormat::Xmeml => rbs_project::Format::Xmeml,
        };
        match rbs_project::export(&dir, &manifest, format, false) {
            Ok(done) => Ok(rbs_api::request::ProjectExported {
                file: done
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                missing: done.missing,
            }),
            Err(rbs_project::ProjectError::Incomplete) => Err(failed(
                "the recording has no timing for an editor project: it was recorded without \
                 separate files or the editor project setting, or it did not stop cleanly",
            )),
            Err(e) => Err(failed(e.to_string())),
        }
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
    /// What the estimate knows about this computer for a start that records into
    /// `folder` (ADR-0087 decision 1): the adapters' declared capabilities, the
    /// calibration entries that still hold and the folder's cached free space. Never
    /// waits: a folder with no fresh reading is unknown now and read for the next call.
    fn estimate_limits(
        &mut self,
        adapters: Vec<rbs_record::estimate::DeclaredAdapter>,
        folder: &Path,
    ) -> rbs_record::estimate::Limits {
        let this = self.this_computer();
        let mut calibration = self.calibration.for_estimate(&this, folder);
        calibration.exports_unmeasurable = self.calibration_run.exports_unmeasurable;
        let now = self.clock.now();
        let free_space = self
            .disk_probe
            .as_mut()
            .and_then(|p| p.free_space(folder, now).bytes());
        rbs_record::estimate::Limits {
            adapters,
            calibration: Some(calibration),
            free_space,
            previous_tick: None,
        }
    }

    /// This computer's adapters and RBS version, for the calibration file.
    fn this_computer(&self) -> crate::calibration_file::ThisComputer {
        crate::calibration_file::ThisComputer {
            // No adapter reports `adapter.version` yet (ADR-0087 decision 4).
            adapters: self
                .adapters
                .video_encoders
                .ids()
                .map(|id| (id.as_str().to_owned(), None))
                .collect(),
            rbs_version: env!("CARGO_PKG_VERSION").to_owned(),
            // The GPU, its driver and the OS: export measurements hold only on the
            // same (step D2). `uname` is a quick local call.
            gpu: Some(format!(
                "{}|{}",
                self.gpu.adapter_key(),
                rbs_system_probe::os_version().unwrap_or_default()
            )),
        }
    }

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
        self.note_encoders(notify);
        if !self.record_profile(c).format.is_audio_only()
            && let Some(why) = &self.cannot_record
        {
            notify(recording_update(
                c,
                RecordingUpdate::NotStarted(why.clone()),
            ));
            return;
        }
        let update = match self.start_recording(c, r, Some(API_PROFILE_ID.into()), StartGate::Older)
        {
            Ok(update) => update,
            Err(why) => RecordingUpdate::NotStarted(why.into_message()),
        };
        notify(recording_update(c, update));
        self.publish(notify);
    }

    /// The UI's Go live for `c`: starts the stream, says how it went, and records
    /// with it when that is on.
    fn stream_asked(&mut self, c: OutputCanvas, r: &StreamRequest, notify: &dyn Fn(Update)) {
        self.note_encoders(notify);
        if let Some(why) = &self.cannot_record {
            notify(stream_update(c, StreamUpdate::NotStarted(why.clone())));
            return;
        }
        let update = match self.start_stream(c, r) {
            Ok(update) => update,
            Err(why) => StreamUpdate::NotStarted(why),
        };
        let started = matches!(update, StreamUpdate::Started { .. });
        notify(stream_update(c, update));
        if started {
            // Each destination that did not start, by name (ADR-0127 decision 7).
            for d in &self.outs[slot(c)].destinations {
                if let (DestinationState::Failed, Some(why)) = (d.state, &d.error) {
                    notify(Update::Error(format!("{}: {why}", d.label())));
                }
            }
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
        o.hold.paused_recording = false;
        let protected = o.protection_stop.take();
        // Tracked until the end: when a stop panics half-way, `Host::recover` finds the
        // recording still tracked and ends it again (ADR-0055 decision 3).
        let Some((path, id)) = o.recording.clone() else {
            return;
        };
        let report = self.studio.stop_output(id);
        // The separate files end with their master (ADR-0086 decision 5).
        let mut separate = Vec::new();
        if c == OutputCanvas::Main
            && let Some(files) = self.iso.take()
        {
            self.studio.stop_iso_video();
            self.studio.stop_iso_audio();
            separate = self.studio.take_iso_media();
            if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
                markers.set_images(&files.images);
            }
        }
        let now = self.clock.now().as_nanos();
        if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
            markers.stopped_ns = now.saturating_sub(markers.log.t0_ns());
            markers.set_ended(report.as_ref().and_then(|r| r.media), separate);
        }
        inject!(self, Fault::InRecordingStop);
        let (frames, dropped, flush_lost, failed) = report.map_or((0, 0, 0, None), |r| {
            (
                r.progress.frames_encoded,
                r.progress.export_drops + r.progress.encoder_skipped + r.progress.encoder_drops,
                r.flush_lost,
                r.failure,
            )
        });
        let mut failure = failure.or(failed).or(protected);
        // The markers go into the manifest once the media is closed (ADR-0089
        // decision 5); a failure there does not touch the media.
        if let Some(mut markers) = self.outs[slot(c)].markers.take() {
            let why = match markers.write() {
                Err(e) => Some(format!("The markers could not be saved: {e}.")),
                // The project only after the manifest is on disk (ADR-0089 decision 10).
                Ok(()) => {
                    let project = markers
                        .write_project()
                        .err()
                        .map(|e| format!("The editor project was not written: {e}."));
                    // Inside the media only after a clean stop (ADR-0089 decision 14).
                    let chapters = if failure.is_none() {
                        markers.write_chapters().err().map(|e| {
                            format!("The markers could not be added to the file as chapters: {e}.")
                        })
                    } else {
                        None
                    };
                    match (project, chapters) {
                        (Some(a), Some(b)) => Some(format!("{a} {b}")),
                        (a, b) => a.or(b),
                    }
                }
            };
            if let Some(why) = why {
                failure = Some(match failure {
                    Some(f) => format!("{f} {why}"),
                    None => why,
                });
            }
        }
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
        // A vertical recording with no section of its own keeps the main encoding
        // and encodes at the vertical canvas: the main size is not copied.
        let size = match c {
            OutputCanvas::Main => p.output_size.map(|s| s.pair()),
            OutputCanvas::Vertical => self
                .vertical_profile
                .as_ref()
                .and_then(|v| v.output_size)
                .map(|s| s.pair()),
        };
        let start = p.recording_start(encoder, size);
        Ok(RecordRequest {
            path,
            container: start.container,
            encoder: start.encoder,
            video: start.video,
            audio_bitrate_bps: start.audio_bitrate_bps,
            bit_depth: start.bit_depth,
            size: start.size,
            iso: self.separate_files,
            editor_project: p.editor_project,
            markers: p.markers,
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
        let started = if !self.record_profile(c).format.is_audio_only()
            && let Some(why) = &self.cannot_record
        {
            Err(why.clone())
        } else {
            self.profile_request(c, self.recording_file(c))
                .and_then(|r| {
                    self.start_recording(c, &r, Some(API_PROFILE_ID.into()), StartGate::Older)
                        .map_err(StartError::into_message)
                })
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

    /// Goes live: reads each destination's key (RTMP: required) from the key store,
    /// then attaches the stream's encoders and one output per enabled destination to
    /// the running studio (ADR-0127): destinations sharing the stream's encoding in
    /// one apply, so they start on the same keyframe, then each with its own
    /// encoding. It goes live when at least one destination starts; the others are
    /// kept as failed with their reason. The connections are made in the background;
    /// [`Host::report_link`] tells the UI how they go.
    fn start_stream(&mut self, c: OutputCanvas, r: &StreamRequest) -> Result<StreamUpdate, String> {
        self.start_stream_to(c, r, None)
    }

    /// [`Host::start_stream`] to destination `only` alone when it is `Some`
    /// (`stream.destination.start` while nothing streams, ADR-0127 decision 16).
    fn start_stream_to(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
        only: Option<u32>,
    ) -> Result<StreamUpdate, String> {
        if let Some(why) = self.calibration_blocks() {
            return Err(format!("{why}."));
        }
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
        let (plans, mut failed) = self.destination_plans(c, r, only);
        if plans.is_empty() {
            return Err(failed
                .first()
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| {
                    "No destination is turned on: turn one on in Settings, Stream.".into()
                }));
        }
        let alone = plans.len() == 1 && failed.is_empty();
        let mut ready: Vec<(Planned, OutputSettings)> = Vec::new();
        for p in plans {
            let key = match self.keys.read(&p.account) {
                Ok(key) => key,
                Err(e) => {
                    let which = match (p.live.id, c) {
                        (_, _) if !p.first => format!(" of {}", p.live.label()),
                        (_, OutputCanvas::Main) => String::new(),
                        (_, OutputCanvas::Vertical) => " of the vertical stream".into(),
                    };
                    let why = format!(
                        "The stream key{which} could not be read from {}: {e}",
                        self.keys.place()
                    );
                    if alone {
                        return Err(why);
                    }
                    failed.push(p.failed(why));
                    continue;
                }
            };
            if p.request.needs_key && key.is_none() {
                if alone {
                    return Ok(StreamUpdate::NoKey);
                }
                failed.push(p.failed("No stream key is saved for it.".into()));
                continue;
            }
            let out = stream_output(&p.request, key, canvas, &self.adapters);
            ready.push((p, out));
        }
        // Destinations with the stream's encoding start together; each other one
        // after, on its own encoder.
        let (shared, own): (Vec<_>, Vec<_>) = ready.into_iter().partition(|(p, _)| !p.live.own);
        let mut started: Vec<LiveDestination> = Vec::new();
        if !shared.is_empty() {
            let outs: Vec<OutputSettings> = shared.iter().map(|(_, o)| o.clone()).collect();
            let results = self.start_outputs(&outs, r.encoder.as_deref());
            for ((p, _), result) in shared.into_iter().zip(results) {
                match result {
                    Ok(id) => started.push(p.started(id)),
                    Err(why) if alone => return Err(why),
                    Err(why) => failed.push(p.failed(why)),
                }
            }
        }
        for (p, out) in own {
            match self.start_output(&out, p.request.encoder.as_deref()) {
                Ok((id, _, _)) => started.push(p.started(id)),
                Err(why) if alone => return Err(why),
                Err(why) => failed.push(p.failed(why)),
            }
        }
        let Some(anchor) = started.first().cloned() else {
            return Err(failed
                .first()
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| "No destination started.".into()));
        };
        let id = anchor.output.ok_or("the stream did not start")?;
        let (video_encoder, audio_encoder) = match self.studio.output(id) {
            Some(report) => (
                report
                    .video_encoder
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                report.audio_encoder.as_ref().map(|c| c.name.clone()),
            ),
            None => (String::new(), None),
        };
        // Before the host tracks it: recovery stops an output a panicked start left
        // running (ADR-0055 decision 3).
        inject!(self, Fault::AfterStreamStart);
        self.outs[slot(c)].streaming = Some((anchor.url.clone(), Link::default(), id));
        self.outs[slot(c)].destinations = started.into_iter().chain(failed).collect();
        let now = self.clock.now();
        let destinations = self.outs[slot(c)]
            .destinations
            .iter()
            .map(|d| d.api(now))
            .collect();
        let protocol = match parse_server(&anchor.url).map(|s| s.protocol) {
            Ok(Protocol::Srt) => api::StreamProtocol::Srt,
            // Only RTMP and SRT servers pass the settings' check.
            _ => api::StreamProtocol::Rtmp,
        };
        self.set_stream_state(
            c,
            api::StreamState::Live {
                // `set_stream_state` sets it from the destinations.
                available: true,
                protocol,
                connected: false,
                connects: 0,
                reconnect_attempts: 0,
                video_encoder: video_encoder.clone(),
                audio_encoder: audio_encoder.clone(),
                held: false,
                held_ms: 0,
                hold_picture: None,
                hold_scene: None,
                hold_sound: None,
                hold_fallback: false,
                hold_sound_fallback: false,
                hold_fallback_reason: None,
                destinations,
            },
        );
        self.mark_stream(c, true);
        Ok(StreamUpdate::Started {
            url: anchor.url,
            video_encoder,
            audio_encoder,
        })
    }

    /// The destinations Go live of `c` sends to, with their requests: the first one
    /// with `r` (what the UI or the API built from the stream settings), every other
    /// enabled one from the saved destinations. Those enabled but not configured come
    /// back failed with the reason.
    fn destination_plans(
        &self,
        c: OutputCanvas,
        r: &StreamRequest,
        only: Option<u32>,
    ) -> (Vec<Planned>, Vec<LiveDestination>) {
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let mut profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        // A request that names a server is the first destination's (the UI or the API
        // built it from the stream settings), also before those settings reached here.
        if profile.server.is_empty() && !r.url.is_empty() {
            profile.server = r.url.clone();
        }
        let mut plans = Vec::new();
        let mut skipped = Vec::new();
        for d in self.destinations.of(role, &profile, &self.key_account) {
            if !d.destination.enabled || only.is_some_and(|id| id != d.destination.id) {
                continue;
            }
            let first = d.destination.is_first();
            let base = if first {
                r.clone()
            } else {
                let Some(server) = d.parsed() else {
                    skipped.push(
                        Planned::of(&d, r.clone(), first)
                            .failed("Not set up: enter its server in Settings, Stream.".into()),
                    );
                    continue;
                };
                StreamRequest {
                    url: d.destination.server.clone(),
                    input: server.protocol.input(),
                    needs_key: server.protocol.needs_key(),
                    reconnect: d.reconnect_policy(),
                    transport_options: d.transport_options(),
                    ..r.clone()
                }
            };
            let request = match &d.destination.encoding {
                DestinationEncoding::Shared => base,
                DestinationEncoding::Own {
                    encoding,
                    output_size,
                } => StreamRequest {
                    encoder: encoding.encoder.clone(),
                    video: encoding.video_settings(),
                    audio_bitrate_bps: encoding.audio_bps(),
                    size: output_size.map(|s| s.pair()),
                    ..base
                },
            };
            plans.push(Planned::of(&d, request, first));
        }
        (plans, skipped)
    }

    /// The request `stream.start` of `c` sends with (from the saved stream settings),
    /// and the server URLs no answer may repeat. With `only` naming a destination
    /// other than the first, the first one's server may be empty: the request then
    /// carries that destination's server, and only the stream's encoding is used.
    fn api_stream_request(
        &self,
        c: OutputCanvas,
        only: Option<u32>,
    ) -> Result<(StreamRequest, Vec<String>), ApiError> {
        // The vertical stream's texts name it, so a client sees which one is missing.
        let which = match c {
            OutputCanvas::Main => "",
            OutputCanvas::Vertical => " for the vertical stream",
        };
        if self.stream_profile.is_none() {
            return Err(failed("the studio is still starting; try again"));
        }
        let no_server = || {
            failed(format!(
                "no server is saved{which}: enter it in Settings, Stream"
            ))
        };
        let other = only.filter(|id| *id != MAIN_FIRST && *id != VERTICAL_FIRST);
        let mut profile = match (self.stream_profile_of(c), other) {
            (Some(p), _) => p.clone(),
            (None, Some(_)) => self.stream_profile.clone().unwrap_or_default(),
            (None, None) => return Err(no_server()),
        };
        // Every destination off: refused before any key is read (ADR-0148 decision 10).
        if only.is_none() && !self.stream_available(c) {
            return Err(failed(STREAMING_OFF));
        }
        // The first slot removed: the other destinations go live on the stream's
        // shared settings (ADR-0148 decision 6), or streaming is off.
        if other.is_none() && !crate::stream_destinations::first_exists(&profile) {
            if !self.stream_available(c) {
                return Err(failed(STREAMING_OFF));
            }
            let encoder = profile
                .encoding
                .encoder
                .clone()
                .filter(|id| self.encoders.iter().any(|e| e.id == *id));
            let request = StreamRequest::from_settings(&profile, encoder).map_err(|_| {
                failed(format!(
                    "the saved stream settings{which} cannot be used: see Settings, Stream"
                ))
            })?;
            return Ok((request, Vec::new()));
        }
        // The saved servers are settings (`admin`): a `control` caller never gets one
        // back, also not inside an error text.
        let mut hidden: Vec<String> = Vec::new();
        if !profile.server.is_empty() {
            hidden.push(profile.server.clone());
        }
        if let Some(id) = other {
            let server = self
                .resolved_destinations(c)
                .into_iter()
                .find(|d| d.destination.id == id)
                .map(|d| d.destination.server)
                .unwrap_or_default();
            if !server.is_empty() {
                hidden.push(server.clone());
            }
            if profile.server.is_empty() {
                profile.server = server;
            }
        }
        if profile.server.is_empty() {
            return Err(no_server());
        }
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = profile
            .encoding
            .encoder
            .clone()
            .filter(|id| self.encoders.iter().any(|e| e.id == *id));
        let request = StreamRequest::from_profile(&profile, encoder).map_err(|_| {
            failed(format!(
                "the saved stream settings{which} cannot be used: see Settings, Stream"
            ))
        })?;
        Ok((request, hidden))
    }

    /// The canvas destination `id` belongs to; `None` for an id the profile does not
    /// have.
    fn destination_canvas(&self, id: u32) -> Option<OutputCanvas> {
        let canvas = match id {
            MAIN_FIRST => DestinationCanvas::Main,
            VERTICAL_FIRST => DestinationCanvas::Vertical,
            _ => self.destinations.get(id)?.canvas,
        };
        let c = match canvas {
            DestinationCanvas::Main => OutputCanvas::Main,
            DestinationCanvas::Vertical => OutputCanvas::Vertical,
        };
        // A first slot that was removed is no destination (ADR-0148 decisions 2, 10).
        if (id == MAIN_FIRST || id == VERTICAL_FIRST)
            && !self
                .stream_profile_of(c)
                .is_some_and(crate::stream_destinations::first_exists)
        {
            return None;
        }
        Some(c)
    }

    /// The stream settings or destinations changed: each canvas's stream state says
    /// again whether streaming is available, and publishes when that changed (control
    /// API 1.93, ADR-0148 decision 10). A removed vertical canvas publishes nothing.
    fn availability_changed(&mut self) {
        for c in self.output_canvases() {
            let state = self.outs[slot(c)].stream_state.clone();
            self.set_stream_state(c, state);
        }
    }

    /// Streaming is available on `c` (ADR-0148 decision 1): one of its existing
    /// destinations is on, or the destinations section did not load.
    fn stream_available(&self, c: OutputCanvas) -> bool {
        if self.destinations_unloaded {
            return true;
        }
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        self.destinations.available(role, &profile)
    }

    /// The destinations of `c` with their first one's settings filled in.
    fn resolved_destinations(&self, c: OutputCanvas) -> Vec<Resolved> {
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        self.destinations.of(role, &profile, &self.key_account)
    }

    /// `stream.destinations` rows of `c` (control API 1.77): never a server or a key.
    fn api_destinations(&self, c: OutputCanvas) -> Vec<api::StreamDestination> {
        self.resolved_destinations(c)
            .into_iter()
            .map(|d| api::StreamDestination {
                id: d.destination.id,
                configured: d.configured(),
                name: d.destination.name,
                service: d.destination.service,
                canvas: c,
                enabled: d.destination.enabled,
                encoding: if d.destination.encoding.is_own() {
                    api::DestinationEncodingKind::Own
                } else {
                    api::DestinationEncodingKind::Shared
                },
            })
            .collect()
    }

    /// Starts destination `id` of `c`'s running stream (`stream.destination.start`,
    /// ADR-0127 decision 16): it joins the shared encoder at its next keyframe, or
    /// opens its own chain. `r` is the request the stream's first destination would
    /// start with.
    fn start_destination(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
        id: u32,
    ) -> Result<(), String> {
        let (plans, failed) = self.destination_plans(c, r, Some(id));
        if let Some(why) = failed.into_iter().find_map(|d| d.error) {
            return Err(why);
        }
        let p = plans
            .into_iter()
            .next()
            .ok_or_else(|| format!("there is no destination {id}"))?;
        let key = self.keys.read(&p.account).map_err(|e| {
            format!(
                "The stream key of {} could not be read from {}: {e}",
                p.live.label(),
                self.keys.place()
            )
        })?;
        if p.request.needs_key && key.is_none() {
            return Err(format!("No stream key is saved for {}.", p.live.label()));
        }
        let canvas = self.output_canvas(c)?;
        let out = stream_output(&p.request, key, canvas, &self.adapters);
        let output = if p.live.own {
            self.start_output(&out, p.request.encoder.as_deref())?.0
        } else {
            self.start_outputs(std::slice::from_ref(&out), r.encoder.as_deref())
                .pop()
                .unwrap_or_else(|| Err("the destination did not start".into()))?
        };
        let live = p.started(output);
        let dests = &mut self.outs[slot(c)].destinations;
        match dests.iter_mut().find(|d| d.id == id) {
            Some(d) => *d = live,
            None => dests.push(live),
        }
        Ok(())
    }

    /// Stops destination `id` of `c`'s running stream while others run on
    /// (`stream.destination.stop`): its output drains and its transport finishes; the
    /// shared encoder runs on. When it was the anchor, the next running destination
    /// becomes the anchor. False when it does not run.
    fn stop_destination(&mut self, c: OutputCanvas, id: u32) -> bool {
        let Some(anchor) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return false;
        };
        let Some(d) = self.outs[slot(c)]
            .destinations
            .iter_mut()
            .find(|d| d.id == id && d.output.is_some())
        else {
            return false;
        };
        let Some(output) = d.output.take() else {
            return false;
        };
        d.state = DestinationState::Stopped;
        d.link.connected = false;
        let _ = self.studio.stop_output(output);
        if output == anchor
            && let Some(next) = self.outs[slot(c)]
                .destinations
                .iter()
                .find(|d| d.output.is_some())
                .cloned()
            && let Some(next_id) = next.output
        {
            self.outs[slot(c)].streaming = Some((next.url, Link::default(), next_id));
            self.refresh_stream_hold(c);
        }
        true
    }

    /// Attaches `out` to the studio with the picked encoder only, or the best one (a
    /// running one when the settings match, ADR-0047); returns the output and the
    /// encoders' names.
    /// Probes video and audio encoders once. Returns whether this call did the probe.
    fn ensure_encoders(&mut self) -> bool {
        if self.encoders_probed {
            return false;
        }
        self.adapters.probe_encoders();
        let (encoders, cannot_record) = encoder_catalog(&self.adapters);
        self.encoders = encoders;
        self.cannot_record = cannot_record;
        self.encoders_probed = true;
        true
    }

    fn publish_encoders(&self, notify: &dyn Fn(Update)) {
        notify(Update::Encoders {
            encoders: self.encoders.clone(),
            cannot_record: self.cannot_record.clone(),
        });
    }

    /// Probes when needed and tells the UI. A later call publishes nothing.
    fn note_encoders(&mut self, notify: &dyn Fn(Update)) {
        if self.ensure_encoders() {
            self.publish_encoders(notify);
        }
    }

    fn start_output(
        &mut self,
        out: &OutputSettings,
        encoder: Option<&str>,
    ) -> Result<(OutputId, String, Option<String>), String> {
        // A path that did not publish still probes before an encoder is chosen.
        self.ensure_encoders();
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
                    report
                        .video_encoder
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    report.audio_encoder.as_ref().map(|c| c.name.clone()),
                ))
            });
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        started
    }

    /// [`Host::start_output`] for several outputs in one apply (ADR-0127 decision 3):
    /// one result per output, in order; one that fails does not stop the others.
    fn start_outputs(
        &mut self,
        outs: &[OutputSettings],
        encoder: Option<&str>,
    ) -> Vec<Result<OutputId, String>> {
        let all = match encoder {
            None => None,
            Some(id) => match only_encoder(&self.adapters.video_encoders, id) {
                Ok(only) => Some(std::mem::replace(&mut self.adapters.video_encoders, only)),
                Err(why) => return outs.iter().map(|_| Err(why.clone())).collect(),
            },
        };
        let results = self
            .studio
            .start_outputs(&self.adapters, outs)
            .into_iter()
            .map(|r| r.map_err(|e| e.to_string()))
            .collect();
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        results
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
        if let Some(why) = self.calibration_blocks() {
            return Err(format!("{why}."));
        }
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

    /// Reads each running destination of `c`'s stream (ADR-0127 decisions 5 and 6):
    /// its connection and state, and its failure. A destination whose output failed is
    /// removed without a drain and named in a notice; the others run on. When the
    /// anchor fails and another destination runs, that one becomes the anchor. Returns
    /// the stream's failure when no destination is left running.
    fn check_destinations(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) -> Option<String> {
        let now = self.clock.now();
        let mut lost: Option<String> = None;
        let anchor = self.outs[slot(c)].streaming.as_ref().map(|s| s.2);
        let mut dests = std::mem::take(&mut self.outs[slot(c)].destinations);
        if dests.is_empty()
            && let Some(id) = anchor
        {
            // A stream the host started before it tracked destinations: one.
            if let Some(why) = self.studio.output_failure(id) {
                return Some(why);
            }
            return None;
        }
        for d in &mut dests {
            let Some(id) = d.output else {
                continue;
            };
            if let Some(why) = self.studio.output_failure(id) {
                let _ = self.studio.abort_output(id);
                d.output = None;
                d.state = DestinationState::Failed;
                d.link.connected = false;
                notify(Update::Error(format!("{}: {why}", d.label())));
                lost = Some(why.clone());
                d.error = Some(why);
                continue;
            }
            if let Some(link) = self.studio.link(id) {
                d.state = if link.connected {
                    DestinationState::Live
                } else if link.connects > 0 {
                    DestinationState::Reconnecting
                } else {
                    DestinationState::Connecting
                };
                if link.connected && d.on_air_since.is_none() {
                    d.on_air_since = Some(now);
                }
                d.link = link;
            }
        }
        let running = dests.iter().find(|d| d.output.is_some()).cloned();
        self.outs[slot(c)].destinations = dests;
        match (running, anchor) {
            (None, _) => Some(lost.unwrap_or_else(|| "every destination failed".into())),
            (Some(next), Some(id))
                if next.output != Some(id)
                    && !self.outs[slot(c)]
                        .destinations
                        .iter()
                        .any(|d| d.output == Some(id)) =>
            {
                // The anchor failed: the stream goes on with the next destination.
                let next_id = next.output?;
                self.outs[slot(c)].streaming = Some((next.url, Link::default(), next_id));
                self.refresh_stream_hold(c);
                None
            }
            _ => None,
        }
    }

    /// Tells the UI when the stream's connection changed.
    fn report_link(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        if self.outs[slot(c)].streaming.is_none() {
            return;
        }
        // Only what the UI shows: a change of transport drops alone sends nothing.
        let shown = |l: &Link| (l.connected, l.connects, l.reconnect_attempts);
        // The stream's link sums its running destinations' (ADR-0127 decision 16):
        // connected while one is.
        let mut sum: Option<Link> = None;
        for d in &self.outs[slot(c)].destinations {
            if d.output.is_some() {
                let s = sum.get_or_insert_with(Link::default);
                s.connected |= d.link.connected;
                s.connects += d.link.connects;
                s.reconnect_attempts += d.link.reconnect_attempts;
                s.transport_drops += d.link.transport_drops;
            }
        }
        // Each destination's row (control API 1.77): a change of state, counters or
        // error is published; the time on air alone is not (never per round).
        let now = self.clock.now();
        let rows: Vec<api::LiveDestination> = self.outs[slot(c)]
            .destinations
            .iter()
            .map(|d| d.api(now))
            .collect();
        let without_time = |rows: &[api::LiveDestination]| -> Vec<api::LiveDestination> {
            rows.iter()
                .cloned()
                .map(|r| api::LiveDestination { on_air_ms: 0, ..r })
                .collect()
        };
        let rows_shown = without_time(&rows);
        if rows_shown != self.outs[slot(c)].destinations_shown {
            self.outs[slot(c)].destinations_shown = rows_shown;
            notify(Update::LiveDestinations(c, rows.clone()));
        }
        let rows_changed = match &self.outs[slot(c)].stream_state {
            api::StreamState::Live { destinations, .. } => {
                without_time(destinations) != without_time(&rows)
            }
            api::StreamState::Idle { .. } => false,
        };
        let Some((_, last, _)) = &mut self.outs[slot(c)].streaming else {
            return;
        };
        let link = sum.filter(|l| shown(l) != shown(last));
        if let Some(link) = link {
            *last = link;
            notify(stream_update(c, StreamUpdate::Link(link)));
        }
        if link.is_none() && !rows_changed {
            return;
        }
        let mut state = self.outs[slot(c)].stream_state.clone();
        if let api::StreamState::Live {
            connected,
            connects,
            reconnect_attempts,
            destinations,
            ..
        } = &mut state
        {
            if let Some(link) = link {
                *connected = link.connected;
                *connects = link.connects;
                *reconnect_attempts = link.reconnect_attempts;
            }
            *destinations = rows;
            self.set_stream_state(c, state);
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
        // Every destination in one draining apply (ADR-0127 decision 7); the anchor's
        // report is the stream's.
        let ids: Vec<OutputId> = std::iter::once(id)
            .chain(
                self.outs[slot(c)]
                    .destinations
                    .iter()
                    .filter_map(|d| d.output)
                    .filter(|o| *o != id),
            )
            .collect();
        let report = self
            .studio
            .stop_outputs(&ids)
            .into_iter()
            .find(|(o, _)| *o == id)
            .map(|(_, r)| r);
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
        self.outs[slot(c)].destinations.clear();
        if !self.outs[slot(c)].destinations_shown.is_empty() {
            self.outs[slot(c)].destinations_shown.clear();
            notify(Update::LiveDestinations(c, Vec::new()));
        }
        self.outs[slot(c)].hold = HoldPlan::default();
        self.mark_stream(c, false);
        // A recording started with the stream outlives a stream that failed: it is the
        // copy of what the viewers missed. From now on it is the user's to stop.
        self.outs[slot(c)].auto_recording = false;
        self.set_stream_state(
            c,
            api::StreamState::Idle {
                // `set_stream_state` sets it from the destinations.
                available: true,
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
        if changed && let Some(dir) = &self.config_dir {
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
            && let Some(dir) = &self.config_dir
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
                // The option defaults Reset source uses too (ADR-0133 decision 2).
                settings.extend(self.option_defaults(&kind.type_id()));
                match target.size {
                    Some((sw, sh)) => Layout::fit(w, h, sw, sh),
                    None => Layout::new([0.0, 0.0], [w as f32, h as f32]),
                }
            }
            SourceKind::Camera => {
                settings.extend(self.option_defaults(&kind.type_id()));
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

    /// Remove from scene (ADR-0065 decision 7, ADR-0142): the item goes, with the
    /// companion items its scene no longer needs; the source too when it was its last
    /// placement. The rule is the model's (`SceneCollection::remove_from_scene`); the
    /// history entry carries the source's name, Delete when it was deleted, else
    /// Remove. Returns the name and what was removed.
    fn remove_from_scene(&mut self, item: ItemId) -> Result<(String, Removed), Refusal> {
        let gone = || Refusal::NotFound(format!("There is no item {}.", item.get()));
        let (_, it) = self.collection.item(item).ok_or_else(gone)?;
        let ItemContent::Source(source) = it.content else {
            return Err(Refusal::Invalid(
                "Only a source can be removed from a scene here.".to_owned(),
            ));
        };
        let name = self.name_of(source);
        let (removed, undo) = self
            .collection
            .remove_from_scene(item)
            .map_err(|e| Refusal::of(&e))?;
        let kind = if removed.source_deleted {
            EditKind::Delete
        } else {
            EditKind::Remove
        };
        self.history.record(kind, &name, 0, vec![undo]);
        Ok((name, removed))
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
    /// edited scene with its layout (answers the new item), else it is removed from the
    /// global scene as Remove from scene does (answers what was removed). One undo step.
    fn unmake_global(
        &mut self,
        id: SourceId,
        keep: bool,
    ) -> Result<Result<ItemId, Removed>, Refusal> {
        let name = self.name_of(id);
        let Some(item) = self.global_item(id) else {
            return Err(if self.collection.source(id).is_none() {
                Refusal::NotFound(format!("There is no source {}.", id.get()))
            } else {
                Refusal::Invalid(format!("{name} is not a global source."))
            });
        };
        if !keep {
            let (_, removed) = self.remove_from_scene(item)?;
            return Ok(Err(removed));
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
                Ok(Ok(new))
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
        self.add_global_with(&target.name, kind, settings, layout)
    }

    /// [`Host::add_global`] with the settings and layout given.
    fn add_global_with(
        &mut self,
        base: &str,
        kind: SourceKind,
        settings: Settings,
        layout: Layout,
    ) -> Result<SourceId, Refusal> {
        let name = unique_name(&self.collection, base);
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
                self.fill_device_offset(id, None, &name, EditKind::Add);
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
                self.fill_device_offset(id, None, name, EditKind::Add);
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
                self.fill_device_offset(id, None, &name, EditKind::Add);
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
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".into()))?;
        let (w, h) = attributes::browser_size(canvas.size.width, canvas.size.height);
        let mut settings = Settings::new();
        // An empty address adds a page that waits for one (Add source › Browser:
        // Properties opens on its address); it starts nothing until then.
        if !url.trim().is_empty() {
            settings.insert(keys::URL.into(), SettingValue::Text(url.trim().to_owned()));
        }
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

    /// A new media source for `path` that no scene places, made the stinger with the
    /// transition set to Stinger (ADR-0140 decisions 1 and 10): one undo step.
    fn set_stinger_file(&mut self, path: &std::path::Path) -> Result<SourceId, Refusal> {
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.into(),
            SettingValue::Text(path.display().to_string()),
        );
        rbs_sources::validate_settings(&SourceKind::Media.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Stinger");
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, SourceKind::Media.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        let mut t = self.collection.transition();
        t.kind = rbs_scene::TransitionKind::Stinger;
        t.stinger = rbs_scene::StingerSettings {
            source: Some(source),
            cut_ms: None,
        };
        match self.collection.set_transition(t) {
            Ok(undo_transition) => {
                self.history.record(
                    EditKind::Transition,
                    &name,
                    0,
                    vec![undo_source, undo_transition],
                );
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Settings and layout of a new text source showing `text` (ADR-0129 decision 4).
    /// The box is the default 640 x 160, centred, so the add does not wait on the
    /// renderer. [`text_measure::begin`] patches the size when the measure arrives
    /// and the text and the box are still this default.
    fn new_text(&self, text: &str) -> Result<(Settings, Layout), Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".into()))?;
        let (cw, ch) = (canvas.size.width, canvas.size.height);
        let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
        let [w, h] = [dw.clamp(1, cw.max(1)), dh.clamp(1, ch.max(1))];
        let mut settings = Settings::new();
        settings.insert(keys::TEXT.into(), SettingValue::Text(text.to_owned()));
        settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(w)));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(h)));
        rbs_sources::validate_settings(&SourceKind::Text.type_id(), &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let layout = Layout::new(
            [(cw as f32 - w as f32) * 0.5, (ch as f32 - h as f32) * 0.5],
            [w as f32, h as f32],
        );
        Ok((settings, layout))
    }

    /// Adds a text source showing `text` to the scene being edited (or as a global
    /// source); one undo step.
    fn add_text(
        &mut self,
        name: Option<&str>,
        text: &str,
        global: bool,
    ) -> Result<SourceId, Refusal> {
        let (settings, layout) = self.new_text(text)?;
        let base = name.filter(|n| !n.trim().is_empty()).unwrap_or("Text");
        let id = if global {
            self.add_global_with(base, SourceKind::Text, settings, layout)
        } else {
            self.add_still(base, SourceKind::Text, settings, layout, None)
        }?;
        text_measure::begin(self, id);
        Ok(id)
    }

    /// Replaces a text source's `text` only (`source.set_text`); one undo step.
    fn set_source_text(&mut self, id: SourceId, text: &str) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Text) {
            return Err(Refusal::Invalid(format!(
                "{} is not a text source.",
                source.name
            )));
        }
        let mut patch = std::collections::BTreeMap::new();
        patch.insert(
            keys::TEXT.to_owned(),
            serde_json::Value::String(text.to_owned()),
        );
        self.patch_source_settings(id, &patch)
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
        self.note_collection();
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
                self.note_collection();
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

    /// The canvas a projector shows and its size, while that canvas runs.
    fn projector_canvas(&self, canvas: OutputCanvas) -> Option<(CanvasId, Size, FrameRate)> {
        let id = match canvas {
            OutputCanvas::Main => self.canvas,
            OutputCanvas::Vertical => self.vertical?.canvas,
        };
        let info = self.collection.canvas(id)?;
        Some((id, info.size, info.frame_rate))
    }

    /// Opens projector `id` (ADR-0125): refused beyond the limits of decision 4 with
    /// [`ProjectorState::Closed`]; its picture is added by [`Self::sync_projectors`].
    fn open_projector(&mut self, id: u32, canvas: OutputCanvas, notify: &dyn Fn(Update)) {
        self.close_projector(id);
        if let Some((_, size, _)) = self.projector_canvas(canvas) {
            let (count, open) = self.projectors_open();
            if let Some(why) = projector_refusal(count, open, size) {
                notify(Update::Projector {
                    id,
                    picture: None,
                    state: ProjectorState::Closed(why.to_owned()),
                });
                return;
            }
        }
        self.projectors.push(ProjectorRun {
            id,
            canvas,
            picture: None,
            size: Size::new(0, 0),
            sent: None,
        });
    }

    /// Closes projector `id`: its picture leaves the studio.
    fn close_projector(&mut self, id: u32) {
        let Some(at) = self.projectors.iter().position(|p| p.id == id) else {
            return;
        };
        let p = self.projectors.remove(at);
        if let Some(picture) = p.picture {
            let run = self.run_collection();
            let running = run.as_ref().unwrap_or(&self.collection);
            // A picture that did not leave cleanly goes with the next update.
            let _ = self
                .studio
                .remove_picture(running, &self.adapters.sources, picture);
            self.waiting = self.studio.waiting();
        }
    }

    /// How many projectors have a picture, and the surface memory they count.
    fn projectors_open(&self) -> (usize, u64) {
        self.projectors
            .iter()
            .filter(|p| p.picture.is_some())
            .fold((0, 0), |(n, b), p| {
                (n + 1, b + projector_surface_bytes(p.size))
            })
    }

    /// Brings the projectors' pictures in line with the studio (ADR-0125 decision 3):
    /// a picture that failed is removed and its projector closed; a projector whose
    /// canvas runs and that has no picture gets one (after a restart, or when the
    /// vertical canvas comes back); one whose canvas is gone is suspended. Tells the UI
    /// what changed. Nothing runs while there is no projector (P13).
    fn sync_projectors(&mut self, notify: &dyn Fn(Update)) {
        if self.projectors.is_empty() {
            return;
        }
        let present = self.studio.pictures();
        let mut closed = Vec::new();
        for at in 0..self.projectors.len() {
            let (canvas, picture) = (self.projectors[at].canvas, self.projectors[at].picture);
            // Gone with its canvas (the studio removed it).
            if let Some(pid) = picture
                && !present.contains(&pid)
            {
                self.projectors[at].picture = None;
            }
            if let Some(pid) = self.projectors[at].picture
                && let Some(why) = self.studio.picture_failure(pid).map(str::to_owned)
            {
                closed.push((
                    self.projectors[at].id,
                    format!("The projector stopped: {why}"),
                ));
                continue;
            }
            if self.projectors[at].picture.is_some() {
                continue;
            }
            let Some((canvas_id, size, fps)) = self.projector_canvas(canvas) else {
                continue;
            };
            let (count, open) = self.projectors_open();
            if let Some(why) = projector_refusal(count, open, size) {
                closed.push((self.projectors[at].id, why.to_owned()));
                continue;
            }
            let desc = PreviewDesc {
                width: size.width,
                height: size.height,
                surfaces: preview_surfaces(fps),
            };
            let run = self.run_collection();
            let running = run.as_ref().unwrap_or(&self.collection);
            match self
                .studio
                .add_picture(running, &self.adapters.sources, canvas_id, desc)
            {
                Ok((pid, _)) => {
                    self.projectors[at].picture = Some(pid);
                    self.projectors[at].size = size;
                }
                Err(e) => closed.push((
                    self.projectors[at].id,
                    format!("The projector did not start: {e}"),
                )),
            }
            self.waiting = self.studio.waiting();
        }
        for (id, why) in closed {
            self.close_projector(id);
            notify(Update::Projector {
                id,
                picture: None,
                state: ProjectorState::Closed(why.clone()),
            });
            notify(Update::Error(why));
        }
        for p in &mut self.projectors {
            let now = match p.picture {
                Some(pid) => (self.studio.picture(pid), ProjectorState::Open),
                None => (None, ProjectorState::Suspended),
            };
            if p.sent.as_ref() != Some(&now) {
                p.sent = Some(now.clone());
                notify(Update::Projector {
                    id: p.id,
                    picture: now.0,
                    state: now.1,
                });
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
        self.note_collection();
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
        let old_device = device_sync::device_key(source);
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
        self.fill_device_offset(id, old_device.as_ref(), &name, EditKind::Retarget);
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

    /// Whether a live pause holds `item`'s scroll on the main and the vertical canvas.
    fn scroll_paused_of(&self, item: &SceneItem) -> [bool; 2] {
        let paused = |canvas: Option<CanvasId>| {
            canvas.is_some_and(|c| {
                self.collection
                    .placed_layout(c, item.layout_on(c))
                    .scroll
                    .is_some()
                    && self.studio.scroll_paused(item.id, c)
            })
        };
        [paused(Some(self.canvas)), paused(self.vertical_edits())]
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
            .chain(
                // The deprecated spelling of a film's `on_return`, computed from the
                // policy until the next major protocol (ADR-0152 decision 10).
                (source.type_id.as_str() == types::MEDIA_SOURCE).then(|| {
                    (
                        keys::RESTART_WHEN_SHOWN.to_owned(),
                        serde_json::Value::Bool(restarts_when_shown(source)),
                    )
                }),
            )
            .chain(
                // The deprecated spelling of a page's `run: placed` (ADR-0152 decision
                // 10, ADR-0149 decision 5).
                (SourceKind::of(&source.type_id) == Some(SourceKind::Browser)).then(|| {
                    (
                        keys::KEEP_RUNNING.to_owned(),
                        serde_json::Value::Bool(keeps_running(source)),
                    )
                }),
            )
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
        self.patch_source_settings_in(id, patch, 0)
    }

    /// [`Self::patch_source_settings`] as part of `gesture`: patches that share a
    /// non-zero gesture are one undo step (a dragged field, design system NumberField).
    fn patch_source_settings_in(
        &mut self,
        id: SourceId,
        patch: &BTreeMap<String, serde_json::Value>,
        gesture: u64,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let (name, type_id) = (source.name.clone(), source.type_id.clone());
        let old_device = device_sync::device_key(source);
        let mut settings = source.settings.clone();
        let mut activity = source.activity;
        let browser = SourceKind::of(&type_id) == Some(SourceKind::Browser);
        // A kept profile's id is RBS's to set, never a client's (ADR-0132 decision 10).
        if browser && patch.contains_key(keys::PROFILE_ID) {
            return Err(Refusal::Invalid(
                "\"profile_id\" is set by RBS when \"keep_signed_in\" turns on.".to_owned(),
            ));
        }
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
        // A film's `restart_when_shown` is the deprecated spelling of its activity
        // `on_return` (ADR-0152 decision 10): written to the policy, never stored.
        if type_id.as_str() == types::MEDIA_SOURCE {
            match settings.remove(keys::RESTART_WHEN_SHOWN) {
                Some(SettingValue::Bool(restart)) => {
                    activity.on_return = Some(if restart {
                        rbs_scene::activity::OnReturn::Restart
                    } else {
                        rbs_scene::activity::OnReturn::Resume
                    });
                }
                Some(_) => {
                    return Err(Refusal::Invalid(
                        "\"restart_when_shown\" must be a flag.".to_owned(),
                    ));
                }
                None if patch.get(keys::RESTART_WHEN_SHOWN) == Some(&serde_json::Value::Null) => {
                    activity.on_return = None;
                }
                None => {}
            }
        }
        // A page's `keep_running` is the deprecated spelling of `run: placed`
        // (ADR-0152 decision 10): true sets it; false or null takes it back to the
        // type's default, and leaves any other `run` alone.
        if browser {
            match settings.remove(keys::KEEP_RUNNING) {
                Some(SettingValue::Bool(true)) => {
                    activity.run = Some(rbs_scene::activity::Run::Placed);
                }
                Some(SettingValue::Bool(false)) => {
                    if activity.run == Some(rbs_scene::activity::Run::Placed) {
                        activity.run = None;
                    }
                }
                Some(_) => {
                    return Err(Refusal::Invalid(
                        "\"keep_running\" must be a flag.".to_owned(),
                    ));
                }
                None if patch.get(keys::KEEP_RUNNING) == Some(&serde_json::Value::Null)
                    && activity.run == Some(rbs_scene::activity::Run::Placed) =>
                {
                    activity.run = None;
                }
                None => {}
            }
        }
        if browser
            && settings.get(keys::KEEP_SIGNED_IN) == Some(&SettingValue::Bool(true))
            && !settings.contains_key(keys::PROFILE_ID)
        {
            settings.insert(
                keys::PROFILE_ID.to_owned(),
                SettingValue::Text(new_profile_id()?),
            );
        }
        rbs_sources::validate_settings(&type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let (settings_changed, activity_changed) =
            self.collection.source(id).map_or((false, false), |s| {
                (s.settings != settings, s.activity != activity)
            });
        if !settings_changed && !activity_changed {
            return Ok(false);
        }
        // A separate picture track keeps its size for the whole take (ADR-0129
        // decision 4): a text's box does not change while one is written.
        if rbs_sources::types::class_of(&type_id) == Some(rbs_sources::types::SourceClass::Text)
            && self.studio.iso_video_records(id)
            && self.collection.source(id).is_some_and(|s| {
                [
                    rbs_sources::types::keys::WIDTH,
                    rbs_sources::types::keys::HEIGHT,
                ]
                .iter()
                .any(|k| s.settings.get(*k) != settings.get(*k))
            })
        {
            return Err(Refusal::Invalid(format!("{name}: {STOP_TO_CHANGE_BOX}")));
        }
        let mut undos = Vec::new();
        if activity_changed {
            undos.push(
                self.collection
                    .set_source_activity(id, activity)
                    .map_err(|e| Refusal::of(&e))?,
            );
        }
        if !settings_changed {
            self.history
                .record(EditKind::SourceSettings, &name, gesture, undos);
            return Ok(true);
        }
        undos.push(
            self.collection
                .set_source_settings(id, settings)
                .map_err(|e| Refusal::of(&e))?,
        );
        // Marks follow stillness (ADR-0129 decision 1): a text that starts to move
        // loses its `video_as: image` in the same undo step.
        if let Some(mark) = self.collection.source(id).and_then(|s| {
            rbs_sources::mark_after_edit(&s.type_id, &s.settings, s.record_separately)
        }) {
            undos.push(
                self.collection
                    .set_source_record_separately(id, mark)
                    .map_err(|e| Refusal::of(&e))?,
            );
        }
        self.history
            .record(EditKind::SourceSettings, &name, gesture, undos);
        self.fill_device_offset(id, old_device.as_ref(), &name, EditKind::SourceSettings);
        Ok(true)
    }

    /// Runs one playback command on a media-file source and reports its state
    /// (protocol 1.74, ADR-0108 decision 10). Live actions: the document does not
    /// change and nothing is an undo step. A source of another type is refused.
    fn media_command(
        &mut self,
        raw: u32,
        op: MediaOp,
    ) -> Result<rbs_api::request::MediaStateInfo, Refusal> {
        use rbs_api::request::{MediaPlayState as Wire, MediaStateInfo};
        use rbs_record::MediaPlayState as State;
        let id = SourceId::from_raw(raw);
        let Some(source) = self.collection.source(id) else {
            return Err(Refusal::NotFound(format!("There is no source {raw}.")));
        };
        if source.type_id.as_str() != types::MEDIA_SOURCE {
            return Err(Refusal::Invalid(format!(
                "Source {raw} is not a media file."
            )));
        }
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let Some(control) = self.studio.media_control(id) else {
            // No instance yet: the source is in no scene the studio runs.
            if matches!(op, MediaOp::State) {
                return Ok(MediaStateInfo {
                    source: raw,
                    state: Wire::Standby,
                    cursor_ms: 0,
                    duration_ms: None,
                });
            }
            return Err(Refusal::Conflict(format!(
                "Media source {raw} is not in a scene that is shown."
            )));
        };
        if !matches!(op, MediaOp::State) && self.studio.stinger_armed(id) {
            // Played by a transition until it ends (ADR-0140 decision 5).
            return Err(Refusal::Conflict(format!(
                "Media source {raw} is playing as the stinger."
            )));
        }
        let failed = |e: rbs_record::MediaNodeError| Refusal::Failed(e.to_string());
        match op {
            MediaOp::Play => control.play().map_err(failed)?,
            MediaOp::Pause => control.pause().map_err(failed)?,
            MediaOp::Seek(ms) => control.seek(Duration::from_millis(ms)).map_err(failed)?,
            MediaOp::Restart => control.restart().map_err(failed)?,
            MediaOp::State => {}
        }
        let state = match control.state() {
            State::Playing => Wire::Playing,
            State::Paused => Wire::Paused,
            State::Cued => Wire::Cued,
            State::Ended => Wire::Ended,
            State::Standby => Wire::Standby,
            State::Unavailable => Wire::Unavailable,
            _ => Wire::Unknown,
        };
        Ok(MediaStateInfo {
            source: raw,
            state,
            cursor_ms: millis(control.cursor()),
            duration_ms: control.duration().map(millis),
        })
    }

    /// Sends the play state of every media source when it changed; while one plays,
    /// asks to be called again in [`MEDIA_TICK`]. Nothing runs without media (P13).
    fn report_media(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        use rbs_record::MediaPlayState as State;
        let mut playing = false;
        let ticks: Vec<MediaTick> = self
            .studio
            .media_controls()
            .map(|(id, control)| {
                let state = control.state();
                playing |= state == State::Playing;
                let step = MEDIA_TICK.as_millis() as u64;
                let cursor = u64::try_from(control.cursor().as_millis()).unwrap_or(u64::MAX);
                MediaTick {
                    id,
                    state: match state {
                        State::Playing => 0,
                        State::Paused => 1,
                        State::Cued => 2,
                        State::Ended => 3,
                        State::Unavailable => 5,
                        _ => 4,
                    },
                    cursor_ms: u32::try_from(cursor / step * step).unwrap_or(u32::MAX),
                    duration_ms: control
                        .duration()
                        .map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
                }
            })
            .collect();
        if self.media_sent.as_ref() != Some(&ticks) {
            self.media_sent = Some(ticks.clone());
            notify(Update::Media(ticks));
        }
        playing.then_some(MEDIA_TICK)
    }

    /// `browser.reload` and `browser.run_script`: a message to the source's running
    /// page. No running page is `conflict` with reason `browser_not_running`; an
    /// agent cannot wake a hidden page (ADR-0132 decision 5, ADR-0149 decision 7).
    fn browser_message(
        &mut self,
        source: u32,
        message: &rbs_sources::BrowserMessage,
    ) -> Result<(), ApiError> {
        if self
            .studio
            .browser_control(SourceId::from_raw(source), message)
        {
            return Ok(());
        }
        Err(self.browser_refusal(source))
    }

    /// Why a message to `source`'s page was not sent: `not_found` when it is not a
    /// browser source, else `conflict` with reason `browser_not_running`.
    fn browser_refusal(&self, source: u32) -> ApiError {
        let browser = self
            .collection
            .source(SourceId::from_raw(source))
            .is_some_and(|s| s.type_id.as_str() == types::BROWSER_SOURCE);
        if !browser {
            return ApiError::new(
                ErrorKind::NotFound,
                format!("there is no browser source {source}"),
            );
        }
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), "browser_not_running".into());
        ApiError::with_data(
            ErrorKind::Conflict,
            "the page is not running: it runs while a scene on Program or Preview shows it",
            data,
        )
    }

    /// `browser.input` (ADR-0132 decisions 6 and 9): the events, in order, to the
    /// source's running page. Checked first, so a bad event sends none of them. No
    /// running page is `conflict` with reason `browser_not_running`. Text is never
    /// logged.
    fn browser_input(
        &mut self,
        source: u32,
        events: &[rbs_api::request::BrowserInputEvent],
    ) -> Result<(), ApiError> {
        if events.len() > rbs_api::request::BrowserInput::MAX_EVENTS {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!(
                    "at most {} events per call",
                    rbs_api::request::BrowserInput::MAX_EVENTS
                ),
            ));
        }
        let wire = events
            .iter()
            .map(browser_input_event)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
        let id = SourceId::from_raw(source);
        if !self.studio.browser_running(id) {
            // Not a browser source (`not_found`) or no running page (`conflict`).
            return Err(self.browser_refusal(source));
        }
        for event in wire {
            if !self
                .studio
                .browser_control(id, &rbs_sources::BrowserMessage::Input(event))
            {
                break;
            }
        }
        Ok(())
    }

    /// Forget sign-in (ADR-0132 decision 10): the source gets a new kept-profile id,
    /// so its page starts again signed out at once, and the old profile's directory is
    /// deleted once the page that used it has closed. One undo step, which brings back
    /// only the id: the profile itself is gone. `stop_first` is the desktop app's
    /// button, which stops the running page itself; a client is refused with `busy`
    /// while the page runs (decision 9). The caller applies the collection.
    fn forget_sign_in(&mut self, id: SourceId, stop_first: bool) -> Result<(), ApiError> {
        let Some(source) = self
            .collection
            .source(id)
            .filter(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no browser source {}", id.get()),
            ));
        };
        let (name, mut settings) = (source.name.clone(), source.settings.clone());
        let Some(old) = crate::browser_profiles::id_of(source).map(str::to_owned) else {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!("{name} keeps no sign-in"),
            ));
        };
        if !stop_first && self.studio.browser_running(id) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("{name} is running: its sign-in is forgotten once the page stops"),
            ));
        }
        let new =
            crate::browser_profiles::new_id().map_err(|e| ApiError::new(ErrorKind::Internal, e))?;
        settings.insert(keys::PROFILE_ID.to_owned(), SettingValue::Text(new));
        let undo = self
            .collection
            .set_source_settings(id, settings)
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        if let Some(dir) = &self.profiles_dir {
            crate::browser_profiles::remove_soon(dir, &old);
        }
        Ok(())
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

    /// `collection.set_transition` (ADR-0128 decision 13): merges the given parts into
    /// the collection's transition as one undo step.
    fn set_transition(
        &mut self,
        p: &rbs_api::request::SetCollectionTransition,
    ) -> Result<bool, Refusal> {
        let mut t = self.collection.transition();
        if let Some(kind) = p.kind {
            t.kind = scene_transition_kind(kind);
        }
        if let Some(kind) = p.direct_kind {
            t.direct = scene_transition_kind(kind);
        }
        if let Some(f) = &p.fade {
            t.fade = rbs_scene::FadeSettings {
                duration_ms: f.duration_ms,
                easing: scene_easing(f.easing),
                via: match &f.via {
                    None => None,
                    Some(c) => Some(parse_rgb(c).ok_or_else(|| {
                        Refusal::Invalid(format!("{c} is not a colour like #1a2b3c."))
                    })?),
                },
            };
        }
        if let Some(s) = p.swipe {
            t.swipe = scene_swipe(s);
        }
        if let Some(s) = p.slide {
            t.slide = scene_swipe(s);
        }
        if let Some(st) = p.stinger {
            t.stinger = self.stinger_settings(st)?;
        }
        self.put_transition(t)
    }

    /// The stinger settings `collection.set_transition` asks for, checked (ADR-0140
    /// decision 9): the source exists and is a media file, and the cut point is within
    /// the file's length when that is known. Placement is the model's check.
    fn stinger_settings(
        &self,
        st: api::StingerTransition,
    ) -> Result<rbs_scene::StingerSettings, Refusal> {
        let source = match st.source {
            None => None,
            Some(raw) => {
                let id = SourceId::from_raw(raw);
                let s = self
                    .collection
                    .source(id)
                    .ok_or_else(|| Refusal::NotFound(format!("There is no source {raw}.")))?;
                if s.type_id.as_str() != types::MEDIA_SOURCE {
                    return Err(Refusal::Invalid(format!(
                        "Source {raw} is not a media file, so it cannot be the stinger."
                    )));
                }
                Some(id)
            }
        };
        if let (Some(cut), Some(id)) = (st.cut_ms, source) {
            let length = (self.collection.transition().stinger.source == Some(id))
                .then(|| self.stinger.duration())
                .flatten();
            if length.is_some_and(|l| u128::from(cut) > l.as_millis()) {
                return Err(Refusal::Invalid(format!(
                    "The cut point {cut} ms is past the end of the stinger's file."
                )));
            }
        }
        Ok(rbs_scene::StingerSettings {
            source,
            cut_ms: st.cut_ms,
        })
    }

    /// `scene.set_transition` (ADR-0135 decision 10) and the Scenes panel's
    /// "Transition into this scene": sets or clears a scene's override as one undo
    /// step; `Ok(false)` when it is already set.
    fn set_scene_transition(
        &mut self,
        id: SceneId,
        t: Option<rbs_scene::SceneTransition>,
    ) -> Result<bool, Refusal> {
        let scene = self
            .collection
            .scene(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?;
        let name = scene.name.clone();
        let t = t.filter(|t| !t.is_empty());
        if scene.transition_in == t {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_transition(id, t)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Transition, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Sets the collection's transition as one undo step; `Ok(false)` when it is
    /// already set.
    fn put_transition(&mut self, t: rbs_scene::CollectionTransition) -> Result<bool, Refusal> {
        if self.collection.transition() == t {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_transition(t)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Transition, "", 0, vec![undo]);
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

    /// Properties' choice of `run` and `on_return` for a source (ADR-0152): the
    /// type's default is stored as absent, so a document stays at the oldest format
    /// that holds it, and the source's linger is kept.
    fn set_source_choice(
        &mut self,
        id: SourceId,
        run: rbs_scene::activity::Run,
        on_return: Option<rbs_scene::activity::OnReturn>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let traits = rbs_sources::activity_traits(&source.type_id);
        let activity = rbs_scene::activity::SourceActivity::new(
            (run != traits.default_run).then_some(run),
            on_return.filter(|r| Some(*r) != traits.default_on_return),
            source.activity.linger_s,
        );
        self.set_source_activity(id, activity)
    }

    /// Replaces a source's activity policy (ADR-0152). One undo step (recorded as a
    /// settings change). A value the source's type does not allow is refused as
    /// invalid; the same policy again changes nothing.
    fn set_source_activity(
        &mut self,
        id: SourceId,
        activity: rbs_scene::activity::SourceActivity,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let traits = rbs_sources::activity_traits(&source.type_id);
        if !traits.allows(&activity) {
            return Err(Refusal::Invalid(format!(
                "Source {} does not allow that activity.",
                id.get()
            )));
        }
        if source.activity == activity {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_activity(id, activity)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
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

    /// Changes which of `id`'s feeds record separately from the next recording on
    /// (ADR-0086 decision 2, ADR-0117 decision 1; protocol 1.46 and 1.52). One undo
    /// step; `Ok(false)` when nothing changed.
    fn set_record_separately(&mut self, id: SourceId, patch: RecordPatch) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let class = types::class_of(&source.type_id);
        let picture_only = matches!(
            class,
            Some(
                types::SourceClass::Capture(_)
                    | types::SourceClass::Still
                    | types::SourceClass::Browser
                    | types::SourceClass::Text
            )
        );
        let sound_only = matches!(class, Some(types::SourceClass::Audio(_)));
        let mut flag = source.record_separately;
        // `enabled` is the main feed: the picture of a picture source, the sound of
        // anything else (a media file records its sound, as in 1.46). Naming that feed
        // twice in one call is refused, so no order between them is needed.
        let aliased = if picture_only {
            patch.video
        } else {
            patch.audio
        };
        if patch.enabled.is_some() && aliased.is_some() {
            return Err(Refusal::Invalid(
                "`enabled` and the feed it stands for cannot both be given.".into(),
            ));
        }
        if let Some(on) = patch.enabled {
            if picture_only {
                flag.video = on;
            } else {
                flag.audio = on;
            }
        }
        if let Some(on) = patch.video {
            if on && sound_only {
                return Err(Refusal::Invalid(format!(
                    "{} has no picture to record separately.",
                    source.name
                )));
            }
            flag.video = on;
        }
        if let Some(on) = patch.audio {
            if on && picture_only {
                return Err(Refusal::Invalid(format!(
                    "{} has no sound of its own to record separately.",
                    source.name
                )));
            }
            flag.audio = on;
        }
        if let Some(form) = patch.video_as {
            if form == VideoAs::Image && !rbs_sources::is_still(&source.type_id, &source.settings) {
                return Err(Refusal::Invalid(format!(
                    "{} is not a still picture, so it cannot be recorded as images.",
                    source.name
                )));
            }
            flag.video_as = form;
        }
        // The form is kept only while the picture is marked (scene model): a form for
        // an unmarked picture would not be saved, so it is refused rather than lost.
        if !flag.video {
            if patch.video_as.is_some_and(|form| form != VideoAs::Auto) {
                return Err(Refusal::Invalid(format!(
                    "{}'s picture is not marked, so it has no form to set.",
                    source.name
                )));
            }
            flag.video_as = VideoAs::Auto;
        }
        if source.record_separately == flag {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_record_separately(id, flag)
            .map_err(|e| Refusal::of(&e))?;
        // A mark the start plan would refuse (more files than it takes) is refused
        // here, so a recording is never refused because of marks set through the
        // app or the API.
        if let Err(e) = rbs_record::plan(&self.collection) {
            let _ = self.collection.undo(undo);
            return Err(Refusal::Invalid(format!("{e}.")));
        }
        self.history
            .record(EditKind::RecordSeparately, &name, 0, vec![undo]);
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
        self.studio.cancel_manual(self.canvas);
        if !self.listed(id) {
            return Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                id.get()
            )));
        }
        // A newer switch replaces one still held back for its texts.
        self.held_switch = None;
        if self.scene == id {
            return Ok(false);
        }
        if self.studio.text_prepare(self.canvas, id) {
            // Goes on air from the control loop once its texts are uploaded.
            self.held_switch = Some(id);
            return Ok(false);
        }
        self.put_on_air(id)
    }

    fn put_on_air(&mut self, id: SceneId) -> Result<bool, Refusal> {
        self.collection
            .set_program(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        self.scene = id;
        Ok(true)
    }

    /// Makes a held-back program switch once its texts are ready or late.
    fn drive_held_switch(&mut self, notify: &dyn Fn(Update)) {
        let Some(id) = self.held_switch else {
            return;
        };
        if self.studio.text_due() == TextDue::Wait {
            return;
        }
        self.held_switch = None;
        // The scene may have been removed meanwhile; the refusal is then news.
        let done = if self.listed(id) && self.scene != id {
            self.put_on_air(id)
        } else {
            Ok(false)
        };
        self.answer(done, notify);
    }

    /// A scene the Scenes panel lists, by name, for the scene levels methods; a global
    /// scene is never listed (ADR-0065).
    fn levels_scene(&self, id: SceneId) -> Result<String, Refusal> {
        self.collection
            .scene(id)
            .filter(|_| self.listed(id))
            .map(|s| s.name.clone())
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))
    }

    /// Turns a scene's Own levels on (ADR-0091 decision 5), copying what every source
    /// it places is heard at now, so nothing jumps. One undo step. `Ok(false)` when it
    /// is in Own levels already.
    fn scene_levels_own(&mut self, id: SceneId) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if self.collection.scene(id).is_some_and(Scene::own_levels) {
            return Ok(false);
        }
        let levels = self.collection.heard_levels(self.canvas, id);
        let undo = self
            .collection
            .set_scene_own_levels(id, levels)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Puts a scene back in Shared levels, discarding its entries (ADR-0091). One undo
    /// step. `Ok(false)` when it is in Shared levels already.
    fn scene_levels_shared(&mut self, id: SceneId) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if !self.collection.scene(id).is_some_and(Scene::own_levels) {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_shared_levels(id)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Removes a scene's entry for one source (ADR-0091): one undo step. `Ok(false)`
    /// when the scene has no entry for it (or is in Shared levels).
    fn scene_level_reset(&mut self, id: SceneId, source: SourceId) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if self.collection.source(source).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                source.get()
            )));
        }
        let has = self
            .collection
            .scene(id)
            .and_then(|s| s.audio.as_ref())
            .is_some_and(|a| a.level(source).is_some());
        if !has {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_level(id, source, None)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        if self.on_air(id) {
            self.studio.set_mix(&self.collection);
        }
        Ok(true)
    }

    /// A scene's own volume or mute for a source (ADR-0091): a live control, not on
    /// the undo stack, sent to the mixer at once while the scene is on air. A scene in
    /// Shared levels is turned to Own levels first, as one undo step. Answers whether
    /// that happened and whether anything changed.
    fn scene_level_set(
        &mut self,
        id: SceneId,
        source: SourceId,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(bool, bool), Refusal> {
        self.levels_scene(id)?;
        if self.collection.source(source).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                source.get()
            )));
        }
        if volume.is_none() && muted.is_none() {
            return Err(Refusal::Invalid(
                "Give a volume, a mute or both.".to_owned(),
            ));
        }
        let level = SceneLevel::new(volume, muted);
        if !level.is_valid() {
            return Err(Refusal::of(&SceneError::InvalidVolume));
        }
        let turned_own = self.scene_levels_own(id)?;
        let now = self
            .collection
            .scene(id)
            .and_then(|s| s.audio.as_ref())
            .and_then(|a| a.level(source).copied());
        let base = now.unwrap_or_default();
        let new = SceneLevel::new(volume.or(base.volume), muted.or(base.muted));
        let changed = now != Some(new);
        if changed {
            // Live: the record is not kept (ADR-0059, ADR-0091 decision 5).
            let _ = self
                .collection
                .set_scene_level(id, source, Some(new))
                .map_err(|e| Refusal::of(&e))?;
        }
        if (changed || turned_own) && self.on_air(id) {
            self.studio.set_mix(&self.collection);
        }
        Ok((turned_own, changed || turned_own))
    }

    /// Whether the window's fader and mute for `source` edit the program scene's level
    /// (ADR-0091 decision 5): that scene keeps its own levels and places the source
    /// (directly, nested, or as a global source of the canvas).
    fn levels_take(&self, source: SourceId) -> bool {
        self.collection
            .scene(self.scene)
            .is_some_and(Scene::own_levels)
            && self
                .collection
                .placed_sources(self.canvas, self.scene)
                .contains(&source)
    }

    /// True while `id` is the studio canvas's program scene, whose levels are heard.
    fn on_air(&self, id: SceneId) -> bool {
        self.scene == id
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
        if !on {
            // Only once the mode has ended, so a refused leave changes nothing
            // (ADR-0151 decision 5).
            self.discard_preview_edits();
            // Leaving studio mode ends a running transition at once (ADR-0128
            // decision 9); the next update binds without it.
            self.studio.end_transitions();
        }
        Ok(true)
    }

    /// Puts the held program scene back as it is on air before studio mode ends
    /// (ADR-0151): edits made in Preview since the hold began never reach the program
    /// without a transition. One undo step, so Undo brings the edits back; nothing
    /// when nothing is held or nothing was edited.
    fn discard_preview_edits(&mut self) {
        let Some(held) = self.held.take() else {
            return;
        };
        if held.from != self.scene {
            return;
        }
        let base = held.scene.id.get();
        let items: Option<Vec<SceneItem>> = held
            .scene
            .items
            .into_iter()
            .map(|mut item| {
                item.id = ItemId::from_raw(item.id.get().checked_sub(base)?);
                Some(item)
            })
            .collect();
        let Some(items) = items else {
            return;
        };
        let Some(scene) = self.collection.scene(held.from) else {
            return;
        };
        if scene.items == items {
            return;
        }
        let name = scene.name.clone();
        // A refusal (the copy no longer fits the collection, which the hold already
        // checks) leaves the scene as it is: the edits stay, as before ADR-0151.
        if let Ok(undo) = self.collection.set_scene_items(held.from, items) {
            self.history
                .record(EditKind::DiscardPreview, &name, 0, vec![undo]);
        }
    }

    /// Makes a scene the preview scene (studio mode only; a live action).
    /// `Ok(false)` when it is the preview scene already.
    fn set_preview_scene(&mut self, id: SceneId) -> Result<bool, Refusal> {
        self.studio.cancel_manual(self.canvas);
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
        // A held T-bar is cancelled first (ADR-0135 decision 7).
        if !self.taking {
            self.studio.cancel_manual(self.canvas);
        }
        let program = self.scene;
        if preview == program {
            // `apply` holds the scene as it is now.
            let taken = self.held.take();
            if let Some(h) = &taken {
                self.held_was = Some(h.from);
                self.held_id_was = Some(h.scene.id);
            }
            return Ok(taken.is_some());
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

    /// Applies a T-bar value (ADR-0135): at 1 the program change happens now, without
    /// a timed transition; when it changes nothing the manual one is cancelled.
    fn drive_progress(&mut self, t: f32, notify: &dyn Fn(Update)) {
        if self.preview().is_none() {
            return;
        }
        self.tbar_zeroed = t <= 0.0;
        match self
            .studio
            .transition_progress(self.canvas, t, &self.adapters.sources)
        {
            Ok(rbs_record::ManualStep::Take) => {
                self.taking = true;
                let changed = self.transition();
                self.taking = false;
                match changed {
                    Ok(true) => self.apply(notify),
                    _ => {
                        self.studio.cancel_manual(self.canvas);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => notify(Update::Error(format!("The T-bar could not move: {e}"))),
        }
    }

    /// Holds the program scene while it is also the preview scene (ADR-0063), and lets
    /// it go otherwise. A copy is taken only when the hold starts, so later edits to
    /// the scene reach the program only through a transition.
    fn sync_held(&mut self) {
        self.held = next_held(
            self.held.take(),
            &self.collection,
            self.canvas,
            self.held_id_was,
        );
    }

    /// The collection the studio runs: the user's own, or with the program held
    /// (ADR-0063) a run collection with the copy on air. A copy that no longer fits
    /// (a duplicate name, say) is taken again from the scene as it is now; if even a
    /// fresh copy does not fit, nothing is held.
    fn run_collection(&mut self) -> Option<SceneCollection> {
        if let Some(h) = &self.held {
            self.held_was = Some(h.from);
            self.held_id_was = Some(h.scene.id);
        }
        self.sync_held();
        let held = self.held.as_ref()?;
        if let Some(run) = run_copy(&self.collection, self.canvas, held) {
            return Some(run);
        }
        // A fresh copy of the same scene, still on air; whatever is previewed, the hold
        // goes on (ADR-0098).
        // The same id as the copy it replaces: the hold goes on, no transition.
        let after = (held.scene.id.get() == HELD_IDS_NEXT).then(|| SceneId::from_raw(HELD_IDS));
        self.held = copy_of(&self.collection, held.from, after);
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
                record_separately: s.record_separately,
            })
            .collect();
        sources.sort_by_key(|s| s.name.to_lowercase());
        let rows = |canvas: Option<CanvasId>| -> Vec<SceneRow> {
            canvas
                .map(|c| {
                    self.collection
                        .showable_scenes(c)
                        .into_iter()
                        .map(|s| SceneRow {
                            id: s.id,
                            name: s.name.clone(),
                            own_audio: s.own_levels(),
                            transition_in: s.transition_in,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let scenes = rows(Some(self.canvas));
        let hold_scenes = [scenes.clone(), rows(self.vertical.map(|v| v.canvas))];
        SceneList {
            scenes,
            hold_scenes,
            program: self.scene,
            preview: self.preview(),
            master: self
                .collection
                .canvas(self.canvas)
                .map(|c| c.master)
                .unwrap_or_default(),
            glide: self.collection.glide(),
            transition: self.collection.transition(),
            stinger_cut_found_ms: self.stinger.cut_found_ms(),
            stinger_length_ms: self
                .stinger_length()
                .map(|l| u32::try_from(l.as_millis()).unwrap_or(u32::MAX)),
            stinger_looking: self.stinger.looking(),
            stinger_alpha_unsupported: self.stinger.alpha_unsupported(),
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
        self.crisp_after_box(item, gesture, &name);
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
        self.note_collection();
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
                self.note_collection();
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

    /// Sets or clears an item's saved scroll on `on` (ADR-0150): one undo step. False
    /// when it was already so.
    fn set_item_scroll(
        &mut self,
        item: ItemId,
        scroll: Option<Scroll>,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, String> {
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
        if layout.scroll == scroll {
            return Ok(false);
        }
        layout.scroll = scroll;
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| edit_error(&e))?;
        let name = match found.content {
            ItemContent::Source(id) => self.name_of(id),
            _ => String::new(),
        };
        self.history
            .record(EditKind::Scroll, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A live call on an item's scroll run (ADR-0150 decision 11).
    fn api_scroll_control(
        &mut self,
        item: u32,
        on: OutputCanvas,
        control: ScrollControl,
    ) -> Result<Response, ApiError> {
        let canvas = self.edit_canvas(on).map_err(|e| refused(&e))?;
        let id = ItemId::from_raw(item);
        let Some((_, found)) = self.collection.item(id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no item {item}"),
            ));
        };
        if matches!(found.content, ItemContent::Scene(_)) {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "scroll is for source items, not nested scenes",
            ));
        }
        match self.studio.scroll_control(id, canvas, control) {
            Ok(_) => Ok(Response::Done(Empty {})),
            Err(ScrollRefused::NoScroll) => Err(ApiError::new(
                ErrorKind::InvalidParams,
                "This source does not scroll on that canvas.",
            )),
            Err(_) => Err(ApiError::new(
                ErrorKind::Conflict,
                "This source is not on a scene a canvas shows, so its scroll is not running.",
            )),
        }
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

    fn publish_transitions(&mut self) {
        for e in self.studio.take_transition_events() {
            let canvas = if e.canvas == self.canvas {
                OutputCanvas::Main
            } else if self.vertical.map(|v| v.canvas) == Some(e.canvas) {
                OutputCanvas::Vertical
            } else {
                continue;
            };
            // The held copy (ADR-0063) is named by its real scene.
            let real = |id: SceneId, held: Option<SceneId>| {
                if id.get() >= HELD_IDS { held } else { Some(id) }
            };
            let now_held = self.held.as_ref().map(|h| h.from);
            let (Some(from), Some(to)) = (
                e.from.and_then(|id| real(id, self.held_was.or(now_held))),
                e.to.and_then(|id| real(id, now_held.or(self.held_was))),
            ) else {
                continue;
            };
            let state = api::TransitionStateChanged {
                canvas,
                state: match e.phase {
                    TransitionPhase::Started => api::TransitionPhase::Started,
                    TransitionPhase::Holding => api::TransitionPhase::Holding,
                    _ => api::TransitionPhase::Ended,
                },
                kind: api_transition_kind(e.kind),
                duration_ms: u32::try_from(e.duration.as_millis()).unwrap_or(u32::MAX),
                from: from.get(),
                to: to.get(),
                at: e.at.as_nanos(),
                manual: e.manual,
                t: e.progress,
            };
            self.events
                .changed(canvas_key(Topic::TransitionState, canvas), || {
                    EventPayload::TransitionState(state)
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
            state.and_then(|st| st.failed.as_deref()).hash(&mut hasher);
            state
                .and_then(|st| st.fault)
                .map(|f| f as u8)
                .hash(&mut hasher);
            state.and_then(|st| st.reason).hash(&mut hasher);
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
                    browser_reason: state.and_then(|st| st.reason),
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

/// The control API's view of the host: one request at a time on the control thread.
struct ApiTarget<'a> {
    host: &'a mut Host,
    notify: &'a dyn Fn(Update),
}

impl ControlTarget for ApiTarget<'_> {
    fn handle_as(
        &mut self,
        caller: &rbs_api::Caller,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        self.host.check_on_air(caller.irreversible, &command)?;
        self.host.check_network(caller.irreversible, &command)?;
        self.host
            .check_local_only(caller.token.is_some(), &command)?;
        self.handle(command, events)
    }

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

/// An estimate enum as the API's, through its wire name: a name the API types do
/// not know reads as their `Other`.
fn api_enum<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

fn api_resource(r: rbs_record::estimate::Resource) -> api::EstimateResource {
    api_enum(r.as_str()).unwrap_or(api::EstimateResource::Other)
}

fn api_level(l: rbs_record::estimate::Level) -> api::EstimateLevel {
    api_enum(l.as_str()).unwrap_or(api::EstimateLevel::Other)
}

/// A number the API may send: finite, else `None` (ADR-0087: never NaN or infinity).
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// `record.estimate`'s answer from the estimate (protocol 1.53). `at_most_now` is the
/// most separate video files a start opens now, when the plan has a picture template.
fn api_estimate(
    e: &rbs_record::estimate::Estimate,
    at_most_now: Option<usize>,
) -> api::EstimateAnswer {
    use rbs_record::estimate as est;
    let track = |t: &est::TrackRef| api::EstimateTrack {
        source: t.source.get(),
        role: api_track_role(t.role),
    };
    let max = |m: &est::MaxTracks, now: Option<usize>| api::MaxTracks {
        count: m.count,
        limit: m.limit.map(api_resource),
        partial: m.partial,
        unknown: m.unknown.iter().copied().map(api_resource).collect(),
        at_most_now: now.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        known_count: m.known_count,
    };
    api::EstimateAnswer {
        level: api_level(e.level),
        gate_level: api_level(e.gate_level),
        unknown: e.unknown.iter().copied().map(api_resource).collect(),
        resources: e
            .resources
            .iter()
            .map(|r| {
                let need = finite(r.need).unwrap_or(0.0);
                let limit = r.limit.and_then(finite);
                // A limit that is not a finite number is reported as unknown.
                let level = match (r.limit, limit) {
                    (Some(_), None) => Some(api::EstimateLevel::Unknown),
                    _ => r.level.map(api_level),
                };
                api::EstimateRow {
                    resource: api_resource(r.resource),
                    scope: r.scope.clone(),
                    need,
                    limit,
                    unit: api_enum(r.unit.as_str()).unwrap_or(api::EstimateUnit::Other),
                    level,
                    measured: r.measured,
                    calibrate: r.calibrate,
                }
            })
            .collect(),
        reasons: e
            .reasons
            .iter()
            .map(|r| api::EstimateReason {
                code: api_enum(r.code.as_str()).unwrap_or(api::EstimateCode::Other),
                resource: r.resource.map(api_resource),
                track: r.track.as_ref().map(track),
            })
            .collect(),
        max_tracks: api::MaxTracksByKind {
            video: max(&e.max_video, at_most_now),
            audio: max(&e.max_audio, None),
        },
        tracks: e
            .tracks
            .iter()
            .map(|t| api::TrackCost {
                source: t.track.source.get(),
                role: api_track_role(t.track.role),
                cost: t
                    .cost
                    .iter()
                    .filter_map(|(r, v)| Some((r.as_str().to_owned(), finite(*v)?)))
                    .collect(),
            })
            .collect(),
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
        // No output starts while *Test this computer* runs (ADR-0087 step D1).
        if matches!(
            command,
            C::RecordStart(_)
                | C::RecordStartToFile(_)
                | C::StreamStart(_)
                | C::VcamStart
                | C::ReplayArm
        ) && let Some(why) = host.calibration_blocks()
        {
            return Err(ApiError::new(ErrorKind::Conflict, why));
        }
        match command {
            C::StudioGet => {
                host.note_encoders(notify);
                Ok(Response::Studio(api::Snapshot {
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
                }))
            }
            C::SourceList => Ok(Response::Sources(api::Snapshot {
                seq: host.events.seq_map([Topic::SourceChanged]),
                data: host
                    .heard
                    .iter()
                    .map(|row| with_input_counters(&host.studio, api_source(row)))
                    .collect(),
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
            C::SourceAdd(p) if p.kind == api::SourceKind::Text => {
                // A text has no target (ADR-0129 decision 8).
                let text = p.text.as_deref().unwrap_or("Text");
                let id = if p.global {
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_text(p.name.as_deref(), text, true)
                        .map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        h.add_text(p.name.as_deref(), text, false)
                            .map_err(|e| refused(&e))
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceAdd(p) => {
                if p.text.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "text is only for a text source",
                    ));
                }
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
                data: scene_cache::answer(host),
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
            C::SceneDuplicate(p) => host.api_scene_duplicate(p, notify),
            C::SourceDuplicate(p) => host.api_source_duplicate(p, notify),
            C::SourceReset(p) => host.api_source_reset(p, notify),
            C::SourceCopyAttributes(p) => host.api_copy_attributes(p, notify),
            C::SourceAttributesClipboard => Ok(host.api_attributes_clipboard()),
            C::SourcePasteAttributes(p) => host.api_paste_attributes(p, notify),
            C::SceneAudioGet(p) => {
                let id = SceneId::from_raw(p.scene);
                host.levels_scene(id).map_err(|e| refused(&e))?;
                let audio = host.collection.scene(id).and_then(|s| s.audio.as_ref());
                Ok(Response::SceneLevels(rbs_api::request::SceneLevels {
                    scene: p.scene,
                    own: audio.is_some(),
                    levels: audio
                        .map(|a| {
                            a.levels
                                .iter()
                                .map(|(source, l)| rbs_api::request::SceneLevelInfo {
                                    source: source.get(),
                                    volume: l.volume,
                                    muted: l.muted,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }))
            }
            C::SceneAudioSet(p) => {
                let (turned_own, changed) = host
                    .scene_level_set(
                        SceneId::from_raw(p.scene),
                        SourceId::from_raw(p.source),
                        p.volume,
                        p.muted,
                    )
                    .map_err(|e| refused(&e))?;
                if turned_own {
                    // An undo step: saved and on the history like any edit.
                    host.apply(notify);
                } else if changed {
                    host.publish(notify);
                }
                Ok(Response::SceneLevelSet(rbs_api::request::SceneLevelSet {
                    turned_own,
                }))
            }
            C::SceneAudioOwn(p) => {
                if host
                    .scene_levels_own(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioShared(p) => {
                if host
                    .scene_levels_shared(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioResetSource(p) => {
                if host
                    .scene_level_reset(SceneId::from_raw(p.scene), SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
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
            C::CanvasTransitionProgress(p) => {
                if !p.t.is_finite() || !(0.0..=1.0).contains(&p.t) {
                    return Err(refused(&Refusal::Invalid(
                        "t must be a number from 0 to 1.".to_owned(),
                    )));
                }
                if p.canvas == Some(OutputCanvas::Vertical) {
                    return Err(refused(&Refusal::Conflict(
                        "Only the main canvas has a T-bar.".to_owned(),
                    )));
                }
                if host.preview().is_none() {
                    return Err(refused(&Refusal::Conflict(STUDIO_MODE_OFF.to_owned())));
                }
                host.pending_progress = Some(p.t);
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransition(p) => {
                let once = transition_once(p).map_err(|e| refused(&e))?;
                let changed = host.transition().map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSwitch(p) => {
                let once = transition_once(p.once()).map_err(|e| refused(&e))?;
                let changed = host
                    .switch_scene(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
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
            C::LoudnessStatus => Ok(Response::LoudnessStatus(host.api_loudness())),
            C::LoudnessSetEnabled(p) => {
                let done = host.set_loudness_enabled(p.on).map_err(|e| refused(&e));
                host.publish(notify);
                done?;
                Ok(Response::Done(Empty {}))
            }
            C::LoudnessReset => {
                host.reset_loudness().map_err(|e| refused(&e))?;
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
            C::FilterTypes(p) => {
                let graph = &host.studio.settings().audio_graph;
                let setup = rbs_audio_filters::DspSetup::new(graph.format, graph.block_frames);
                Ok(Response::FilterTypes(filters::api_types(p.domain, setup)))
            }
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
            C::SyncMeasureStart(p) => {
                let (measurement, reference) = host.start_sync_measure(
                    SourceId::from_raw(p.source),
                    p.reference.map(SourceId::from_raw),
                )?;
                Ok(Response::SyncMeasureStarted(
                    rbs_api::request::SyncMeasureStarted {
                        measurement,
                        reference: reference.get(),
                    },
                ))
            }
            C::SyncMeasureCancel(p) => {
                host.cancel_sync_measure(&p.measurement)?;
                Ok(Response::Done(Empty {}))
            }
            C::SyncDevices => Ok(Response::SyncDevices(rbs_api::request::SyncDeviceList {
                devices: host.api_sync_devices(),
            })),
            C::SyncForgetDevice(p) => {
                if host.forget_device(&sync_measure::file_key(p.key)) {
                    Ok(Response::Done(Empty {}))
                } else {
                    Err(ApiError::new(
                        ErrorKind::NotFound,
                        "No offset is remembered for this device.",
                    ))
                }
            }
            C::SyncApplyToDeviceSources(p) => {
                let source = SourceId::from_raw(p.source);
                let peers = host.device_peers(source);
                host.apply_to_device_sources(source)
                    .map_err(|e| refused(&e))?;
                if !peers.is_empty() {
                    host.publish(notify);
                }
                Ok(Response::SyncAppliedToDeviceSources(
                    rbs_api::request::SyncAppliedToDeviceSources {
                        changed: peers.iter().map(|(id, _)| id.get()).collect(),
                    },
                ))
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
                // A text path would put any readable file on air: the API cannot write
                // it until token scopes exist (ADR-0129 decision 8).
                if p.settings.contains_key(keys::FILE)
                    && host
                        .collection
                        .source(SourceId::from_raw(p.source))
                        .is_some_and(|s| SourceKind::of(&s.type_id) == Some(SourceKind::Text))
                {
                    return Err(ApiError::new(
                        ErrorKind::Forbidden,
                        "the control API cannot set a text source's file",
                    ));
                }
                if host
                    .patch_source_settings(SourceId::from_raw(p.source), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetText(p) => {
                if host
                    .set_source_text(SourceId::from_raw(p.source), &p.text)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::TextTimer(p) => host.text_timer(p),
            C::TextFonts => match host.studio.text_fonts(&host.adapters.sources) {
                rbs_record::TextFonts::Ready(families) => {
                    Ok(Response::TextFonts(rbs_api::types::TextFonts {
                        families: families.as_ref().clone(),
                    }))
                }
                rbs_record::TextFonts::NotReady => Err(ApiError::new(
                    ErrorKind::NotReady,
                    "the font list is being read; ask again shortly",
                )),
                rbs_record::TextFonts::Unavailable => Err(ApiError::new(
                    ErrorKind::Failed,
                    "no text renderer is installed",
                )),
            },
            C::MediaPlay(p) => host
                .media_command(p.source, MediaOp::Play)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaPause(p) => host
                .media_command(p.source, MediaOp::Pause)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaSeek(p) => host
                .media_command(p.source, MediaOp::Seek(p.position_ms))
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaRestart(p) => host
                .media_command(p.source, MediaOp::Restart)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaState(p) => host
                .media_command(p.source, MediaOp::State)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::BrowserReload(p) => host
                .browser_message(
                    p.source,
                    &rbs_sources::BrowserMessage::Reload {
                        bypass_cache: p.bypass_cache,
                    },
                )
                .map(|()| Response::Done(Empty {})),
            C::BrowserRunScript(p) => host
                .browser_message(p.source, &rbs_sources::BrowserMessage::RunScript(p.script))
                .map(|()| Response::Done(Empty {})),
            C::BrowserForgetSignIn(p) => {
                host.forget_sign_in(SourceId::from_raw(p.source), false)?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::BrowserInput(p) => host
                .browser_input(p.source, &p.events)
                .map(|()| Response::Done(Empty {})),
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
                let (_, removed) = host
                    .remove_from_scene(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemRemoved(item_removed(&removed)))
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
                    Ok(item) => Response::ItemAdded(ItemAdded { item: item.get() }),
                    Err(removed) => Response::ItemRemoved(item_removed(&removed)),
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
            C::ItemSetScroll(p) => {
                let scroll = match &p.scroll {
                    Some(s) => Some(
                        scene_scroll(s)
                            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?,
                    ),
                    None => None,
                };
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
                let item = ItemId::from_raw(p.item);
                match host.collection.item(item).map(|(_, i)| i.content) {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "scroll is for source items, not nested scenes",
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
                    .set_item_scroll(item, scroll, 0, p.canvas)
                    .map_err(failed)?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemScrollPause(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Pause)
            }
            C::ItemScrollResume(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Resume)
            }
            C::ItemScrollRestart(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Restart)
            }
            C::ItemScrollSpeed(p) => {
                if !(MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(&p.speed) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "speed must be 1 to 4000; pause is item.scroll_pause",
                    ));
                }
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Speed(f64::from(p.speed)))
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
                let gate = start_gate(&p, host.limit_policy)?;
                match p.canvas.unwrap_or_default().single() {
                    Some(one) => {
                        let c = host.api_canvas_of(Some(one))?;
                        let path = host.recording_file(c);
                        let iso = p.iso.unwrap_or(host.separate_files);
                        self.record(c, path, false, iso, gate)
                    }
                    None => {
                        let iso = p.iso.unwrap_or(host.separate_files);
                        self.start_all(api::OutputKind::Record, iso, gate)
                    }
                }
            }
            C::RecordEstimate(p) => {
                check_profile(&p.profile)?;
                let target = p.canvas.unwrap_or_default();
                let (c, all) = match target.single() {
                    Some(one) => (host.api_canvas_of(Some(one))?, false),
                    None => (OutputCanvas::Main, true),
                };
                host.record_estimate(c, all, p.iso)
                    .map(Response::RecordEstimate)
                    .map_err(failed)
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
                self.record(c, path, true, false, StartGate::Older)
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
            C::RecordPause(p) => self.pause_asked(p, Some(true)),
            C::RecordResume(p) => self.pause_asked(p, Some(false)),
            C::RecordPauseToggle(p) => self.pause_asked(p, None),
            C::StreamHold(p) => self.hold_asked(p.target(), Some(true), p),
            C::StreamResume(p) => self.hold_asked(
                p.target(),
                Some(false),
                rbs_api::request::StreamHold::default(),
            ),
            C::StreamHoldToggle(p) => self.hold_asked(p.target(), None, p),
            C::OutputBreakToggle => self.break_toggle(),
            C::MarkerAdd(p) => {
                let added = host.add_marker(&p)?;
                notify(host.markers_update(Some(&added)));
                Ok(Response::MarkersAdded(added))
            }
            C::MarkerRemove(p) => {
                host.remove_marker(p.canvas, p.index)?;
                notify(host.markers_update(None));
                Ok(Response::Done(Empty {}))
            }
            C::ProjectExport(p) => host
                .export_project(&p.recording, p.format)
                .map(Response::ProjectExported),
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
                    return self.start_all(api::OutputKind::Stream, false, StartGate::Older);
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
            C::StreamDestinations(p) => {
                let canvases = match p.canvas {
                    Some(c) => vec![host.api_canvas_of(Some(c))?],
                    None if host.vertical.is_some() => {
                        vec![OutputCanvas::Main, OutputCanvas::Vertical]
                    }
                    None => vec![OutputCanvas::Main],
                };
                Ok(Response::StreamDestinations(api::StreamDestinations {
                    destinations: canvases
                        .into_iter()
                        .flat_map(|c| host.api_destinations(c))
                        .collect(),
                }))
            }
            C::StreamDestinationStart(p) => self.destination_start(p.id),
            C::StreamDestinationStop(p) => self.destination_stop(p.id),
            C::StreamDestinationSetEnabled(p) => {
                if host.destination_canvas(p.id).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("there is no destination {}", p.id),
                    ));
                }
                if host.destinations.set_enabled(p.id, p.enabled) {
                    notify(Update::StreamDestinations(host.destinations.clone()));
                }
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
            C::CalibrationRun(p) => {
                check_profile(&p.profile)?;
                let Some(one) = p.canvas.unwrap_or_default().single() else {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "one canvas per run: main or vertical",
                    ));
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.switch_pending.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the studio is restarting",
                    ));
                }
                host.run_calibration(c).map_err(|e| match e {
                    calibration::RunRefusal::Conflict(why) => {
                        ApiError::new(ErrorKind::Conflict, why)
                    }
                    calibration::RunRefusal::NotFound(why) => {
                        ApiError::new(ErrorKind::NotFound, why)
                    }
                    calibration::RunRefusal::Failed(why) => failed(why),
                })?;
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationCancel => {
                host.cancel_calibration();
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationStatus => Ok(Response::CalibrationStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::CalibrationState]),
                data: host.api_calibration(),
            })),
            C::ProtectionStatus => Ok(Response::ProtectionStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::ProtectionChanged]),
                data: host.api_protection(),
            })),
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
                if let Some(l) = &patch.loudness
                    && let Some(why) =
                        crate::loudness_settings::problem(l.target, l.true_peak_limit)
                {
                    return Err(ApiError::new(ErrorKind::InvalidParams, why));
                }
                // A canvas change is checked here; applying it is the part that can
                // still fail, and then nothing of the patch has been applied.
                let canvas = match &patch.canvas {
                    Some(canvas) => {
                        let c = canvas_profile(canvas)?;
                        let same = host
                            .collection
                            .canvas(host.canvas)
                            .is_some_and(|k| k.size == c.size() && k.frame_rate == c.frame_rate());
                        if same {
                            None
                        } else if host.any_recording() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "stop the recording before changing the canvas",
                            ));
                        } else if host.any_streaming() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing the canvas",
                            ));
                        } else {
                            Some(c)
                        }
                    }
                    None => None,
                };
                // The loudness file is written before anything is applied (a failed
                // save changes nothing); a canvas change that then fails puts it back.
                let loudness = match &patch.loudness {
                    Some(l) => host
                        .save_loudness_settings(l.target, l.true_peak_limit)
                        .map_err(|e| refused(&e))?,
                    None => None,
                };
                if let Some(c) = canvas
                    && let Err(e) = host.change_canvas(c.size(), c.frame_rate(), notify)
                {
                    if loudness.is_some() {
                        host.undo_loudness_save();
                    }
                    return Err(failed(e));
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
                if let Some(saved) = loudness {
                    host.apply_loudness_settings(saved);
                    host.publish(notify);
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
            C::ImportObsScan => obs_import_api::scan(),
            C::ImportObsCheck(p) => obs_import_api::check(&p),
            C::ImportObsRun(p) => host.import_obs(&p, notify),
            C::AddonList => Ok(Response::AddOnList(host.addon_list())),
            C::AddonInstall(r) => host
                .addon_install(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonCancel(r) => host.addon_cancel(&r.id).map(|()| Response::Done(Empty {})),
            C::AddonClean(r) => host
                .addon_clean(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonRemove(r) => host
                .addon_remove(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
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
            C::SceneSetTransition(p) => {
                let t = p.transition.map(|t| rbs_scene::SceneTransition {
                    kind: t.kind.map(scene_transition_kind),
                    duration_ms: t.duration_ms,
                });
                let changed = host
                    .set_scene_transition(SceneId::from_raw(p.scene), t)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CollectionSetTransition(p) => {
                let changed = host.set_transition(&p).map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::TransitionSet(rbs_api::request::TransitionSet {
                    alpha_unsupported: host.stinger_alpha_unsupported(),
                }))
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
            C::SourceSetRecordSeparately(p) => {
                if host
                    .set_record_separately(SourceId::from_raw(p.source), RecordPatch::from_api(&p))
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
            C::SourceSetActivity(p) => {
                let activity = rbs_scene::activity::SourceActivity::new(
                    p.run.map(activity_run),
                    p.on_return.map(activity_return),
                    p.linger_s,
                );
                if host
                    .set_source_activity(SourceId::from_raw(p.source), activity)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetActivity(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                let traits = rbs_sources::activity_traits(&source.type_id);
                let saved = source.activity;
                let effective = traits.effective(Some(&saved));
                Ok(Response::SourceActivity(
                    rbs_api::request::SourceActivityInfo {
                        source: p.source,
                        run: saved.run.map(api_run),
                        on_return: saved.on_return.map(api_return),
                        linger_s: saved.linger_s,
                        effective: rbs_api::request::EffectiveActivity {
                            run: api_run(effective.run),
                            on_return: effective.on_return.map(api_return),
                            linger_s: effective.linger_s,
                        },
                        supported: saved.is_default() || traits.allows(&saved),
                        runs: traits.runs.iter().copied().map(api_run).collect(),
                        on_returns: traits.on_returns.iter().copied().map(api_return).collect(),
                    },
                ))
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
        iso: bool,
        gate: StartGate,
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
        host.note_encoders(notify);
        if !host.record_profile(c).format.is_audio_only()
            && let Some(why) = &host.cannot_record
        {
            return Err(failed(why.clone()));
        }
        if host.outs[slot(c)].recording.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a recording is already running",
            ));
        }
        let mut request = host.profile_request(c, path).map_err(|why| match c {
            OutputCanvas::Main => failed(why),
            OutputCanvas::Vertical => failed(format!("the vertical recording: {why}")),
        })?;
        request.iso = iso;
        let profile = (!explicit).then(|| API_PROFILE_ID.to_owned());
        let result = host.start_recording(c, &request, profile, gate);
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
                        estimate: host.running_estimate(c),
                        // A one-file take (every video file skipped, ADR-0087
                        // decision 7) has its skipped files only in its state.
                        skipped: match &host.outs[slot(c)].record_state {
                            api::RecordState::Recording { skipped, .. } => skipped.clone(),
                            _ => Vec::new(),
                        },
                    },
                    _ => return Err(failed("the recording did not start")),
                };
                // The UI shows the recording as if its own button started it.
                notify(recording_update(c, update));
                Ok(Response::RecordStarted(started))
            }
            Err(why) => Err(why.into_api()),
        }
    }
}

/// How a `record.start` asks for its separate video files (protocol 1.54): with
/// neither new parameter it is an older request; `cpu` is refused until the CPU
/// encoder for separate files exists (ADR-0119 step (c), owner record on ADR-0087).
fn start_gate(
    p: &rbs_api::request::RecordStart,
    profile: (LimitPolicy, bool),
) -> Result<StartGate, ApiError> {
    use rbs_api::request::HardwareLimitPolicy as P;
    if p.acknowledge_heavy.is_none() && p.on_hardware_limit.is_none() {
        return Ok(StartGate::Older);
    }
    let policy = match p.on_hardware_limit {
        Some(P::Ask) => LimitPolicy::Ask,
        Some(P::Skip) => LimitPolicy::Skip,
        Some(P::Cpu) => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "the CPU encoder for separate files comes with a later version",
            ));
        }
        None => {
            if !profile.1 {
                rbs_crash::errln!(
                    "rbs: recording.iso.on_hardware_limit has a value this version does not \
                     know; it behaves as \"ask\""
                );
            }
            profile.0
        }
    };
    Ok(StartGate::Asked {
        acknowledge_heavy: p.acknowledge_heavy.unwrap_or(false),
        policy,
    })
}

impl ApiTarget<'_> {
    /// `record.start` or `stream.start` with `canvas: "all"` (ADR-0111 decision 3): the
    /// main canvas first, whose refusal fails the call; then the vertical one, whose
    /// refusal is only its entry. One step on the control thread.
    fn start_all(
        &mut self,
        kind: api::OutputKind,
        iso: bool,
        gate: StartGate,
    ) -> Result<Response, ApiError> {
        let mut outputs = Vec::new();
        for c in self.host.output_canvases() {
            let out = &self.host.outs[slot(c)];
            let (running, ending) = match kind {
                api::OutputKind::Record => (out.recording.is_some(), out.ending),
                api::OutputKind::Stream => (out.streaming.is_some(), false),
            };
            let entry = |outcome| api::OutputEntry {
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
                estimate: None,
                skipped: Vec::new(),
            };
            if ending {
                let e = ApiError::new(ErrorKind::Conflict, "the recording is still ending");
                if c == OutputCanvas::Main {
                    return Err(e);
                }
                outputs.push(api::OutputEntry {
                    kind: None,
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                });
                continue;
            }
            if running {
                outputs.push(entry(api::OutputOutcome::Running));
                continue;
            }
            // `canvas: "all"` starts the canvases where streaming is available
            // (ADR-0148 decision 10).
            if kind == api::OutputKind::Stream && !self.host.stream_available(c) {
                outputs.push(entry(api::OutputOutcome::NotConfigured));
                continue;
            }
            let started = match kind {
                api::OutputKind::Record => {
                    let path = self.host.recording_file(c);
                    self.record(c, path, false, iso, gate)
                }
                api::OutputKind::Stream => self.stream(c),
            };
            match started {
                Ok(Response::RecordStarted(r)) => outputs.push(api::OutputEntry {
                    kind: None,
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    estimate: r.estimate,
                    skipped: r.skipped,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(Response::StreamStarted(r)) => outputs.push(api::OutputEntry {
                    kind: None,
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(_) => outputs.push(entry(api::OutputOutcome::Started)),
                Err(e) if c == OutputCanvas::Main => return Err(e),
                Err(e) => outputs.push(api::OutputEntry {
                    kind: None,
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                }),
            }
        }
        // Refused only when no canvas streams or can (ADR-0148 decision 10).
        if kind == api::OutputKind::Stream
            && outputs
                .iter()
                .all(|o| o.outcome == api::OutputOutcome::NotConfigured)
        {
            return Err(failed(STREAMING_OFF));
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
    /// `record.pause` (`Some(true)`), `record.resume` (`Some(false)`) or
    /// `record.pause_toggle` (`None`) on the canvases `p` names (control API 1.61).
    fn pause_asked(
        &mut self,
        p: rbs_api::request::OnCanvases,
        pause: Option<bool>,
    ) -> Result<Response, ApiError> {
        let Some(one) = p.target().single() else {
            return self.pause_all(pause);
        };
        let notify = self.notify;
        let host = &mut *self.host;
        let c = host.api_canvas_of(Some(one))?;
        host.pause_recording(c, pause)?;
        notify(host.pause_update());
        host.publish(notify);
        Ok(Response::Done(Empty {}))
    }

    /// `stream.hold` (`Some(true)`), `stream.resume` (`Some(false)`) or
    /// `stream.hold_toggle` (`None`) on the canvases `target` names (control API
    /// 1.64). With `all`: every live stream, main first; a toggle holds them all when
    /// any is not held, else resumes them all.
    fn hold_asked(
        &mut self,
        target: api::OutputTarget,
        hold: Option<bool>,
        asked: rbs_api::request::StreamHold,
    ) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if let Some(one) = target.single() {
            let c = host.api_canvas_of(Some(one))?;
            host.hold_stream(c, hold, asked)?;
            notify(host.pause_update());
            notify(host.hold_update());
            host.publish(notify);
            return Ok(Response::Done(Empty {}));
        }
        let canvases = host.output_canvases();
        let live = |host: &Host, c: OutputCanvas| host.outs[slot(c)].streaming.is_some();
        let hold = hold.unwrap_or_else(|| {
            canvases
                .iter()
                .any(|&c| live(host, c) && !host.stream_held(c))
        });
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        for c in canvases {
            let (outcome, error) = if !live(host, c) {
                (api::OutputOutcome::NotRunning, None)
            } else {
                match host.hold_stream(c, Some(hold), asked) {
                    Ok(true) => (api::OutputOutcome::Held, None),
                    Ok(false) => (api::OutputOutcome::Resumed, None),
                    Err(e) => {
                        let error = api::OutputError::from(&e);
                        refusals.push(e);
                        (api::OutputOutcome::Refused, Some(error))
                    }
                }
            };
            outputs.push(api::OutputEntry {
                canvas: c,
                kind: None,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        }
        changed_or_first_refusal(&outputs, refusals, "nothing streams")?;
        notify(host.pause_update());
        notify(host.hold_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `output.break_toggle` (control API 1.64, ADR-0124 decision 15): when every
    /// live stream is on hold and every running recording is paused, resumes them
    /// all (streams first, so a recording a hold paused resumes with its stream);
    /// otherwise holds the streams that are not held (with the picture their "While on
    /// hold" setting names) and pauses the recordings that are not paused. The answer lists the outputs it changed or
    /// that refused, main first, each with its `kind`.
    fn break_toggle(&mut self) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let canvases = host.output_canvases();
        let streams: Vec<_> = canvases
            .iter()
            .copied()
            .filter(|&c| host.outs[slot(c)].streaming.is_some())
            .collect();
        let recording = |host: &Host, c: OutputCanvas| {
            let o = &host.outs[slot(c)];
            o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1)
        };
        let paused = |host: &Host, c: OutputCanvas| {
            recording(host, c)
                .and_then(|id| host.studio.output_pause(id))
                .is_some_and(|p| p.paused)
        };
        let records: Vec<_> = canvases
            .iter()
            .copied()
            .filter(|&c| recording(host, c).is_some())
            .collect();
        if streams.is_empty() && records.is_empty() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "nothing records or streams",
            ));
        }
        let resume = streams.iter().all(|&c| host.stream_held(c))
            && records.iter().all(|&c| paused(host, c));
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        let mut entry = |c, kind, done: Result<api::OutputOutcome, ApiError>| {
            let (outcome, error) = match done {
                Ok(o) => (o, None),
                Err(e) => {
                    let error = api::OutputError::from(&e);
                    refusals.push(e);
                    (api::OutputOutcome::Refused, Some(error))
                }
            };
            outputs.push(api::OutputEntry {
                canvas: c,
                kind: Some(kind),
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        };
        let before: Vec<bool> = records.iter().map(|&c| paused(host, c)).collect();
        for &c in &streams {
            if resume || !host.stream_held(c) {
                let done = host
                    .hold_stream(c, Some(!resume), rbs_api::request::StreamHold::default())
                    .map(|held| {
                        if held {
                            api::OutputOutcome::Held
                        } else {
                            api::OutputOutcome::Resumed
                        }
                    });
                entry(c, api::OutputKind::Stream, done);
            }
        }
        // After the streams: a hold may have paused, or its end resumed, a recording
        // sharing its encoder (decision 14); that recording is listed as changed.
        for (&c, was) in records.iter().zip(before) {
            let now = paused(host, c);
            if was != now {
                let outcome = if now {
                    api::OutputOutcome::Paused
                } else {
                    api::OutputOutcome::Resumed
                };
                entry(c, api::OutputKind::Record, Ok(outcome));
            } else if now == resume {
                let done = host.pause_recording(c, Some(!resume)).map(|p| {
                    if p {
                        api::OutputOutcome::Paused
                    } else {
                        api::OutputOutcome::Resumed
                    }
                });
                entry(c, api::OutputKind::Record, done);
            }
        }
        changed_or_first_refusal(&outputs, refusals, "nothing changed")?;
        notify(host.pause_update());
        notify(host.hold_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `record.pause`, `record.resume` or `record.pause_toggle` with `canvas: "all"`
    /// (control API 1.61): every running recording, main first. A toggle pauses them
    /// all when any runs unpaused, else resumes them all. `conflict` when none
    /// changed: the one refusal when there is one, else "nothing records".
    fn pause_all(&mut self, pause: Option<bool>) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let canvases = host.output_canvases();
        let running = |host: &Host, c: OutputCanvas| {
            let o = &host.outs[slot(c)];
            o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1)
        };
        let pause = pause.unwrap_or_else(|| {
            canvases.iter().any(|&c| {
                running(host, c)
                    .is_some_and(|id| !host.studio.output_pause(id).is_some_and(|p| p.paused))
            })
        });
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        for c in canvases {
            let (outcome, error) = if running(host, c).is_none() {
                (api::OutputOutcome::NotRunning, None)
            } else {
                match host.pause_recording(c, Some(pause)) {
                    Ok(true) => (api::OutputOutcome::Paused, None),
                    Ok(false) => (api::OutputOutcome::Resumed, None),
                    Err(e) => {
                        let error = api::OutputError::from(&e);
                        refusals.push(e);
                        (api::OutputOutcome::Refused, Some(error))
                    }
                }
            };
            outputs.push(api::OutputEntry {
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        }
        if !outputs.iter().any(|o| {
            matches!(
                o.outcome,
                api::OutputOutcome::Paused | api::OutputOutcome::Resumed
            )
        }) {
            return Err(refusals
                .into_iter()
                .next()
                .unwrap_or_else(|| ApiError::new(ErrorKind::Conflict, "nothing records")));
        }
        notify(host.pause_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

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
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
                estimate: None,
                skipped: Vec::new(),
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
        self.stream_to(c, None)
    }

    /// The running canvas of destination `id`, or `not_found`.
    fn destination_of(&self, id: u32) -> Result<OutputCanvas, ApiError> {
        let c = self.host.destination_canvas(id).ok_or_else(|| {
            ApiError::new(ErrorKind::NotFound, format!("there is no destination {id}"))
        })?;
        self.host.api_canvas_of(Some(c))
    }

    /// `stream.destination.start` (protocol 1.77, ADR-0127 decision 16).
    fn destination_start(&mut self, id: u32) -> Result<Response, ApiError> {
        let c = self.destination_of(id)?;
        let notify = self.notify;
        let conflict = |why: &str| Err(ApiError::new(ErrorKind::Conflict, why));
        let Some(d) = self
            .host
            .resolved_destinations(c)
            .into_iter()
            .find(|d| d.destination.id == id)
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no destination {id}"),
            ));
        };
        // Turning one on stays a separate, visible step.
        if !d.destination.enabled {
            return conflict("the destination is turned off: turn it on first");
        }
        if d.parsed().is_none() {
            return conflict("the destination is not set up: enter its server in Settings, Stream");
        }
        if self.host.outs[slot(c)].streaming.is_none() {
            return self.stream_to(c, Some(id));
        }
        if self.host.outs[slot(c)]
            .destinations
            .iter()
            .any(|d| d.id == id && d.output.is_some())
        {
            return conflict("the destination is already on air");
        }
        let host = &mut *self.host;
        let (request, hidden) = host.api_stream_request(c, Some(id))?;
        let result = host.start_destination(c, &request, id);
        host.report_link(c, notify);
        host.publish(notify);
        result.map_err(|why| {
            failed(
                hidden
                    .iter()
                    .fold(why, |w, url| w.replace(url.as_str(), "the saved server")),
            )
        })?;
        Ok(Response::Done(Empty {}))
    }

    /// `stream.destination.stop` (protocol 1.77): the last running destination ends
    /// the stream, as `stream.stop` does.
    fn destination_stop(&mut self, id: u32) -> Result<Response, ApiError> {
        let c = self.destination_of(id)?;
        let notify = self.notify;
        let host = &mut *self.host;
        let o = &host.outs[slot(c)];
        let running = o.destinations.iter().filter(|d| d.output.is_some()).count();
        let this = o
            .destinations
            .iter()
            .any(|d| d.id == id && d.output.is_some());
        if o.streaming.is_none() || !this {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "the destination is not on air",
            ));
        }
        if running == 1 {
            host.end_stream_asked(c, notify);
        } else {
            host.stop_destination(c, id);
            host.report_link(c, notify);
        }
        host.publish(notify);
        Ok(Response::Done(Empty {}))
    }

    /// `stream.start` of `c`, or `stream.destination.start` of `only` while `c` does
    /// not stream: that canvas's stream with this destination alone (protocol 1.77).
    fn stream_to(&mut self, c: OutputCanvas, only: Option<u32>) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        host.note_encoders(notify);
        if let Some(why) = &host.cannot_record {
            return Err(failed(why.clone()));
        }
        if host.outs[slot(c)].streaming.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a stream is already running",
            ));
        }
        let (request, hidden) = host.api_stream_request(c, only)?;
        let hide = |why: String| {
            failed(
                hidden
                    .iter()
                    .fold(why, |w, url| w.replace(url.as_str(), "the saved server")),
            )
        };
        let result = host.start_stream_to(c, &request, only);
        // The texts name the stream, or the destination, whose key is missing.
        let which = match (only, c) {
            (Some(id), _) if id != MAIN_FIRST && id != VERTICAL_FIRST => " for this destination",
            (_, OutputCanvas::Main) => "",
            (_, OutputCanvas::Vertical) => " for the vertical stream",
        };
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
        SourceKind::Text => api::SourceKind::Text,
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
        api::SourceKind::Text => SourceKind::Text,
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

/// The API form of a scroll (ADR-0150).
fn api_scroll(s: &Scroll) -> api::ItemScroll {
    api::ItemScroll {
        direction: match s.direction {
            ScrollDirection::Right => api::ScrollDirection::Right,
            ScrollDirection::Up => api::ScrollDirection::Up,
            ScrollDirection::Down => api::ScrollDirection::Down,
            _ => api::ScrollDirection::Left,
        },
        speed: s.speed,
        gap: s.gap,
        repeat: match s.repeat {
            ScrollRepeat::Once => api::ScrollRepeat::Once,
            _ => api::ScrollRepeat::Loop,
        },
        start: match s.start {
            ScrollStart::Outside => api::ScrollStart::Outside,
            _ => api::ScrollStart::Inside,
        },
        fade: s.fade,
        ramp_ms: s.ramp_ms,
    }
}

/// The scene's form of an API scroll; the error says what is out of range.
fn scene_scroll(s: &api::ItemScroll) -> Result<Scroll, String> {
    let direction = match s.direction {
        api::ScrollDirection::Left => ScrollDirection::Left,
        api::ScrollDirection::Right => ScrollDirection::Right,
        api::ScrollDirection::Up => ScrollDirection::Up,
        api::ScrollDirection::Down => ScrollDirection::Down,
    };
    let scroll = Scroll::new(direction, s.speed)
        .with_gap(s.gap)
        .with_repeat(match s.repeat {
            api::ScrollRepeat::Loop => ScrollRepeat::Loop,
            api::ScrollRepeat::Once => ScrollRepeat::Once,
        })
        .with_start(match s.start {
            api::ScrollStart::Inside => ScrollStart::Inside,
            api::ScrollStart::Outside => ScrollStart::Outside,
        })
        .with_fade(s.fade)
        .with_ramp_ms(s.ramp_ms);
    if scroll.is_valid() {
        Ok(scroll)
    } else {
        Err(SCROLL_ERROR.into())
    }
}

/// Why a scroll is refused.
const SCROLL_ERROR: &str =
    "Scroll speed must be 1 to 4000, gap 0 to 16384, edge fade 0 to 2048 and ramp 0 to 5000 ms.";

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

fn api_track_role(role: rbs_record::IsoRole) -> api::TrackRole {
    match role {
        rbs_record::IsoRole::Audio => api::TrackRole::Audio,
        rbs_record::IsoRole::Image => api::TrackRole::Image,
        _ => api::TrackRole::Video,
    }
}

fn api_picture_form(form: VideoAs) -> api::PictureForm {
    match form {
        VideoAs::Video => api::PictureForm::Video,
        VideoAs::Image => api::PictureForm::Image,
        _ => api::PictureForm::Auto,
    }
}

/// Nanoseconds as milliseconds, to the microsecond (control API 1.77).
fn ns_to_ms(ns: i64) -> f64 {
    (ns as f64 / 1_000.0).round() / 1_000.0
}

/// `info` with the counters of its source's audio input, when one runs (`source.list`,
/// protocol 1.77, ADR-0130 decision 9).
fn with_input_counters(studio: &Studio, mut info: api::SourceInfo) -> api::SourceInfo {
    if let Some(c) = studio.input_counters(SourceId::from_raw(info.id)) {
        info.input_late_frames = Some(c.late_frames);
        info.input_resyncs = Some(c.resyncs);
    }
    info
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
            scroll: l.scroll.map(|s| api_scroll(&s)),
        }),
        picture_size: row.picture.map(|(w, h)| [w, h]),
        visible: row.visible,
        locked: row.locked,
        volume: row.audio.volume,
        muted: row.audio.muted,
        mixer_hidden: row.audio.mixer_hidden,
        monitor: monitor::api_mode(row.audio.monitor),
        companion_of: row.companion_of.map(|c| c.get()),
        // 1.46's meaning: the main feed (a picture source's picture, anything
        // else's sound), so an older client's switch shows and clears that feed.
        record_separately: if row
            .kind
            .is_some_and(|k| !k.is_audio() && k != SourceKind::Media)
        {
            row.record_separately.video
        } else {
            row.record_separately.audio
        },
        record_tracks: (!row.record_separately.is_none()).then(|| api::RecordTracks {
            video: row.record_separately.video,
            audio: row.record_separately.audio,
            video_as: api_picture_form(row.record_separately.video_as),
        }),
        filters: row.filters.iter().map(filters::api_row).collect(),
        sync_offset_ms: row.audio.sync_offset_ms,
        glide: row.glide.map(api_glide_source),
        heard: row.heard.map(|h| api::Heard {
            volume: h.volume,
            muted: h.muted,
        }),
        filter_latency_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.filter_latency_ns)),
        effective_offset_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.heard_offset_ns)),
        uncompensated_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.uncompensated_ns)),
        // Growing counters: only `source.list` reads them (`with_input_counters`), so
        // a row's change event is not sent on every block.
        input_late_frames: None,
        input_resyncs: None,
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

/// A one-off transition of `canvas.transition` or `scene.switch` (ADR-0128 decision
/// 13), refused when its duration is out of range.
fn transition_once(p: rbs_api::request::TransitionOnce) -> Result<TransitionOnce, Refusal> {
    if let Some(ms) = p.duration_ms
        && ms > rbs_scene::MAX_GLIDE_MS
    {
        return Err(Refusal::Invalid(format!(
            "duration_ms must be at most {}.",
            rbs_scene::MAX_GLIDE_MS
        )));
    }
    Ok(TransitionOnce::new(
        p.kind.map(scene_transition_kind),
        p.duration_ms,
    ))
}

fn scene_transition_kind(k: api::TransitionKind) -> rbs_scene::TransitionKind {
    match k {
        api::TransitionKind::Cut => rbs_scene::TransitionKind::Cut,
        api::TransitionKind::Fade => rbs_scene::TransitionKind::Fade,
        api::TransitionKind::Move => rbs_scene::TransitionKind::Move,
        api::TransitionKind::Swipe => rbs_scene::TransitionKind::Swipe,
        api::TransitionKind::Slide => rbs_scene::TransitionKind::Slide,
        api::TransitionKind::Stinger => rbs_scene::TransitionKind::Stinger,
    }
}

fn api_transition_kind(k: rbs_scene::TransitionKind) -> api::TransitionKind {
    match k {
        rbs_scene::TransitionKind::Cut => api::TransitionKind::Cut,
        rbs_scene::TransitionKind::Fade => api::TransitionKind::Fade,
        rbs_scene::TransitionKind::Swipe => api::TransitionKind::Swipe,
        rbs_scene::TransitionKind::Slide => api::TransitionKind::Slide,
        rbs_scene::TransitionKind::Stinger => api::TransitionKind::Stinger,
        _ => api::TransitionKind::Move,
    }
}

fn scene_easing(e: api::GlideEasing) -> rbs_scene::GlideEasing {
    match e {
        api::GlideEasing::Linear => rbs_scene::GlideEasing::Linear,
        api::GlideEasing::EaseInOut => rbs_scene::GlideEasing::EaseInOut,
    }
}

fn scene_swipe(s: api::SwipeTransition) -> rbs_scene::SwipeSettings {
    rbs_scene::SwipeSettings {
        duration_ms: s.duration_ms,
        easing: scene_easing(s.easing),
        direction: match s.direction {
            api::SwipeDirection::Left => rbs_scene::SwipeDirection::Left,
            api::SwipeDirection::Right => rbs_scene::SwipeDirection::Right,
            api::SwipeDirection::Up => rbs_scene::SwipeDirection::Up,
            api::SwipeDirection::Down => rbs_scene::SwipeDirection::Down,
        },
    }
}

fn api_swipe(s: rbs_scene::SwipeSettings) -> api::SwipeTransition {
    api::SwipeTransition {
        duration_ms: s.duration_ms,
        easing: api_easing(s.easing),
        direction: match s.direction {
            rbs_scene::SwipeDirection::Right => api::SwipeDirection::Right,
            rbs_scene::SwipeDirection::Up => api::SwipeDirection::Up,
            rbs_scene::SwipeDirection::Down => api::SwipeDirection::Down,
            _ => api::SwipeDirection::Left,
        },
    }
}

/// The collection's transition as `scene.list` shows it (protocol 1.81; the stinger
/// since 1.89).
fn api_transition(
    t: rbs_scene::CollectionTransition,
    cut_found_ms: Option<u32>,
) -> api::Transition {
    api::Transition {
        kind: api_transition_kind(t.kind),
        direct_kind: api_transition_kind(t.direct),
        fade: api::FadeTransition {
            duration_ms: t.fade.duration_ms,
            easing: api_easing(t.fade.easing),
            via: t
                .fade
                .via
                .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")),
        },
        swipe: api_swipe(t.swipe),
        slide: api_swipe(t.slide),
        stinger: api::StingerTransition {
            source: t.stinger.source.map(SourceId::get),
            cut_ms: t.stinger.cut_ms,
            cut_found_ms: t.stinger.source.and(cut_found_ms),
        },
    }
}

/// `#rrggbb` as bytes.
fn parse_rgb(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
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
                own_audio: s.own_audio,
                transition_in: s.transition_in.map(|t| api::SceneTransition {
                    kind: t.kind.map(api_transition_kind),
                    duration_ms: t.duration_ms,
                }),
            })
            .collect(),
        program: list.program.get(),
        preview: list.preview.map(SceneId::get),
        master: api::Master {
            volume: list.master.volume,
            muted: list.master.muted,
        },
        glide: api_glide(list.glide),
        transition: api_transition(list.transition, list.stinger_cut_found_ms),
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
