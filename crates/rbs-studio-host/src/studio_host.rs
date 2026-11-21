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
    Shape, Source, SourceAudio, SourceId, SourceRegion, VideoAs,
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
pub use command_path::{CommandPath, WindowReason};
use control_api::*;
use recording_markers::*;
pub use refusal::Refusal;
pub use rows::*;
pub use updates::*;
mod canvas;
mod command;
mod command_path;
mod control_loop;
mod cues;
mod history;
mod items;
mod outputs;
mod pickers;
mod projector;
#[cfg(test)]
mod publish_cost;
mod recording;
mod recording_markers;
mod refusal;
mod rows;
mod scenes;
mod source_settings;
mod sources;
mod start;
mod stream;
mod support;
mod text_resize;
mod updates;
pub use support::*;
mod handle;
pub use handle::*;
mod config;
pub use config::*;
mod views;
pub use views::*;
mod output_state;
pub use output_state::*;
mod crop;
pub use crop::*;
mod targets;
pub use targets::*;
mod held;
use control_loop::*;
use held::*;
use start::*;

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
    crate::browser_profiles::new_id().map_err(|e| Refusal::Invalid(e.to_string()))
}

/// A new browser page's size on a `w` x `h` canvas: the canvas, at most 1920x1080,
/// even ("Use canvas size" on the Page tab sets it).
pub fn attributes_browser_size(w: u32, h: u32) -> (i64, i64) {
    attributes::browser_size(w, h)
}

/// Whether the app shows the preview scene's own picture in studio mode (ADR-0061):
/// macOS, where each picture is its own Core Animation layer, and Windows, where both
/// pictures share the window's one DirectComposition target.
const SCENE_PICTURE: bool = cfg!(any(target_os = "macos", windows));

const STUDIO_MODE_OFF: &str = "Studio mode is off: there is no preview scene.";
const NO_AREA: &str = "The crop must leave at least one pixel of the picture in each direction.";

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
    /// The Show cues sent last ([`Update::Cues`]).
    cues_sent: Vec<(ItemId, u32)>,
    /// The source pickers' state: thumbnail and region grabs, and the permission prompts.
    pickers: pickers::Pickers,
    /// [`Host::save_stamp`] as last handed to `autosave` or loaded (epoch 0 at start).
    saved: Option<(u64, u64)>,
    studio: Studio,
    adapters: RecordAdapters,
    collection: SceneCollection,
    /// Bumped when the whole collection value is replaced. Together with
    /// [`SceneCollection::generation`] it says whether source rows must be rebuilt.
    collection_epoch: u64,
    /// The last [`Self::compute_list_stamp`] and the collection's level edits the
    /// rows showed then (F-10). `None` until the first publish.
    list_stamp: Option<(u64, rbs_scene::LevelEdits)>,
    canvas: CanvasId,
    /// The canvas's program scene: the one the Sources panel and the source commands
    /// edit (kept in step by [`Host::keep_program`]).
    scene: SceneId,
    rows: Vec<SourceRow>,
    /// The program scene's rows last published (the Mixer and `source.changed`).
    heard: Vec<SourceRow>,
    /// The global sources last published to the Mixer (ADR-0065).
    globals_shown: Vec<SourceRow>,
    /// The program and its changes: the held copy, the transition and the T-bar.
    program: held::ProgramState,
    /// The preview scene's picture endpoint last sent to the UI.
    scene_preview_sent: Option<PreviewEndpoint>,
    /// The vertical canvas: its state, flags, shared scenes and picture.
    verticals: outputs::VerticalHost,
    /// Open and suspended projectors (ADR-0125 decision 3). Empty: nothing of them.
    projectors: Vec<ProjectorRun>,
    /// Open Interact windows (ADR-0132 decision 6). Empty: nothing of them.
    interacts: Vec<interact::InteractRun>,
    /// The scene list as last sent.
    scenes_shown: Option<SceneList>,
    /// `scene.list` for the snapshot last published or last polled. See
    /// [`scene_cache`].
    scene_answer: scene_cache::SceneAnswerCache,
    /// The recording's settings: the profiles, separate files, the limit policy and the encoders.
    recording: recording::RecordingState,
    /// The master meter last sent to the UI.
    master_sent: Option<Arc<rbs_audio::LevelMeter>>,
    /// Meters published for `audio.levels`.
    levels: Arc<crate::levels::LevelFeed>,
    /// The last picture size seen per video source (ADR-0058).
    sizes: BTreeMap<SourceId, (u32, u32)>,
    /// What Copy attributes took (ADR-0133 decision 6); `None` costs nothing.
    attribute_clipboard: Option<attributes::AttributeSnapshot>,
    /// The undo history and the steps the window shows.
    edits: history::EditHistory,
    waiting: bool,
    /// Live text: timers, elapsed clocks, crisp re-renders and fonts asked for.
    texts: text_live::TextState,
    /// The recording and stream of each canvas, by [`slot`]
    /// (ADR-0093 decision 6).
    outs: [CanvasOutputs; 2],
    /// The stream's settings: the profiles, the destinations and the key store.
    streaming: stream::StreamState,
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
    #[cfg(feature = "fault-injection")]
    api_trace: Option<ApiTrace>,
    /// Counts `seq` per state key and publishes events to control API clients.
    events: EventPublisher,
    /// The Mixer panel is shown. Combined with an `audio.levels` subscription.
    meters_ui: bool,
    /// What the mixer was last told.
    meters_applied: bool,
    /// The control API, while remote control is on.
    api: Option<Api>,
    /// Kinds the adapters can add.
    kinds: Vec<SourceKind>,
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
    /// The virtual camera: its factory, settings, setup and what was published.
    vcam: outputs::CameraState,
    /// The monitor output: its saved device and volume, when its device opens, what
    /// the API saw last (ADR-0097).
    monitor: monitor::MonitorHost,
    /// The loudness meter: shown or not, its settings and what was sent.
    loudness_state: loudness::LoudnessState,
    /// The replay buffer's settings and state (ADR-0088).
    replay: replay::ReplayHost,
    /// Recording protection while an output runs (ADR-0090).
    protection: protection::ProtectionHost,
}

