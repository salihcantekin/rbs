//! Requests and responses of protocol version 1 (first slice, control API spec "Initial
//! method set"). [`Request`] is everything a client may send; [`Command`] is the part
//! the session forwards to the control thread's [`ControlTarget`](crate::ControlTarget).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::Topic;
use crate::scope::{Scope, ScopeSet};
use crate::types::{
    CalibrationState, CanvasRecordState, CanvasStreamState, CaptureTarget, EstimateAnswer,
    EstimateSummary, FilterTypeList, Library, LoudnessStatus, MonitorDeviceList, MonitorMode,
    MonitorStatus, OutputCanvas, OutputKind, OutputProfile, OutputTarget, OutputsResult,
    ProtectionStatus, RecordState, ReplayStatus, SceneList, SeqMap, Settings, SkippedTrack,
    Snapshot, SourceInfo, SourceKind, StreamState, StudioInfo, VcamStatus,
};
use crate::types::{DeviceKey, DeviceSyncEntry};
use crate::types::{ItemScroll, ItemStyle};

mod add_source;
mod added;
mod arrange;
mod browser;
mod groups;
mod item_edits;
mod media;
mod methods;
mod scene_refs;
mod settings_patch;
mod transform;
mod wire;
pub use add_source::AddSource;
pub use added::*;
pub use arrange::*;
pub use browser::*;
pub use groups::*;
pub use item_edits::*;
pub use media::{MediaPlayState, MediaSeek, MediaSource, MediaStateInfo};
pub use methods::{METHODS, UI_METHODS};
pub use scene_refs::*;
pub use settings_patch::SettingsPatch;
pub use transform::*;
use wire::WireRequest;

/// Parameters of a method that takes none. Unknown fields are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

/// `session.hello`: the protocol versions the client speaks, and who it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// `major.minor` versions, for example `["1.0"]`.
    pub protocols: Vec<String>,
    /// The client.
    pub client: ClientInfo,
}

/// A client's or server's name and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
}

impl ClientInfo {
    /// A name and version.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// `session.authenticate`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authenticate {
    /// The token's text.
    pub token: String,
}

impl std::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authenticate { token: <redacted> }")
    }
}

/// `source.targets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetsParams {
    /// The kind whose capture targets to list.
    pub kind: SourceKind,
}

/// `source.set_text` (protocol 1.75, ADR-0129 decision 8): replaces a text source's
/// `text` only. One undo step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetText {
    /// The text source's id.
    pub source: u32,
    /// The new text.
    pub text: String,
}

/// What `text.timer` does (protocol 1.84, ADR-0129 decision 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerAction {
    /// Runs it from its value; a countdown at zero starts over.
    Start,
    /// Stops it at its value.
    Pause,
    /// Back to zero, stopped.
    Reset,
}

/// `text.timer` (protocol 1.84, ADR-0129 decision 11): starts, pauses or resets a
/// countdown (by duration) or stopwatch text source. A live action: not an undo step
/// and not in the scene collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextTimer {
    /// The text source's id.
    pub source: u32,
    /// What to do.
    pub action: TimerAction,
}

/// `source.set_companion_audio` (protocol 1.26, ADR-0096): adds or removes the "Capture
/// audio too" companion of a display or window source. One undo step each way. Adding
/// one that exists, or removing one that does not, changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCompanionAudio {
    /// The picture source's id.
    pub source: u32,
    /// Whether it has a companion sound.
    pub enabled: bool,
}

/// `collection.set_glide` (protocol 1.35, ADR-0100): collection morph defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCollectionGlide {
    /// Milliseconds in `0..=5000`. `0` jumps.
    pub duration_ms: u32,
    /// Slide easing (`"linear"` or `"ease-in-out"`).
    pub easing: crate::types::GlideEasing,
}

/// `collection.set_transition` (protocol 1.81, ADR-0128): the collection's transition.
/// Only the given parts change; one undo step.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCollectionTransition {
    /// The kind a program change uses in studio mode (in every mode before 1.97).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::types::TransitionKind>,
    /// The kind a program change uses outside studio mode (protocol 1.97, ADR-0153).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_kind: Option<crate::types::TransitionKind>,
    /// Fade settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<crate::types::FadeTransition>,
    /// Swipe settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swipe: Option<crate::types::SwipeTransition>,
    /// Slide settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slide: Option<crate::types::SwipeTransition>,
    /// Stinger settings, replaced whole (protocol 1.89, ADR-0140 decision 9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stinger: Option<crate::types::StingerTransition>,
}

/// The answer to `collection.set_transition` (protocol 1.89): empty, or a warning about
/// the stinger (ADR-0140 decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TransitionSet {
    /// The stinger's file has transparency no decoder on this computer keeps, so
    /// each Stinger change runs as a Cut; absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alpha_unsupported: bool,
}

/// A transition for one program change only (protocol 1.81, ADR-0128 decision 13):
/// `kind` and `duration_ms` replace the collection's for this change; absent ones keep
/// it. With kind `move`, `duration_ms` replaces the glide's duration and every
/// per-source override for this change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionOnce {
    /// The kind for this change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::types::TransitionKind>,
    /// Milliseconds in `0..=5000` for this change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
}

/// `scene.switch` (protocol 1.6; `kind` and `duration_ms` since 1.81).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchScene {
    /// The scene's id (`scene.list`).
    pub scene: u32,
    /// The kind for this change only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::types::TransitionKind>,
    /// Milliseconds for this change only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
}

impl SwitchScene {
    /// The one-off transition of this switch.
    pub fn once(&self) -> TransitionOnce {
        TransitionOnce {
            kind: self.kind,
            duration_ms: self.duration_ms,
        }
    }
}

/// `source.set_glide` (protocol 1.35, ADR-0100): per-source slide override, or `null`
/// to clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceGlide {
    /// The source's id.
    pub source: u32,
    /// Override, or `null` to use the collection defaults.
    pub glide: Option<crate::types::Glide>,
}

/// `source.set_activity` (protocol 1.95, ADR-0152): replaces a source's activity
/// policy. A key left out (or `null`) is the type's default, so `{source}` alone
/// resets it. One undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceActivity {
    /// The source's id.
    pub source: u32,
    /// When it runs; absent is the type's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<crate::types::ActivityRun>,
    /// What it does when it goes on air again; absent is the type's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_return: Option<crate::types::ActivityReturn>,
    /// Seconds it stays Active after it stops being needed (`0..=600`); absent is 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger_s: Option<u16>,
}

/// The policy in force for a source (protocol 1.95, ADR-0152).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveActivity {
    /// When it runs.
    pub run: crate::types::ActivityRun,
    /// What it does when it comes back; absent for types without the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_return: Option<crate::types::ActivityReturn>,
    /// Seconds it lingers.
    pub linger_s: u16,
}

/// `source.get_activity`'s answer (protocol 1.95, ADR-0152).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceActivityInfo {
    /// The source's id.
    pub source: u32,
    /// The saved choice; each key absent is the type's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<crate::types::ActivityRun>,
    /// The saved choice for the return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_return: Option<crate::types::ActivityReturn>,
    /// The saved linger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger_s: Option<u16>,
    /// The policy in force: the saved choice merged with the type's defaults, or the
    /// defaults alone when `supported` is false.
    pub effective: EffectiveActivity,
    /// False when the saved choice uses a value the type does not allow (the
    /// `activity_unsupported` warning).
    pub supported: bool,
    /// The `run` values the source's type allows.
    pub runs: Vec<crate::types::ActivityRun>,
    /// The `on_return` values the type allows; empty when it has none.
    pub on_returns: Vec<crate::types::ActivityReturn>,
}

/// How a display's work area is placed on the canvas (protocol 1.41, ADR-0104).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChromeFill {
    /// Cover the canvas (layout fill).
    #[default]
    Cover,
    /// Fit inside the canvas (layout fit).
    Fit,
}

impl ChromeFill {
    /// True for [`ChromeFill::Cover`], which documents and answers omit.
    pub fn is_cover(&self) -> bool {
        *self == Self::Cover
    }
}

/// Display chrome insets in capture pixels (protocol 1.41, ADR-0104).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromeSpec {
    /// Pixels cut from the top.
    #[serde(default)]
    pub top: u32,
    /// Pixels cut from the right.
    #[serde(default)]
    pub right: u32,
    /// Pixels cut from the bottom.
    #[serde(default)]
    pub bottom: u32,
    /// Pixels cut from the left.
    #[serde(default)]
    pub left: u32,
    /// `"cover"` (default) or `"fit"`. Not part of the stored region.
    #[serde(default, skip_serializing_if = "ChromeFill::is_cover")]
    pub fill: ChromeFill,
}

/// `source.set_region` (protocol 1.41, ADR-0103). One undo step, distinct from
/// `item.set_crop`. `null` clears the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceRegion {
    /// The source's id.
    pub source: u32,
    /// `[left, top, right, bottom]` in full-picture pixels, or `null`.
    pub region: Option<[u32; 4]>,
}

/// `source.set_record_separately` (protocol 1.46, ADR-0086; `video`, `audio` and
/// `video_as` 1.52, ADR-0117). One undo step. A patch: an absent field keeps the
/// source's value. The per-track options of ADR-0086 decision 2 are later minors, so
/// they are refused as unknown fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRecordSeparately {
    /// The source's id.
    pub source: u32,
    /// Record its main feed as a file of its own from the next recording on: the
    /// sound of a sound source or of a media file (as in 1.46), the picture of a
    /// picture source (1.52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Record its picture separately (1.52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    /// Record its sound separately (1.52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    /// The picture's form (1.52, ADR-0117): `image` only for a still source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_as: Option<VideoAs>,
}

/// The form of a separate picture file (protocol 1.52, ADR-0117 decision 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoAs {
    /// Images for a still source, a video file otherwise.
    #[default]
    Auto,
    /// A video file.
    Video,
    /// PNG images, one per change; a still source only.
    Image,
}

/// A marker colour (protocol 1.50, ADR-0089 decision 2): a fixed, semantic set that
/// mirrors the recording manifest's. The app draws each from a theme token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerColor {
    /// The default.
    #[default]
    Blue,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Purple.
    Purple,
}

/// `marker.add` (protocol 1.50, ADR-0089): a manual marker in the running
/// recordings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerAdd {
    /// One line of at most 200 characters; absent or blank is "Marker N".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Absent is `blue`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<MarkerColor>,
    /// `main`, `vertical` or `all`; absent is `all` (every running recording).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputTarget>,
}

/// `marker.remove` (protocol 1.50, ADR-0089): takes one marker back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerRemove {
    /// The recording's canvas.
    pub canvas: OutputCanvas,
    /// The marker's `index` from `marker.add`.
    pub index: u32,
}

/// One marker `marker.add` placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerPlaced {
    /// The recording's canvas.
    pub canvas: OutputCanvas,
    /// 1-based, in the order the recording's markers were added.
    pub index: u32,
    /// Position from the recording's start (`t0`), after the reaction-time offset, in
    /// nanoseconds. File time: the pauses before it are left out (protocol 1.63).
    pub t_ns: u64,
    /// Added while the recording was paused: the marker sits on that pause's join
    /// (protocol 1.63, ADR-0124 decision 10). Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub while_paused: bool,
}

/// `marker.add`'s answer: one entry per recording that got the marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkersAdded {
    /// Main canvas first.
    pub markers: Vec<MarkerPlaced>,
}

/// `project.export` (protocol 1.58, ADR-0089 decisions 8 and 11): an editor project
/// from a finished recording's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectExport {
    /// The recording's folder name, or a plain recording's file name, inside the
    /// profile's recordings folder: one path segment.
    pub recording: String,
    /// The project format.
    pub format: crate::types::ProjectFormat,
}

/// `project.export`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectExported {
    /// The project's file name, next to the manifest (never an absolute path).
    pub file: String,
    /// Separate files the manifest names that were not in the folder; left out.
    pub missing: Vec<String>,
}

/// `source.get_region`'s answer (protocol 1.41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRegionInfo {
    /// The source's id.
    pub source: u32,
    /// The region, or `null` for the whole picture.
    pub region: Option<[u32; 4]>,
}

/// `scene.audio.reset_source` (protocol 1.48, ADR-0091): a scene and one of the
/// sources it may set a level for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSourceRef {
    /// The scene's id.
    pub scene: u32,
    /// The source's id.
    pub source: u32,
}

/// `scene.audio.set` (protocol 1.48, ADR-0091): the scene's own level for a source.
/// At least one of `volume` and `muted`; a field left out keeps what the scene has.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSceneLevel {
    /// The scene's id.
    pub scene: u32,
    /// The source's id.
    pub source: u32,
    /// A linear factor from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Muted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// One entry of a scene's own levels (protocol 1.48, ADR-0091); a field left out uses
/// the source's own value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SceneLevelInfo {
    /// The source's id.
    pub source: u32,
    /// The scene's volume for it, a linear factor from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// The scene's mute for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// `scene.audio.get`'s answer (protocol 1.48, ADR-0091).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneLevels {
    /// The scene's id.
    pub scene: u32,
    /// Own levels (`true`) or Shared levels.
    pub own: bool,
    /// Its entries by source id; empty in Shared levels.
    #[serde(default)]
    pub levels: Vec<SceneLevelInfo>,
}

/// `scene.audio.set`'s answer (protocol 1.48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneLevelSet {
    /// The scene was in Shared levels and this call turned it to Own levels (one undo
    /// step, copying what was heard).
    #[serde(default)]
    pub turned_own: bool,
}

/// `source.set_chrome` (protocol 1.41, ADR-0104). Writes the insets and the region
/// they describe. Does not move the item box. `null` clears chrome and the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceChrome {
    /// The source's id.
    pub source: u32,
    /// The insets, or `null` to turn chrome off.
    pub chrome: Option<ChromeSpec>,
}

/// `source.get_chrome`'s answer (protocol 1.41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceChromeInfo {
    /// The source's id.
    pub source: u32,
    /// The chrome, or `null` when it is off.
    pub chrome: Option<ChromeSpec>,
}

/// `source.delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The source's id.
    pub source: u32,
}

/// `item.set_style` (protocol 1.3, ADR-0053): replaces the whole style of the item's
/// layout on the studio's canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetItemStyle {
    /// The item's id (`source.list` gives each video source's program-scene item).
    pub item: u32,
    /// The new style.
    pub style: ItemStyle,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.set_crop` (protocol 1.5, ADR-0058): the source pixels cut from each edge of
/// an item's picture, on the item's layout on the studio's canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemCrop {
    /// The item's id (`source.list` gives each video source's program-scene item).
    pub item: u32,
    /// Pixels cut from the left, top, right and bottom.
    pub crop: [u32; 4],
    /// Keep the size of a source pixel on the canvas and the place of what stays (the
    /// box changes); `false` sets only the crop, and what stays fills the box. Default
    /// `true`.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub keep_scale: bool,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_true(v: &bool) -> bool {
    *v
}

/// `scene.add` (protocol 1.6): an empty shared scene, not on air.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddScene {
    /// Its name; `null` lets the server pick "Scene", "Scene 2", ...
    #[serde(default)]
    pub name: Option<String>,
}

/// `scene.duplicate` (protocol 1.43; `sources` from 1.68, ADR-0133 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicateScene {
    /// The scene's id (`scene.list`).
    pub scene: u32,
    /// `shared` (default) or `copied`.
    #[serde(default)]
    pub sources: crate::types::DuplicateSources,
}

/// `source.duplicate` and `source.copy_attributes` (protocol 1.68, ADR-0133): a source,
/// its item in `scene` (default: the scene being edited) or its global item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInScene {
    /// The source's id.
    pub source: u32,
    /// The scene; absent for the scene being edited. Not allowed for a global source.
    #[serde(default)]
    pub scene: Option<u32>,
    /// The canvas whose layout is read (`source.copy_attributes` only).
    #[serde(default)]
    pub canvas: OutputCanvas,
}

/// `source.reset` (protocol 1.68, ADR-0133 decision 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetSource {
    /// The source's id.
    pub source: u32,
    /// The scene; absent for the scene being edited.
    #[serde(default)]
    pub scene: Option<u32>,
    /// The canvas whose layout is written.
    #[serde(default)]
    pub canvas: OutputCanvas,
    /// Group ids; absent means every non-live group.
    #[serde(default)]
    pub groups: Option<Vec<String>>,
}

/// `source.paste_attributes` (protocol 1.68, ADR-0133 decision 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasteAttributes {
    /// The target items, in any scene.
    pub items: Vec<u32>,
    /// Group ids to paste; not empty.
    pub groups: Vec<String>,
    /// `replace` (default) or `append`.
    #[serde(default)]
    pub filters: crate::types::FilterPasteMode,
    /// The canvas whose layouts are written.
    #[serde(default)]
    pub canvas: OutputCanvas,
}

/// `scene.set_transition` (protocol 1.86, ADR-0135 decision 10): sets or clears the
/// transition into a scene; one undo step. `null` or `{}` clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSceneTransition {
    /// The scene's id.
    pub scene: u32,
    /// The override, or `null` to clear it.
    #[serde(default)]
    pub transition: Option<crate::types::SceneTransition>,
}

/// `canvas.transition_progress` (protocol 1.87, ADR-0135 decision 1): a T-bar value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionProgress {
    /// The canvas; the studio's main canvas when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<crate::types::OutputCanvas>,
    /// The progress in `0.0..=1.0`: above 0 starts or moves a manual transition to
    /// the preview scene, 1 puts it on air, 0 cancels it.
    pub t: f32,
}

/// `source.rename` (protocol 1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameSource {
    /// The source's id.
    pub source: u32,
    /// The new name, unique among sources.
    pub name: String,
}

/// `source.set_volume` (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceVolume {
    /// The source's id.
    pub source: u32,
    /// A linear factor from 0 to 1.
    pub volume: f32,
}

/// `source.set_muted` (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceMuted {
    /// The source's id.
    pub source: u32,
    /// Muted.
    pub muted: bool,
}

/// The arrangement `canvas.add_vertical` writes for the new shape (protocol 1.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerticalLayout {
    /// Each item scaled to fit, letterboxed.
    Fit,
    /// Each item scaled to cover the canvas.
    Fill,
    /// The visible items in equal bands from top to bottom.
    Stack,
}

/// `canvas.add_vertical` (protocol 1.28, ADR-0093 decision 8): adds and runs the
/// second, vertical canvas. At most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddVerticalCanvas {
    /// Picture width in pixels (default 1080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Picture height in pixels (default 1920).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Shows the main canvas's program scene and switches with it (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<bool>,
    /// Arranges the program scene's sources for the new shape; absent: they keep the
    /// main canvas's layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<VerticalLayout>,
}

/// `canvas.auto_layout` (protocol 1.44, ADR-0093 decision 4): arranges a scene's
/// visible source items for the vertical canvas, one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasAutoLayout {
    /// The canvas to arrange for; only `"vertical"` in 1.44.
    pub canvas: OutputCanvas,
    /// The arrangement.
    pub mode: VerticalLayout,
    /// The scene; the one the vertical canvas shows now when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
}

/// `canvas.auto_layout`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoLayoutDone {
    /// The number of layouts written; 0 is not an error.
    pub items: u32,
}

/// `canvas.set_vertical_size` (protocol 1.44, ADR-0093 decision 8): the vertical
/// canvas's size in pixels, even and from 64 to 8192.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetVerticalSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// `canvas.set_master_volume` (protocol 1.15, ADR-0081): the studio canvas's master.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMasterVolume {
    /// A linear factor from 0 to 1.
    pub volume: f32,
}

/// `canvas.set_master_muted` (protocol 1.15, ADR-0081).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMasterMuted {
    /// Muted.
    pub muted: bool,
}

/// `source.set_monitor` (protocol 1.30, ADR-0097).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceMonitor {
    /// The source's id.
    pub source: u32,
    /// Off, only on the monitor, or also on the monitor.
    pub monitor: MonitorMode,
}

/// `monitor.set_device` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMonitorDevice {
    /// A `monitor.devices` id; `null`: System default.
    pub device: Option<String>,
}

/// `monitor.set_volume` (protocol 1.30).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMonitorVolume {
    /// Linear factor from 0 to 1.
    pub volume: f32,
}

/// `monitor.set_solo` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMonitorSolo {
    /// The soloed sources; empty ends solo.
    pub sources: Vec<u32>,
}

/// `loudness.set_enabled` (protocol 1.47, ADR-0092): starts or stops the main canvas's
/// loudness meter. Session state: a restart of the app starts with it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetLoudnessEnabled {
    /// Measure (true) or not.
    pub on: bool,
}

/// `source.set_mixer_hidden` (protocol 1.14, ADR-0080).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceMixerHidden {
    /// The source's id.
    pub source: u32,
    /// Hidden from the Mixer; still heard.
    pub hidden: bool,
}

/// Which filter types `filter.types` lists (protocol 1.16, ADR-0082).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterDomains {
    /// Video filters only: what a 1.8 to 1.15 server lists, so old clients never meet
    /// the audio kinds.
    #[default]
    Video,
    /// Audio filters only.
    Audio,
    /// Both.
    All,
}

/// `filter.types` (protocol 1.8; `domain` since 1.16).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterTypesParams {
    /// Which types to list; video when absent.
    #[serde(default)]
    pub domain: FilterDomains,
}

/// `filter.add` (protocol 1.8, ADR-0062): a filter at the end of a source's chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddFilter {
    /// The source's id.
    pub source: u32,
    /// The filter type, `org.rbs.filter.<name>` (see `filter.types`).
    #[serde(rename = "type")]
    pub type_id: String,
    /// Settings by key; missing keys take their defaults. Numbers are JSON numbers,
    /// colours `"#rrggbb"` or `"#rrggbbaa"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<BTreeMap<String, serde_json::Value>>,
}