/// Capture region (has, l, t, r, b), chrome (on, t, r, b, l, fill), work area, and
/// timed show (ms) for a sources list row.
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
);

impl Host {
    /// Runs a control API command for the window (ADR-0163 decision 1): `ApiTarget::run`
    /// with panic containment and refusal logging, as a remote call is, without the
    /// caller checks of `handle_as` (decision 3).
    fn run_api(
        &mut self,
        command: rbs_api::Command,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let method = command.method();
        #[cfg(feature = "fault-injection")]
        if let Some(trace) = &self.api_trace {
            trace.lock().unwrap_or_else(|e| e.into_inner()).push(method);
        }
        let result = rbs_crash::contain(|| ApiTarget { host: self, notify }.run(command));
        let result = result.unwrap_or_else(|_| {
            self.caught = true;
            Err(ApiError::new(ErrorKind::Internal, rbs_crash::CAUGHT_TEXT))
        });
        if let Err(e) = &result {
            failures::log_api_refusal(method, e);
        }
        result
    }

    /// Runs a control API command for a window action that already has a method; a
    /// refusal shows as the error bar. The window and a remote client then share one
    /// implementation.
    fn run_ui_api(&mut self, command: rbs_api::Command, notify: &dyn Fn(Update)) -> bool {
        match self.run_api(command, notify) {
            Ok(_) => true,
            Err(e) => {
                notify(Update::Error(e.message));
                false
            }
        }
    }

    /// [`Command::Api`]: the call, answered as the window asked (ADR-0163 decision 2).
    fn api_command(&mut self, call: rbs_api::Command, reply: ApiReply, notify: &dyn Fn(Update)) {
        // A window call adds where the window edits: the preview scene in studio mode
        // (ADR-0061 decision 4), as its own commands did; a client's goes on air.
        let into = self.program.into.replace(self.edited());
        let result = self.run_api(call, notify);
        self.program.into = into;
        let shown = match (&result, reply) {
            (Ok(_), _) | (_, ApiReply::Quiet | ApiReply::QuietTicket(_)) => false,
            (Err(e), ApiReply::QuietIf(kind)) => e.kind != kind,
            (Err(_), ApiReply::Shown | ApiReply::Ticket(_)) => true,
        };
        if let (Err(e), true) = (&result, shown) {
            notify(Update::Error(e.message.clone()));
        }
        if let ApiReply::Ticket(ticket) | ApiReply::QuietTicket(ticket) = reply {
            notify(Update::ApiAnswer { ticket, result });
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
            | Command::AddGlobalSource { .. }
            | Command::UndoDelete
            | Command::MoveSourceTo { .. }
            | Command::MoveSceneTo { .. }
            | Command::SetSourceVolume { .. }
            | Command::SetSourceMuted { .. }
            | Command::SetMetersUi(..)
            | Command::EnsureEncoders
            | Command::SetSceneOwnLevels { .. }
            | Command::ResetSceneLevel { .. }
            | Command::SetSourceMixerHidden { .. } => self.source_command(c, notify),
            Command::AutoLayoutVertical(..)
            | Command::SetVerticalSize(..)
            | Command::SetMasterVolume { .. }
            | Command::SetMasterMuted { .. }
            | Command::SetLoudnessShown(..)
            | Command::SetLoudnessSettings { .. } => self.canvas_and_mix_command(c, notify),
            Command::AddVoiceChain(..)
            | Command::AddSample { .. }
            | Command::AddStamp { .. }
            | Command::AddImage { .. }
            | Command::AddMedia { .. }
            | Command::SetStingerFile(..)
            | Command::FitTextBox(..)
            | Command::AddBrowser { .. }
            | Command::SetSourceFilled { .. }
            | Command::ApplyLook { .. }
            | Command::SetFilterValues { .. }
            | Command::PreviewFilter { .. }
            | Command::RestoreFilter(..)
            | Command::OpenSyncMeasure(..)
            | Command::StartSyncMeasure(..)
            | Command::CloseSyncMeasure
            | Command::RetargetSource { .. }
            | Command::GrabThumbnails { .. }
            | Command::StopThumbnails
            | Command::OpenSourceSettings(..)
            | Command::CloseSourceSettings
            | Command::SetSourceSettings { .. }
            | Command::ForgetBrowserSignIn(..)
            | Command::BrowserControl { .. } => self.source_edit_command(c, notify),
            Command::OpenAttributes { .. }
            | Command::SwitchScene(..)
            | Command::SetStudioMode(..)
            | Command::Rehearse
            | Command::SetSwapAfterTransition(..) => self.scene_command(c, notify),
            Command::SetSourceBox { .. }
            | Command::SetSourceCrop { .. }
            | Command::TransformSource { .. } => self.item_command(c, notify),
            Command::SetSourceRegion { .. }
            | Command::SetSourceStyle { .. }
            | Command::SetSourceScroll { .. } => self.item_display_command(c, notify),
            Command::StartRecording(..)
            | Command::StartVerticalRecording(..)
            | Command::SetVerticalProfile(..)
            | Command::SetVerticalStreamProfile(..)
            | Command::SetStreamDestinations(..)
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
            | Command::StartVirtualCamera { .. }
            | Command::StopVirtualCamera
            | Command::SetReplay(..)
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
            Command::Api { call, reply } => self.api_command(*call, reply, notify),
        }
    }