/// `filter.remove` (protocol 1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterRef {
    /// The filter's id (unique in the collection).
    pub filter: u32,
}

/// `filter.move` (protocol 1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveFilter {
    /// The filter's id.
    pub filter: u32,
    /// Its new position in the chain, from 0.
    pub index: u32,
}

/// `filter.set_enabled` (protocol 1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFilterEnabled {
    /// The filter's id.
    pub filter: u32,
    /// Applied or not.
    pub enabled: bool,
}

/// `filter.set_settings` (protocol 1.8): a patch on the settings on air.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFilterSettings {
    /// The filter's id.
    pub filter: u32,
    /// The keys to replace; the others stay.
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// `source.retarget` (protocol 1.13, ADR-0077): point a source at another listed target
/// of its kind (listed from 1.100). Its name, layout, filters and places stay; one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetargetSource {
    /// The source's id.
    pub source: u32,
    /// The adapter that listed the target (`source.targets`).
    pub adapter: String,
    /// The target's id for that adapter.
    pub target: String,
}

/// `source.reload` (protocol 1.13, ADR-0077): try a source that is not running again with
/// its stored settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadSource {
    /// The source's id.
    pub source: u32,
}

/// `source.get_settings` (protocol 1.17, ADR-0077).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSourceSettings {
    /// The source's id.
    pub source: u32,
}

/// `source.set_settings` (protocol 1.17, ADR-0077): a patch of a source's stored settings.
/// A key set to `null` is removed; the result is validated for the source's type and the
/// change is one undo step. The source is opened again with the new settings (filters,
/// layout and its place in the scenes stay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceSettings {
    /// The source's id.
    pub source: u32,
    /// The keys to change.
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `source.get_settings`' answer: every stored setting of the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSettingsInfo {
    /// The source's id.
    pub source: u32,
    /// Its settings.
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// `source.set_sync_offset` (protocol 1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceSyncOffset {
    /// The source's id.
    pub source: u32,
    /// Milliseconds, `-200..=200`: positive delays the sound, negative advances it.
    pub offset_ms: i32,
}

/// `sync.measure_start` (protocol 1.66, ADR-0126 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMeasureStart {
    /// The audio source to measure.
    pub source: u32,
    /// The picture source to clap in view of; absent: the camera of a camera
    /// microphone, else the only camera in the program scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<u32>,
}

/// `sync.measure_start`'s answer (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncMeasureStarted {
    /// The measurement's id, as `sync.measure.changed` names it.
    pub measurement: String,
    /// The reference picture source used.
    pub reference: u32,
}

/// `sync.measure_cancel` (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMeasureCancel {
    /// The id `sync.measure_start` answered.
    pub measurement: String,
}

/// `sync.forget_device` (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncForgetDevice {
    /// The device, as `sync.devices` lists it.
    pub key: DeviceKey,
}

/// `sync.apply_to_device_sources` (protocol 1.66).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncApplyToDeviceSources {
    /// The source whose offset the other sources of its device get.
    pub source: u32,
}

/// `sync.apply_to_device_sources`'s answer (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncAppliedToDeviceSources {
    /// The sources that changed.
    pub changed: Vec<u32>,
}

/// `sync.devices`'s answer (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncDeviceList {
    /// One entry per remembered device.
    pub devices: Vec<DeviceSyncEntry>,
}

/// `item.add` (protocol 1.6): a source the collection has, placed on top of the
/// program scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddItem {
    /// The source's id (`scene.list` lists every source).
    pub source: u32,
    /// The scene it is placed in (protocol 1.9); absent: the program scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
}

/// Which way `item.move` or `scene.move` steps in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerDirection {
    /// Over the next source with a picture (or one place up in the scene list).
    Up,
    /// Under the next source with a picture (or one place down in the scene list).
    Down,
    /// To the top of the picture layers (or the start of the scene list). Protocol 1.36.
    Front,
    /// To the bottom of the picture layers (or the end of the scene list). Protocol 1.36.
    Back,
}

/// `source.set_active` (protocol 1.33): keep the device open, or release it.
/// Hiding a source does not release it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSourceActive {
    /// The source's id.
    pub source: u32,
    /// Open while a scene that places it is on air or in preview.
    pub active: bool,
}

/// How the Library groups a built-in look (protocol 1.34, ADR-0075).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookGroup {
    /// Colour grading.
    Color,
    /// Blur, sharpen, keying.
    Effect,
    /// The shape and border of the picture.
    Shape,
    /// A group this decoder does not know. A client treats it like any other group
    /// it cannot name.
    #[serde(other)]
    Other,
}

/// One built-in look, as `look.list` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookInfo {
    /// Stable id, `builtin:<slug>`.
    pub id: String,
    /// Name for lists.
    pub name: String,
    /// Its group.
    pub group: LookGroup,
}

/// `look.list`'s answer (protocol 1.34): the built-in looks, in Library order.
/// There are no user-saved looks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookList {
    /// One row per built-in look.
    pub looks: Vec<LookInfo>,
}

/// `look.apply` (protocol 1.34, ADR-0075): copy a built-in look onto a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyLook {
    /// The source's id.
    pub source: u32,
    /// The look's id (`look.list`).
    pub look: String,
}

/// `source.add_sample` (protocol 1.34, ADR-0075): a built-in sample picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddSample {
    /// The sample's id.
    pub sample: String,
}

/// `item.set_box` (protocol 1.34): the picture's box on the canvas the studio shows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemBox {
    /// The source's id. The box is the one on the scene being edited.
    pub source: u32,
    /// Top-left corner, in canvas pixels.
    pub position: [f32; 2],
    /// Width and height, in canvas pixels. Zero, or a value that is not finite, is
    /// refused.
    pub size: [f32; 2],
    /// Boxes with the same number other than 0 that follow each other are one undo
    /// step (one drag). 0, the default, is a step of its own.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
    /// The caller's number for this request, echoed in the answer. 0 when the caller
    /// does not track one.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub seq: u64,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.set_box`'s answer: the request's `seq`, and that the box is the source's now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoxPlaced {
    /// The request's `seq`.
    pub seq: u64,
    /// The box was taken (changed, or it was already that box).
    pub ok: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_main(c: &OutputCanvas) -> bool {
    *c == OutputCanvas::Main
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// `global.add` (protocol 1.10, ADR-0065).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddGlobal {
    /// The source's id.
    pub source: u32,
}

/// `global.remove` (protocol 1.10, ADR-0065).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveGlobal {
    /// The source's id.
    pub source: u32,
    /// Move it into the program scene (in studio mode the preview scene) instead of
    /// removing it.
    #[serde(default)]
    pub keep: bool,
}

/// `canvas.set_preview` (protocol 1.9, ADR-0061): a scene enters studio mode with it in
/// preview, or changes the preview scene; `null` leaves studio mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPreview {
    /// The preview scene's id, or `null`.
    pub scene: Option<u32>,
}

/// `item.set_show_for` (ADR-0105). Undoable document state. No protocol minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemShowFor {
    /// The item's id (`source.list`).
    pub item: u32,
    /// Milliseconds in `1..=600000`, or `null` / absent to clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_for_ms: Option<u32>,
}

/// `item.set_scroll` (protocol 1.94, ADR-0150 decision 11): sets or clears the saved
/// scroll of a source item's layout on a canvas. One undo step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemScroll {
    /// The item's id (`source.list`).
    pub item: u32,
    /// The scroll; `null` clears it.
    pub scroll: Option<ItemScroll>,
    /// `"main"` (the default) or `"vertical"`, as for the other item edits.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.scroll_pause`, `item.scroll_resume` and `item.scroll_restart` (protocol 1.94,
/// ADR-0150 decision 11): live, not saved, not undone. They act on the item's run on
/// the canvas, so every item sharing that run moves too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollItem {
    /// The item's id.
    pub item: u32,
    /// `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.scroll_speed` (protocol 1.94, ADR-0150 decision 11): live speed of the item's
/// run until a scroll edit, a restart or the run's end.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetScrollSpeed {
    /// The item's id.
    pub item: u32,
    /// Units per second, 1 to 4000 (0 is refused: pause is the call for it).
    pub speed: f32,
    /// `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.show` (ADR-0105). Live session cue. Does not change `visible` and is not
/// undone. No protocol minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowItem {
    /// The item's id (`source.list`).
    pub item: u32,
}

/// `calibration.run` (protocol 1.55, ADR-0087 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationRun {
    /// The recording profile whose plan is measured (`output.profiles`).
    pub profile: String,
    /// The canvas; the main one when absent or `null`. `all` is `invalid_params`:
    /// one canvas per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputTarget>,
}

/// `record.start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStart {
    /// A recording profile's id (`output.profiles`). The file goes into its folder.
    pub profile: String,
    /// The canvas to record; the main one when absent or `null` (protocol 1.29);
    /// `all` records every canvas, main first (protocol 1.42).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputTarget>,
    /// Write the separate files of the marked sources (protocol 1.46, ADR-0086); only
    /// the main canvas's recording has them. Absent or `null` follows Settings,
    /// Recording, Separate files (`separate_files.enabled`, ADR-0164 decision 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<bool>,
    /// The caller accepts a `heavy` plan (protocol 1.54, ADR-0087 decision 6). Absent
    /// or `null` is `false`; with this or `on_hardware_limit` set, the start gate may
    /// refuse the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledge_heavy: Option<bool>,
    /// What to do with separate video files past their encoder's capacity (protocol
    /// 1.54). Absent or `null` uses the profile's `recording.iso.on_hardware_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_hardware_limit: Option<HardwareLimitPolicy>,
}

impl RecordStart {
    /// A start of `profile` on the main canvas, every other field absent.
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            canvas: None,
            iso: None,
            acknowledge_heavy: None,
            on_hardware_limit: None,
        }
    }
}

/// `record.start`'s `on_hardware_limit` (protocol 1.54, ADR-0087 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareLimitPolicy {
    /// Refuse the start and say which files do not fit.
    Ask,
    /// Start without the files that do not fit.
    Skip,
    /// Encode the files that do not fit on the CPU (refused until the CPU encoder for
    /// separate files exists, ADR-0119 step (c)).
    Cpu,
}

/// `record.stop`, `stream.start` and `stream.stop`: one canvas (protocol 1.29) or every
/// canvas the studio runs, main first (`all`, protocol 1.42); also `record.pause`,
/// `record.resume` and `record.pause_toggle` (protocol 1.61). Unknown fields are
/// ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnCanvases {
    /// The canvases; the main one when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputTarget>,
}

impl OnCanvases {
    /// The canvases asked for, the main one by default.
    pub fn target(self) -> OutputTarget {
        self.canvas.unwrap_or_default()
    }
}

/// `stream.destinations` (protocol 1.74, ADR-0127 decision 16): one canvas's
/// destinations, or both canvases' when `canvas` is absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationsOf {
    /// The canvas; both when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputCanvas>,
}

/// `stream.destination.start` and `stream.destination.stop` (protocol 1.74).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationRef {
    /// The destination's id (`stream.destinations`).
    pub id: u32,
}

/// `stream.destination.set_enabled` (protocol 1.74).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDestinationEnabled {
    /// The destination's id.
    pub id: u32,
    /// Go live sends to it.
    pub enabled: bool,
}

/// `output.set_with_vertical` (protocol 1.42, ADR-0111 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetWithVertical {
    /// Recording or stream.
    pub kind: OutputKind,
    /// Record or Go live also act on the vertical output.
    pub on: bool,
}

/// `stream.hold` and `stream.hold_toggle` (protocol 1.64, ADR-0124 decisions 12 and
/// 13): the canvases as in [`OnCanvases`] and the picture sent while held (a scene
/// and its sound from protocol 1.67). Unknown
/// fields are ignored; an unknown `picture` is `invalid_params`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHold {
    /// The canvases; the main one when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputTarget>,
    /// What the stream sends while held; the stream settings' `hold_picture` when
    /// absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<HoldPicture>,
    /// With `picture: "scene"`: the scene shown (protocol 1.67); the stream settings'
    /// `hold_scene` when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
    /// With `picture: "scene"`: the sound sent (protocol 1.67); the stream settings'
    /// `hold_sound` when absent or `null`. With `freeze` or `black` it is
    /// `invalid_params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<HoldSound>,
}

impl StreamHold {
    /// The canvases asked for, the main one by default.
    pub fn target(self) -> OutputTarget {
        self.canvas.unwrap_or_default()
    }
}

/// What a held stream sends (protocol 1.64, ADR-0124 decision 13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldPicture {
    /// The program's last frame, repeated.
    #[default]
    Freeze,
    /// A black picture.
    Black,
    /// A scene of the collection, drawn live (protocol 1.67, ADR-0124 phase 2b).
    Scene,
}

/// What a stream held with a scene sends for sound (protocol 1.67, ADR-0124 phase
/// 2b).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldSound {
    /// Silence, as a freeze or black hold.
    #[default]
    Silence,
    /// The scene's own sources, mixed at their levels.
    Scene,
}

/// `record.status` and `stream.status`: the canvas they report (protocol 1.29). Unknown
/// fields are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnCanvas {
    /// The canvas; the main one when absent or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputCanvas>,
}

impl OnCanvas {
    /// The canvas asked for, the main one by default.
    pub fn canvas(self) -> OutputCanvas {
        self.canvas.unwrap_or_default()
    }
}

/// `replay.save` (protocol 1.27).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySave {
    /// How many seconds back, 1 to the buffer's length; everything it holds when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
}

/// `record.start_to_file` (`admin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStartToFile {
    /// A recording profile's id, for the encoding settings.
    pub profile: String,
    /// The file, absolute; refused with `file_exists` if it exists.
    pub path: String,
    /// The canvas to record; the main one when absent or `null` (protocol 1.29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputCanvas>,
}

/// What `import.obs.check` and `import.obs.run` bring (protocol 1.82, ADR-0131
/// decision 12): the parts the desktop wizard offers. Every part is on when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsImportChoice {
    /// The scenes to bring, by OBS name (and the scenes they nest); absent: all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenes: Option<Vec<String>>,
    /// OBS's desktop audio and microphones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_audio: Option<bool>,
    /// Audio filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_filters: Option<bool>,
    /// Video filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_filters: Option<bool>,
    /// The canvas size and frame rate of the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<bool>,
    /// Recording settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
    /// The stream server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_server: Option<bool>,
    /// The stream key (into the credential store only; needs `stream_server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<bool>,
    /// Hotkeys (added; never replacing a shortcut of the user's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkeys: Option<bool>,
}

/// `import.obs.check` and `import.obs.run` (protocol 1.82): a collection and a
/// profile named as `import.obs.scan` found them, never by path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsImport {
    /// A collection name from `import.obs.scan`.
    pub collection: String,
    /// A profile name from `import.obs.scan`; absent: no profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// What to bring; absent: everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choose: Option<ObsImportChoice>,
}

/// `profile.switch` and `collection.switch` (protocol 1.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRef {
    /// The entry's id (`profile.list`, `collection.list`).
    pub id: String,
}