    fn post(&self, m: Msg) {
        if let Some(tx) = self.inbox.upgrade() {
            let _ = tx.send(m);
        }
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
                let v = self.verticals.vertical.as_ref()?;
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
        self.mark_scene_changes();
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        let program = running
            .canvas(self.canvas)
            .and_then(|c| c.program)
            .map(|id| {
                let real = match &self.program.held {
                    Some(h) if h.scene.id == id => h.from,
                    _ => id,
                };
                (id, real)
            });
        let same = same_picture(self.program.ran_program, program);
        self.program.ran_program = program;
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
            } else if self.verticals.vertical.map(|v| v.canvas) == Some(e.canvas) {
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
        if self.verticals.vertical_sent != Some(self.verticals.vertical) {
            self.verticals.vertical_sent = Some(self.verticals.vertical);
            notify(Update::Vertical(self.verticals.vertical));
        }
        if let Some(why) = self.verticals.vertical_restore_error.take() {
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
        if self.loudness_state.loudness_sent.as_ref() != Some(&loudness) {
            self.loudness_state.loudness_sent = Some(loudness.clone());
            notify(Update::Loudness(loudness));
        }
        let stamp = self.compute_list_stamp();
        let levels = self.collection.level_edits();
        match self.list_stamp.replace((stamp, levels)) {
            Some((was, seen)) if was == stamp => self.patch_levels(seen, levels, notify),
            _ => {
                self.publish_scene_list(notify);
                self.publish_rows(notify);
            }
        }
        self.publish_cues(notify);
        let picture = self.studio.scene_preview();
        if picture != self.scene_preview_sent {
            self.scene_preview_sent = picture.clone();
            notify(Update::ScenePreview(picture));
        }
        let vertical = self
            .verticals
            .vertical
            .and_then(|v| self.studio.canvas_preview(v.canvas));
        if vertical != self.verticals.vertical_preview_sent {
            self.verticals.vertical_preview_sent = vertical.clone();
            notify(Update::VerticalPreview(vertical));
        }
        // After the rows, so the UI sees the edit before what undoes it.
        let shown = (
            self.edits.history.next_undo().cloned(),
            self.edits.history.next_redo().cloned(),
        );
        if shown != self.edits.history_shown {
            self.edits.history_shown = shown.clone();
            notify(Update::History {
                undo: shown.0,
                redo: shown.1,
            });
        }
    }

    fn note_collection(&mut self) {
        self.collection_epoch = self.collection_epoch.wrapping_add(1);
    }

    /// Whether the scene list and the source rows still match what was published.
    /// Cue time is not included: it travels in [`Update::Cues`] (F-10). Document edits
    /// bump [`SceneCollection::generation`]; replacing the collection bumps the epoch.
    fn compute_list_stamp(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.collection_epoch.hash(&mut hasher);
        // Live level edits are left out: `publish` patches the rows they move.
        self.collection
            .level_edits()
            .others(self.collection.generation())
            .hash(&mut hasher);
        self.scene.hash(&mut hasher);
        self.preview().hash(&mut hasher);
        self.edited().hash(&mut hasher);
        self.verticals.vertical.map(|v| v.canvas).hash(&mut hasher);
        self.stinger.cut_found_ms().hash(&mut hasher);
        self.stinger.looking().hash(&mut hasher);
        self.stinger.alpha_unsupported().hash(&mut hasher);
        self.stinger_length()
            .map(|l| l.as_millis() as u64)
            .hash(&mut hasher);
        // Borrowed, so an unchanged round allocates nothing (F-10).
        for health in self.studio.source_health() {
            health.source.hash(&mut hasher);
            health.failed.hash(&mut hasher);
            health.fault.map(|f| f as u8).hash(&mut hasher);
            health.reason.hash(&mut hasher);
        }
        for source in self.collection.sources() {
            source.id.hash(&mut hasher);
            // With the reason above, all `seams::page_reason` reads.
            self.studio
                .browser_gpu_fell_back(source.id)
                .hash(&mut hasher);
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
            // Device peers come from the collection alone: its generation covers them.
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
                self.scroll_paused_of(item).hash(&mut hasher);
            }
        }
        hasher.finish()
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
                ) = self.source_row_extra(s, item.id);
                SourceRow {
                    media: MediaSettings::of(s),
                    activity: ActivityChoice::of(s, self.studio.warm_up(s.id)),
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

#[cfg(test)]
mod tests;