/// `events.subscribe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    /// Topics to add.
    pub topics: Vec<Topic>,
    /// Sampling rates in Hz for latest-value topics; capped per topic.
    #[serde(default)]
    pub rates: BTreeMap<Topic, u32>,
}

/// `events.unsubscribe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsubscribe {
    /// Topics to drop.
    pub topics: Vec<Topic>,
}

/// `tokens.create`: a named access token for apps (ADR-0044 decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateToken {
    /// Its name, unique; surrounding spaces are dropped.
    pub name: String,
    /// What it allows: `["read"]`, `["read", "control"]` or `["read", "control",
    /// "admin"]` (the access levels the desktop app offers). Never more than the
    /// caller's own scopes (`not_permitted`, protocol 1.80).
    pub scopes: ScopeSet,
    /// "Allow actions that cannot be undone" (protocol 1.80, ADR-0052 decision 4);
    /// absent: allowed, as tokens behave without the field. Only a caller that holds
    /// the permission reaches `tokens.create` (it is `destructive`), so it can give
    /// either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<bool>,
}

/// `tokens.revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRef {
    /// The token's name.
    pub name: String,
}

/// `addon.install`, `addon.cancel`, `addon.clean` and `addon.remove` (protocol 1.90,
/// ADR-0145): an add-on of the built-in catalog, by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddOnRef {
    /// The add-on's id, as `addon.list` names it (`org.rbs.browser-engine`).
    pub id: String,
}

/// A request the control thread carries out.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `studio.get`.
    StudioGet,
    /// `source.list`.
    SourceList,
    /// `source.targets`.
    SourceTargets(TargetsParams),
    /// `source.add`.
    SourceAdd(AddSource),
    /// `source.delete`.
    SourceDelete(SourceRef),
    /// `item.set_style` (protocol 1.3).
    ItemSetStyle(SetItemStyle),
    /// `item.set_crop` (protocol 1.5).
    ItemSetCrop(SetItemCrop),
    /// `item.flip` (protocol 1.18).
    ItemFlip(FlipItem),
    /// `item.rotate` (protocol 1.18).
    ItemRotate(RotateItem),
    /// `item.fit` (protocol 1.18).
    ItemFit(FitItem),
    /// `item.align` (protocol 1.18).
    ItemAlign(AlignItem),
    /// `item.reset_transform` (protocol 1.18).
    ItemResetTransform(ResetItemTransform),
    /// `item.set_rotation` (protocol 1.100, ADR-0166).
    ItemSetRotation(SetItemRotation),
    /// `item.align_items` (protocol 1.106).
    ItemAlignItems(AlignItems),
    /// `item.distribute` (protocol 1.106).
    ItemDistribute(DistributeItems),
    /// `item.fit_text_box` (protocol 1.100).
    ItemFitTextBox(FitTextBox),
    /// `item.set_filled` (protocol 1.100).
    ItemSetFilled(SetItemFilled),
    /// `item.set_shown_region` (protocol 1.100).
    ItemSetShownRegion(SetItemShownRegion),
    /// `canvas.auto_layout` (protocol 1.44).
    CanvasAutoLayout(CanvasAutoLayout),
    /// `canvas.set_vertical_size` (protocol 1.44).
    CanvasSetVerticalSize(SetVerticalSize),
    /// `scene.list` (protocol 1.6).
    SceneList,
    /// `scene.add` (protocol 1.6).
    SceneAdd(AddScene),
    /// `scene.remove` (protocol 1.6).
    SceneRemove(SceneRef),
    /// `scene.rename` (protocol 1.6).
    SceneRename(RenameScene),
    /// `group.create` (protocol 1.104, ADR-0182).
    GroupCreate(CreateGroup),
    /// `group.remove` (protocol 1.104): ungroups.
    GroupRemove(GroupRef),
    /// `group.rename` (protocol 1.104).
    GroupRename(RenameGroup),
    /// `group.list` (protocol 1.104).
    GroupList(SceneRef),
    /// `scene.set_transition` (protocol 1.86).
    SceneSetTransition(SetSceneTransition),
    /// `scene.duplicate` (protocol 1.43): a copy of the scene, not on air.
    SceneDuplicate(DuplicateScene),
    /// `source.duplicate` (protocol 1.68).
    SourceDuplicate(SourceInScene),
    /// `source.reset` (protocol 1.68).
    SourceReset(ResetSource),
    /// `source.copy_attributes` (protocol 1.68).
    SourceCopyAttributes(SourceInScene),
    /// `source.attributes_clipboard` (protocol 1.68).
    SourceAttributesClipboard,
    /// `source.paste_attributes` (protocol 1.68).
    SourcePasteAttributes(PasteAttributes),
    /// `source.set_text` (protocol 1.75).
    SourceSetText(SetText),
    /// `text.fonts` (protocol 1.75).
    TextFonts,
    /// `scene.switch` (protocol 1.6).
    SceneSwitch(SwitchScene),
    /// `source.rename` (protocol 1.6).
    SourceRename(RenameSource),
    /// `source.set_volume` (protocol 1.6).
    SourceSetVolume(SetSourceVolume),
    /// `source.set_muted` (protocol 1.6).
    SourceSetMuted(SetSourceMuted),
    /// `source.set_mixer_hidden` (protocol 1.14, ADR-0080).
    SourceSetMixerHidden(SetSourceMixerHidden),
    /// `canvas.set_master_volume` (protocol 1.15, ADR-0081).
    CanvasSetMasterVolume(SetMasterVolume),
    /// `canvas.set_master_muted` (protocol 1.15, ADR-0081).
    CanvasSetMasterMuted(SetMasterMuted),
    /// `item.add` (protocol 1.6).
    ItemAdd(AddItem),
    /// `filter.types` (protocol 1.8; the `domain` parameter since 1.16, ADR-0082).
    FilterTypes(FilterTypesParams),
    /// `filter.add` (protocol 1.8).
    FilterAdd(AddFilter),
    /// `filter.add_voice_chain` (protocol 1.100).
    FilterAddVoiceChain(SourceRef),
    /// `filter.remove` (protocol 1.8).
    FilterRemove(FilterRef),
    /// `filter.move` (protocol 1.8).
    FilterMove(MoveFilter),
    /// `filter.set_enabled` (protocol 1.8).
    FilterSetEnabled(SetFilterEnabled),
    /// `filter.set_settings` (protocol 1.8).
    FilterSetSettings(SetFilterSettings),
    /// `source.set_sync_offset` (protocol 1.8).
    SourceSetSyncOffset(SetSourceSyncOffset),
    /// `sync.measure_start` (protocol 1.66, ADR-0126).
    SyncMeasureStart(SyncMeasureStart),
    /// `sync.measure_cancel` (protocol 1.66).
    SyncMeasureCancel(SyncMeasureCancel),
    /// `sync.devices` (protocol 1.66).
    SyncDevices,
    /// `sync.forget_device` (protocol 1.66).
    SyncForgetDevice(SyncForgetDevice),
    /// `sync.apply_to_device_sources` (protocol 1.66).
    SyncApplyToDeviceSources(SyncApplyToDeviceSources),
    /// `source.retarget` (protocol 1.13).
    SourceRetarget(RetargetSource),
    /// `source.reload` (protocol 1.13).
    SourceReload(ReloadSource),
    /// `browser.reload` (protocol 1.91, ADR-0132).
    BrowserReload(BrowserReload),
    /// `browser.run_script` (protocol 1.91, ADR-0149).
    BrowserRunScript(BrowserRunScript),
    /// `browser.forget_sign_in` (protocol 1.96, ADR-0132).
    BrowserForgetSignIn(BrowserForgetSignIn),
    /// `browser.input` (protocol 1.96, ADR-0132).
    BrowserInput(BrowserInput),
    /// `source.get_settings` (protocol 1.17).
    SourceGetSettings(GetSourceSettings),
    /// `media.play` (protocol 1.83, ADR-0108).
    MediaPlay(MediaSource),
    /// `media.pause` (protocol 1.83).
    MediaPause(MediaSource),
    /// `media.seek` (protocol 1.83).
    MediaSeek(MediaSeek),
    /// `media.restart` (protocol 1.83).
    MediaRestart(MediaSource),
    /// `media.state` (protocol 1.83).
    MediaState(MediaSource),
    /// `media.next` (protocol 1.102, ADR-0171): a slide show's next picture.
    MediaNext(MediaSource),
    /// `media.previous` (protocol 1.102, ADR-0171): a slide show's previous picture.
    MediaPrevious(MediaSource),
    /// `source.set_settings` (protocol 1.17).
    SourceSetSettings(SetSourceSettings),
    /// `source.set_companion_audio` (protocol 1.26).
    SourceSetCompanionAudio(SetCompanionAudio),
    /// `canvas.add_vertical` (protocol 1.28, ADR-0093).
    CanvasAddVertical(AddVerticalCanvas),
    /// `canvas.remove_vertical` (protocol 1.28, ADR-0093).
    CanvasRemoveVertical(Empty),
    /// `settings.recent_targets` (protocol 1.13).
    SettingsRecentTargets(TargetsParams),
    /// `settings.clear_recent_targets` (protocol 1.13).
    SettingsClearRecentTargets(TargetsParams),
    /// `item.set_visible` (protocol 1.6).
    ItemSetVisible(SetItemVisible),
    /// `item.set_locked` (protocol 1.6).
    ItemSetLocked(SetItemLocked),
    /// `item.set_show_for` (ADR-0105). Undoable. No protocol minor.
    ItemSetShowFor(SetItemShowFor),
    /// `item.show` (ADR-0105). Live, not undoable. No protocol minor.
    ItemShow(ShowItem),
    /// `item.set_scroll` (protocol 1.94, ADR-0150). Undoable.
    ItemSetScroll(SetItemScroll),
    /// `item.scroll_pause` (protocol 1.94). Live.
    ItemScrollPause(ScrollItem),
    /// `item.scroll_resume` (protocol 1.94). Live.
    ItemScrollResume(ScrollItem),
    /// `item.scroll_restart` (protocol 1.94). Live.
    ItemScrollRestart(ScrollItem),
    /// `item.scroll_speed` (protocol 1.94). Live.
    ItemScrollSpeed(SetScrollSpeed),
    /// `scene.sources` (protocol 1.9): the source items of any scene.
    SceneSources(SceneRef),
    /// `canvas.set_preview` (protocol 1.9).
    CanvasSetPreview(SetPreview),
    /// `canvas.transition` (protocol 1.9).
    CanvasTransition(TransitionOnce),
    /// `canvas.rehearse` (protocol 1.100).
    CanvasRehearse(Rehearse),
    /// `canvas.transition_progress` (protocol 1.87, ADR-0135): a T-bar value.
    CanvasTransitionProgress(TransitionProgress),
    /// `item.remove` (protocol 1.10).
    ItemRemove(ItemRef),
    /// `item.move` (protocol 1.32).
    ItemMove(MoveItem),
    /// `scene.move` (protocol 1.33).
    SceneMove(MoveScene),
    /// `source.set_active` (protocol 1.33).
    SourceSetActive(SetSourceActive),
    /// `look.list` (protocol 1.34).
    LookList,
    /// `look.apply` (protocol 1.34).
    LookApply(ApplyLook),
    /// `source.add_sample` (protocol 1.34).
    SourceAddSample(AddSample),
    /// `item.set_box` (protocol 1.34).
    ItemSetBox(SetItemBox),
    /// `global.list` (protocol 1.10).
    GlobalList,
    /// `global.add` (protocol 1.10).
    GlobalAdd(AddGlobal),
    /// `global.remove` (protocol 1.10).
    GlobalRemove(RemoveGlobal),
    /// `edit.undo`.
    EditUndo,
    /// `edit.redo`.
    EditRedo,
    /// `edit.undo_delete` (protocol 1.100).
    EditUndoDelete,
    /// `output.profiles`.
    OutputProfiles,
    /// `record.start`.
    RecordStart(RecordStart),
    /// `record.estimate` (protocol 1.53, ADR-0087).
    RecordEstimate(RecordStart),
    /// `record.start_to_file`.
    RecordStartToFile(RecordStartToFile),
    /// `record.stop`.
    RecordStop(OnCanvases),
    /// `record.pause` (protocol 1.61, ADR-0124).
    RecordPause(OnCanvases),
    /// `record.resume` (protocol 1.61, ADR-0124).
    RecordResume(OnCanvases),
    /// `record.pause_toggle` (protocol 1.61, ADR-0124).
    RecordPauseToggle(OnCanvases),
    /// `record.status`.
    RecordStatus(OnCanvas),
    /// `stream.start`.
    StreamStart(OnCanvases),
    /// `stream.stop`.
    StreamStop(OnCanvases),
    /// `stream.hold` (protocol 1.64, ADR-0124).
    StreamHold(StreamHold),
    /// `stream.resume` (protocol 1.64, ADR-0124).
    StreamResume(OnCanvases),
    /// `stream.hold_toggle` (protocol 1.64, ADR-0124).
    StreamHoldToggle(StreamHold),
    /// `output.break_toggle` (protocol 1.64, ADR-0124 decision 15).
    OutputBreakToggle,
    /// `stream.status`.
    StreamStatus(OnCanvas),
    /// `stream.destinations` (protocol 1.74).
    StreamDestinations(DestinationsOf),
    /// `stream.destination.start` (protocol 1.74).
    StreamDestinationStart(DestinationRef),
    /// `stream.destination.stop` (protocol 1.74).
    StreamDestinationStop(DestinationRef),
    /// `stream.destination.set_enabled` (protocol 1.74).
    StreamDestinationSetEnabled(SetDestinationEnabled),
    /// `output.set_with_vertical` (protocol 1.42).
    OutputSetWithVertical(SetWithVertical),
    /// `replay.status` (protocol 1.27).
    ReplayStatus,
    /// `protection.status` (protocol 1.49, ADR-0090).
    ProtectionStatus,
    /// `calibration.run` (protocol 1.55, ADR-0087 decision 4): *Test this computer*.
    CalibrationRun(CalibrationRun),
    /// `calibration.cancel` (protocol 1.55).
    CalibrationCancel,
    /// `calibration.status` (protocol 1.55).
    CalibrationStatus,
    /// `source.set_monitor` (protocol 1.30).
    SourceSetMonitor(SetSourceMonitor),
    /// `monitor.status` (protocol 1.30).
    MonitorStatus,
    /// `monitor.devices` (protocol 1.30).
    MonitorDevices,
    /// `monitor.set_device` (protocol 1.30).
    MonitorSetDevice(SetMonitorDevice),
    /// `monitor.set_volume` (protocol 1.30).
    MonitorSetVolume(SetMonitorVolume),
    /// `monitor.set_solo` (protocol 1.30).
    MonitorSetSolo(SetMonitorSolo),
    /// `loudness.status` (protocol 1.47, ADR-0092).
    LoudnessStatus,
    /// `loudness.set_enabled` (protocol 1.47).
    LoudnessSetEnabled(SetLoudnessEnabled),
    /// `loudness.reset` (protocol 1.47).
    LoudnessReset,
    /// `replay.arm` (protocol 1.27).
    ReplayArm,
    /// `replay.disarm` (protocol 1.27).
    ReplayDisarm,
    /// `replay.save` (protocol 1.27).
    ReplaySave(ReplaySave),
    /// `vcam.status` (protocol 1.12).
    VcamStatus,
    /// `vcam.start` (protocol 1.12).
    VcamStart,
    /// `vcam.stop` (protocol 1.12).
    VcamStop,
    /// `settings.get`.
    SettingsGet,
    /// `settings.set`.
    SettingsSet(SettingsPatch),
    /// `profile.list` (protocol 1.4).
    ProfileList,
    /// `profile.switch` (protocol 1.4).
    ProfileSwitch(EntryRef),
    /// `collection.list` (protocol 1.4).
    CollectionList,
    /// `collection.switch` (protocol 1.4).
    CollectionSwitch(EntryRef),
    /// `collection.set_glide` (protocol 1.35, ADR-0100).
    CollectionSetGlide(SetCollectionGlide),
    /// `collection.set_stinger_file` (protocol 1.100).
    CollectionSetStingerFile(SetStingerFile),
    /// `collection.set_transition` (protocol 1.81, ADR-0128).
    CollectionSetTransition(SetCollectionTransition),
    /// `source.set_glide` (protocol 1.35, ADR-0100).
    SourceSetGlide(SetSourceGlide),
    /// `source.set_activity` (protocol 1.95, ADR-0152).
    SourceSetActivity(SetSourceActivity),
    /// `source.get_activity` (protocol 1.95, ADR-0152).
    SourceGetActivity(SourceRef),
    /// `source.set_region` (protocol 1.41, ADR-0103).
    SourceSetRegion(SetSourceRegion),
    /// `source.set_record_separately` (protocol 1.46, ADR-0086).
    SourceSetRecordSeparately(SetRecordSeparately),
    /// `marker.add` (protocol 1.50, ADR-0089).
    MarkerAdd(MarkerAdd),
    /// `marker.remove` (protocol 1.50, ADR-0089).
    MarkerRemove(MarkerRemove),
    /// `project.export` (protocol 1.58, ADR-0089).
    ProjectExport(ProjectExport),
    /// `source.get_region` (protocol 1.41).
    SourceGetRegion(SourceRef),
    /// `source.set_chrome` (protocol 1.41, ADR-0104).
    SourceSetChrome(SetSourceChrome),
    /// `source.get_chrome` (protocol 1.41).
    SourceGetChrome(SourceRef),
    /// `source.fit_chrome` (protocol 1.41): places the item with the stored fill.
    SourceFitChrome(SourceRef),
    /// `source.reset_chrome` (protocol 1.41): copies insets from `work_rect`.
    /// A no-op when the adapter did not report one.
    SourceResetChrome(SourceRef),
    /// `scene.audio.get` (protocol 1.48, ADR-0091).
    SceneAudioGet(SceneRef),
    /// `scene.audio.set` (protocol 1.48).
    SceneAudioSet(SetSceneLevel),
    /// `scene.audio.own` (protocol 1.48): Own levels, copying what is heard.
    SceneAudioOwn(SceneRef),
    /// `scene.audio.shared` (protocol 1.48): Shared levels, discarding the entries.
    SceneAudioShared(SceneRef),
    /// `scene.audio.reset_source` (protocol 1.48): removes one entry.
    SceneAudioResetSource(SceneSourceRef),
    /// `events.subscribe`: answered on the control thread with the `seq` of the topics'
    /// state keys, read after the subscriber mask was set. The target never sees it.
    EventsSubscribe(Subscribe),
    /// `tokens.list`.
    TokensList,
    /// `tokens.create`.
    TokensCreate(CreateToken),
    /// `tokens.revoke`.
    TokensRevoke(TokenRef),
    /// `import.obs.scan` (protocol 1.82): the OBS collections and profiles on this
    /// computer, by name.
    ImportObsScan,
    /// `import.obs.check` (protocol 1.82): the report of an import, nothing written.
    ImportObsCheck(ObsImport),
    /// `import.obs.run` (protocol 1.82): imports as a new scene collection and output
    /// profile.
    ImportObsRun(ObsImport),
    /// `text.timer` (protocol 1.84): starts, pauses or resets a countdown or stopwatch.
    TextTimer(TextTimer),
    /// `addon.list` (protocol 1.90, ADR-0145): every add-on of the catalog.
    AddonList,
    /// `addon.install` (protocol 1.90): starts a download; progress through
    /// `addon.changed`.
    AddonInstall(AddOnRef),
    /// `addon.update` (protocol 1.100).
    AddonUpdate(AddOnRef),
    /// `addon.cancel` (protocol 1.90): stops a running install.
    AddonCancel(AddOnRef),
    /// `addon.clean` (protocol 1.90): removes versions older than the one before the
    /// catalog's.
    AddonClean(AddOnRef),
    /// `addon.remove` (protocol 1.90): removes every version.
    AddonRemove(AddOnRef),
}

/// Everything a client may send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "WireRequest", into = "WireRequest")]
pub enum Request {
    /// `session.hello`.
    Hello(Hello),
    /// `session.authenticate`.
    Authenticate(Authenticate),
    /// `api.describe`.
    Describe,
    /// `events.unsubscribe`.
    Unsubscribe(Unsubscribe),
    /// A command for the control thread.
    Command(Command),
}

impl Request {
    /// Its method name.
    pub fn method(&self) -> &'static str {
        match self {
            Request::Hello(_) => "session.hello",
            Request::Authenticate(_) => "session.authenticate",
            Request::Describe => "api.describe",
            Request::Unsubscribe(_) => "events.unsubscribe",
            Request::Command(c) => c.method(),
        }
    }

    /// The scope it needs, or `None` for the handshake.
    pub fn scope(&self) -> Option<Scope> {
        method_scope(self.method()).flatten()
    }
}

impl Command {
    /// What this call does (ADR-0052 decision 4): the method's effect, except that
    /// `settings.set` with a `canvas` is `live`, because it restarts the studio,
    /// which clears the undo history and re-arms the replay buffer empty. A method
    /// missing from the table counts as `destructive`.
    pub fn effect(&self) -> Effect {
        match self {
            Command::SettingsSet(p) if p.canvas.is_some() => Effect::Live,
            c => method_effect(c.method()).unwrap_or(Effect::Destructive),
        }
    }

    /// True when the call, by its parameters alone, could make RBS open a connection
    /// to a host (ADR-0052 decision 4, protocol 1.79): a browser source, or a location
    /// value that is not a local absolute path ([`crate::location::is_local_path`]).
    /// Checked on the API thread; a source's settings and target, whose meaning
    /// depends on the source's kind, are checked by the control target.
    pub fn reaches_network(&self) -> bool {
        use crate::location::is_local_path;
        match self {
            Command::SourceAdd(p) => {
                p.kind == SourceKind::Browser
                    || p.path.as_deref().is_some_and(|f| !is_local_path(f))
            }
            // Downloads from the catalog's hosts (ADR-0145 decision 9).
            Command::AddonInstall(_) | Command::AddonUpdate(_) => true,
            Command::RecordStartToFile(p) => !is_local_path(&p.path),
            Command::CollectionSetStingerFile(p) => !is_local_path(&p.path),
            Command::SettingsSet(p) => {
                let far = |f: &str| !f.is_empty() && !is_local_path(f);
                p.profiles
                    .iter()
                    .flatten()
                    .chain(p.vertical_profile.iter().flatten().map(|b| &**b))
                    .any(|r| far(&r.folder))
                    || p.replay
                        .as_ref()
                        .and_then(|r| r.folder.as_deref())
                        .is_some_and(far)
            }
            _ => false,
        }
    }
}

impl Command {
    /// The call's parameters as on the wire (`params`), for checks that look at
    /// which scene, item, source or filter a call names (ADR-0052 decision 4).
    /// `null` for a method without parameters.
    pub fn params_json(&self) -> serde_json::Value {
        let wire = WireRequest::from(Request::Command(self.clone()));
        serde_json::to_value(wire)
            .ok()
            .and_then(|mut v| v.get_mut("params").map(serde_json::Value::take))
            .unwrap_or(serde_json::Value::Null)
    }
}

/// A method and the scope it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MethodInfo {
    /// `namespace.verb`.
    pub name: &'static str,
    /// The scope it needs; `null` for the handshake.
    pub scope: Option<Scope>,
    /// What it does to the studio (protocol 1.76, ADR-0052 decision 4).
    pub effect: Effect,
}

/// What a method does to the studio (protocol 1.76, ADR-0052 decision 4). A client
/// treats a missing or unknown effect as [`Effect::Destructive`]. Without the token
/// permission "Allow actions that cannot be undone", `live` and `destructive`
/// methods are refused with `not_permitted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Reads state; changes nothing.
    Read,
    /// Undoable, or reversed by another call with nothing lost, or only adds a new
    /// file and never replaces one.
    Edit,
    /// Starts or stops what goes out: recordings, streams, the virtual camera, the
    /// replay buffer, a calibration run; switches the output profile.
    Live,
    /// Removes something outside the undo history.
    Destructive,
}

impl Effect {
    /// Whether it needs the irreversible-actions permission.
    pub fn is_irreversible(self) -> bool {
        matches!(self, Effect::Live | Effect::Destructive)
    }
}

const fn m(name: &'static str, scope: Option<Scope>, effect: Effect) -> MethodInfo {
    MethodInfo {
        name,
        scope,
        effect,
    }
}

/// The effect of a method name, `None` for an unknown method (ADR-0052 decision 4).
pub fn method_effect(name: &str) -> Option<Effect> {
    METHODS.iter().find(|i| i.name == name).map(|i| i.effect)
}

/// The scope of a method name: `None` for an unknown method, `Some(None)` for the
/// handshake.
pub fn method_scope(name: &str) -> Option<Option<Scope>> {
    METHODS.iter().find(|i| i.name == name).map(|i| i.scope)
}

/// The scope needed to subscribe to a topic: `settings.changed` carries folders, which
/// only `admin` may read; every other topic needs `read`.
pub const fn topic_scope(topic: Topic) -> Scope {
    match topic {
        Topic::SettingsChanged => Scope::Admin,
        _ => Scope::Read,
    }
}

/// `session.hello`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// The protocol version the session speaks.
    pub protocol: String,
    /// A random id of this server run; `seq` counters restart with it.
    pub instance: String,
    /// On IPC, a verified same-user peer is authenticated by `session.hello` and gets
    /// its scopes here; otherwise `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<ScopeSet>,
}

/// `session.authenticate`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResult {
    /// What the session may do (the token's scopes, limited by the channel).
    pub scopes: ScopeSet,
    /// Whether the token holds "Allow actions that cannot be undone" (protocol
    /// 1.76). Absent from an older server: a client treats it as `false`.
    #[serde(default)]
    pub irreversible: bool,
    /// The server.
    pub server: ClientInfo,
}

/// `api.describe`'s answer. The JSON Schema joins it with the schema generator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    /// The protocol version the session speaks.
    pub protocol: String,
    /// Every method and its scope.
    pub methods: Vec<MethodInfoOwned>,
    /// Every event topic.
    pub topics: Vec<Topic>,
}

/// A method and its scope, owned (for deserializing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodInfoOwned {
    /// `namespace.verb`.
    pub name: String,
    /// The scope it needs; `null` for the handshake.
    pub scope: Option<Scope>,
    /// What it does (protocol 1.76); absent from an older server, which a client
    /// treats as `destructive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
}

/// `source.targets`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetList {
    /// The kind.
    pub kind: SourceKind,
    /// Its capture targets, in OS order.
    pub targets: Vec<CaptureTarget>,
}

/// `scene.sources`' answer (protocol 1.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneSourceList {
    /// The scene.
    pub scene: u32,
    /// One row per source item of the scene, top first, as in `source.list`.
    pub sources: Vec<SourceInfo>,
}

/// `item.remove`'s answer, and `global.remove`'s without `keep` (protocol 1.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRemoved {
    /// The source was deleted too: it was its last placement.
    pub source_deleted: bool,
    /// Every item removed in any scene, the named one first: a picture's companion
    /// items in the same scene go with it (protocol 1.88, ADR-0142). Absent before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_items: Vec<u32>,
    /// Every source deleted: the source at its last placement and companions left
    /// placed nowhere (protocol 1.88, ADR-0142). Absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_sources: Vec<u32>,
}

/// `global.list`'s answer (protocol 1.10): the global sources of the studio's canvas,
/// top first, as `source.list` rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalSourceList {
    /// One row per global source.
    pub sources: Vec<SourceInfo>,
}

/// `filter.add`'s answer (protocol 1.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterAdded {
    /// The new filter's id.
    pub filter: u32,
}

/// `output.profiles`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileList {
    /// The recording profiles.
    pub profiles: Vec<OutputProfile>,
}

/// `record.start`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStarted {
    /// The video encoder's name.
    pub video_encoder: String,
    /// The audio encoder's name, when audio is recorded.
    pub audio_encoder: Option<String>,
    /// The estimate of the plan the start decided to open (protocol 1.53, ADR-0087).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<EstimateSummary>,
    /// Marked sources left without a file, as `record.status` lists them (protocol
    /// 1.53).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedTrack>,
}

/// `stream.start`'s answer: the stream runs and connects in the background
/// (`stream.state` says when it reaches the server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStarted {
    /// The video encoder's name.
    pub video_encoder: String,
    /// The audio encoder's name, when audio is sent.
    pub audio_encoder: Option<String>,
}

/// `events.subscribe`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribed {
    /// The current `seq` per state key of the subscribed state topics.
    pub seq: SeqMap,
}

/// An access token as `tokens.list` shows it: never the token or its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// Its name.
    pub name: String,
    /// What it allows.
    pub scopes: ScopeSet,
    /// It has "Allow actions that cannot be undone" (protocol 1.80); an older server
    /// omits it, which reads as allowed.
    #[serde(default = "allowed")]
    pub irreversible: bool,
    /// Who made it (protocol 1.85, ADR-0134): `user`, or `ai_tools` for the AI tools
    /// token. An older server omits it, which reads as `user`.
    #[serde(default)]
    pub kind: crate::token::TokenKind,
}

/// The default of a token's permission in an answer from a server before 1.80.
fn allowed() -> bool {
    true
}

/// `tokens.list`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenList {
    /// The tokens, oldest first.
    pub tokens: Vec<TokenInfo>,
}

/// `tokens.create`'s answer: the only time the token exists in readable form.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreated {
    /// Its name as kept (trimmed).
    pub name: String,
    /// What it allows.
    pub scopes: ScopeSet,
    /// It has "Allow actions that cannot be undone" (protocol 1.80); an older server
    /// omits it, which reads as allowed.
    #[serde(default = "allowed")]
    pub irreversible: bool,
    /// The token's text (`rbs1_` and 64 hex digits).
    pub token: String,
}

impl std::fmt::Debug for TokenCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCreated")
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .field("irreversible", &self.irreversible)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A successful answer. Serialized as the JSON-RPC `result`; a client knows its type
/// from the method it called, so each result type deserializes on its own.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Response {
    /// `session.hello`.
    Hello(HelloResult),
    /// `session.authenticate`.
    Authenticated(AuthResult),
    /// `api.describe`.
    Describe(Description),
    /// `studio.get`.
    Studio(Snapshot<StudioInfo>),
    /// `source.list`.
    Sources(Snapshot<Vec<SourceInfo>>),
    /// `source.targets`.
    Targets(TargetList),
    /// `source.add`.
    SourceAdded(SourceAdded),
    /// `scene.list` (protocol 1.6).
    Scenes(Snapshot<SceneList>),
    /// `scene.add` (protocol 1.6).
    SceneAdded(SceneAdded),
    /// `group.create` (protocol 1.104).
    GroupCreated(GroupCreated),
    /// `group.list` (protocol 1.104).
    Groups(GroupList),
    /// `item.add` (protocol 1.6).
    ItemAdded(ItemAdded),
    /// `filter.types` (protocol 1.8).
    FilterTypes(FilterTypeList),
    /// `filter.add` (protocol 1.8).
    FilterAdded(FilterAdded),
    /// `scene.sources` (protocol 1.9).
    SceneSources(SceneSourceList),
    /// `global.list` (protocol 1.10).
    GlobalSources(GlobalSourceList),
    /// `source.get_settings` (protocol 1.17).
    SourceSettings(SourceSettingsInfo),
    /// `source.get_region` (protocol 1.41).
    SourceRegion(SourceRegionInfo),
    /// `scene.audio.get` (protocol 1.48).
    SceneLevels(SceneLevels),
    /// `scene.audio.set` (protocol 1.48).
    SceneLevelSet(SceneLevelSet),
    /// `source.get_chrome` (protocol 1.41).
    SourceChrome(SourceChromeInfo),
    /// `source.get_activity` (protocol 1.95, ADR-0152).
    SourceActivity(SourceActivityInfo),
    /// `look.list` (protocol 1.34).
    Looks(LookList),
    /// `scene.duplicate` (protocol 1.43; `sources` from 1.68).
    SceneDuplicated(crate::types::SceneDuplicated),
    /// `source.duplicate` (protocol 1.68).
    SourceDuplicated(crate::types::SourceDuplicated),
    /// `source.reset`, `source.paste_attributes` (protocol 1.68).
    AttributesApplied(crate::types::AttributesApplied),
    /// `source.copy_attributes`, `source.attributes_clipboard` (protocol 1.68); `null`
    /// while the clipboard is empty.
    AttributesClipboard(Option<crate::types::AttributesClipboard>),
    /// `text.fonts` (protocol 1.75).
    TextFonts(crate::types::TextFonts),
    /// `text.timer` (protocol 1.84).
    TextTimerState(crate::types::TextTimerState),
    /// `addon.list` (protocol 1.90).
    AddOnList(crate::types::AddOnList),
    /// `item.set_box` (protocol 1.34).
    BoxPlaced(BoxPlaced),
    /// `canvas.auto_layout` (protocol 1.44).
    AutoLayoutDone(AutoLayoutDone),
    /// `collection.set_transition` (protocol 1.89): empty, or a stinger warning.
    TransitionSet(TransitionSet),
    /// `marker.add` (protocol 1.50).
    MarkersAdded(MarkersAdded),
    /// `project.export` (protocol 1.58).
    ProjectExported(ProjectExported),
    /// `item.remove`, `global.remove` (protocol 1.10).
    ItemRemoved(ItemRemoved),
    /// `output.profiles`.
    Profiles(ProfileList),
    /// `record.start`, `record.start_to_file`.
    RecordStarted(RecordStarted),
    /// `record.start`, `record.stop`, `stream.start` and `stream.stop` with
    /// `canvas: "all"` (protocol 1.42); `record.pause`, `record.resume` and
    /// `record.pause_toggle` with `canvas: "all"` (protocol 1.61).
    Outputs(OutputsResult),
    /// `record.status` for a canvas other than the main one (protocol 1.29).
    RecordCanvasStatus(Snapshot<CanvasRecordState>),
    /// `record.status`.
    RecordStatus(Snapshot<RecordState>),
    /// `record.estimate` (protocol 1.53).
    RecordEstimate(EstimateAnswer),
    /// `stream.start`.
    StreamStarted(StreamStarted),
    /// `stream.status` for a canvas other than the main one (protocol 1.29).
    StreamCanvasStatus(Snapshot<CanvasStreamState>),
    /// `stream.status`.
    StreamStatus(Snapshot<StreamState>),
    /// `stream.destinations` (protocol 1.74).
    StreamDestinations(crate::types::StreamDestinations),
    /// `vcam.status` (protocol 1.12).
    VcamStatus(Snapshot<VcamStatus>),
    /// `replay.status` (protocol 1.27).
    ReplayStatus(Snapshot<ReplayStatus>),
    /// `protection.status` (protocol 1.49).
    ProtectionStatus(Snapshot<ProtectionStatus>),
    /// `calibration.status` (protocol 1.55).
    CalibrationStatus(Snapshot<CalibrationState>),
    /// `sync.measure_start` (protocol 1.66).
    SyncMeasureStarted(SyncMeasureStarted),
    /// `sync.devices` (protocol 1.66).
    SyncDevices(SyncDeviceList),
    /// Every `media.*` method (protocol 1.83).
    MediaState(MediaStateInfo),
    /// `sync.apply_to_device_sources` (protocol 1.66).
    SyncAppliedToDeviceSources(SyncAppliedToDeviceSources),
    /// `monitor.status` (protocol 1.30).
    MonitorStatus(Snapshot<MonitorStatus>),
    /// `monitor.devices` (protocol 1.30).
    MonitorDevices(MonitorDeviceList),
    /// `loudness.status` (protocol 1.47).
    LoudnessStatus(LoudnessStatus),
    /// `settings.get`, `settings.set`.
    Settings(Snapshot<Settings>),
    /// `profile.list`, `collection.list` (protocol 1.4).
    Library(Library),
    /// `import.obs.scan` (protocol 1.82).
    ObsScan(crate::types::ObsScan),
    /// `import.obs.check` (protocol 1.82).
    ObsCheck(crate::types::ObsCheck),
    /// `import.obs.run` (protocol 1.82).
    ObsRun(crate::types::ObsRun),
    /// `events.subscribe`.
    Subscribed(Subscribed),
    /// `tokens.list`.
    Tokens(TokenList),
    /// `tokens.create`.
    TokenCreated(TokenCreated),
    /// Methods with nothing to return: `source.delete`, `edit.undo`, `edit.redo`,
    /// `record.stop`, `record.pause`, `record.resume`, `record.pause_toggle` (one canvas), `stream.stop`, `vcam.start`, `vcam.stop`, `replay.arm`, `replay.disarm`, `replay.save`, the `monitor.set_*` methods, `source.set_monitor`, `marker.remove`, `events.unsubscribe`, `tokens.revoke`,
    /// `profile.switch`, `collection.switch`, and the protocol 1.6 edits other than
    /// `scene.add` and `item.add`.
    Done(Empty),
}
