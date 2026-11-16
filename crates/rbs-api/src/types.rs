//! The data the first method slice reads and writes: studio, sources, capture targets,
//! recording profiles, recording and stream state and settings. Plain serializable values with no
//! engine types, so clients in any language get them from the JSON Schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A change counter per state key (`"record.state"`, later `"source.changed:7"`).
pub type SeqMap = BTreeMap<String, u64>;

/// A state query's answer: the data and the `seq` of each state key it includes, so a
/// client ignores events with an equal or lower `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot<T> {
    /// `seq` per state key included in `data`.
    pub seq: SeqMap,
    /// The state.
    pub data: T,
}

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// The canvas's size and frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator.
    pub fps_den: u32,
}

/// A kind of source the user can add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// A picture: a file or a built-in sample (protocol 1.34, ADR-0075).
    Image,
    /// A solid colour (protocol 1.34, ADR-0075).
    Color,
    /// A local media file (protocol TBD, ADR-0108).
    Media,
    /// A browser page from the helper process (protocol TBD, ADR-0109).
    Browser,
    /// A text drawn by the server's text renderer (protocol 1.75, ADR-0129).
    Text,
}

/// Why a source cannot deliver (protocol 1.20, ADR-0077). A client treats a value it does
/// not know as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFault {
    /// The target is gone: a window closed, a device removed.
    TargetGone,
    /// The system refused access, or the user has not decided yet.
    PermissionDenied,
    /// Another application holds the device.
    Busy,
    /// Any other cause.
    #[serde(other)]
    Other,
}

/// How a source is doing.
///
/// Serialised with a `state` tag. A client built for an older protocol treats a `state`
/// it does not know as `failed`; this decoder does the same, taking the words from
/// `reason` or `detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceStatus {
    /// It runs.
    Running,
    /// It is not shown or played, so it is not opened.
    Idle,
    /// It is not running.
    Failed {
        /// Why, in words.
        reason: String,
    },
    /// It cannot deliver because its target is gone, refused or busy (protocol 1.20).
    Unavailable {
        /// The typed cause.
        fault: SourceFault,
        /// The adapter's own text.
        detail: String,
    },
}

impl<'de> Deserialize<'de> for SourceStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            state: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            fault: Option<SourceFault>,
            #[serde(default)]
            detail: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(match raw.state.as_str() {
            "running" => SourceStatus::Running,
            "idle" => SourceStatus::Idle,
            "unavailable" => SourceStatus::Unavailable {
                fault: raw.fault.unwrap_or(SourceFault::Other),
                detail: raw.detail.or(raw.reason).unwrap_or_default(),
            },
            // "failed" and any state a newer server adds.
            _ => SourceStatus::Failed {
                reason: raw.reason.or(raw.detail).unwrap_or_default(),
            },
        })
    }
}

/// One source of the program scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Its id in the scene collection.
    pub id: u32,
    /// Its name, unique in the collection.
    pub name: String,
    /// Its kind; `null` for a type this server cannot name. `image` and `color` are
    /// picture sources (protocol 1.34).
    pub kind: Option<SourceKind>,
    /// How it is doing.
    pub status: SourceStatus,
    /// The id of its item in the program scene (protocol 1.3; absent before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u32>,
    /// Where its picture is on the studio's canvas; absent for sound-only sources and
    /// before protocol 1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<ItemLayout>,
    /// Width and height in pixels of the picture it delivers, as last seen while it ran
    /// (protocol 1.5, ADR-0058); absent for sound-only sources and before it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_size: Option<[u32; 2]>,
    /// The item is shown (protocol 1.6, ADR-0059; absent before, meaning shown).
    #[serde(default = "shown", skip_serializing_if = "is_shown")]
    pub visible: bool,
    /// The item is locked on the picture panel (protocol 1.6).
    #[serde(default, skip_serializing_if = "is_unlocked")]
    pub locked: bool,
    /// The source's volume, a linear factor from 0 to 1 (protocol 1.6).
    #[serde(default = "full_volume")]
    pub volume: f32,
    /// The source's sound is muted (protocol 1.6).
    #[serde(default)]
    pub muted: bool,
    /// Its filters in chain order (protocol 1.8, ADR-0062; absent when none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<FilterInfo>,
    /// Sync offset of its sound in milliseconds (protocol 1.8; absent when 0).
    #[serde(default, skip_serializing_if = "is_zero_ms")]
    pub sync_offset_ms: i32,
    /// The source is kept out of the Mixer but still heard (protocol 1.14, ADR-0080;
    /// absent before, meaning shown).
    #[serde(default, skip_serializing_if = "is_false")]
    pub mixer_hidden: bool,
    /// The picture source this one is the "Capture audio too" companion of (protocol
    /// 1.26, ADR-0096); absent for other sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion_of: Option<u32>,
    /// Whether the source is heard on the monitor output (protocol 1.30, ADR-0097;
    /// absent when off).
    #[serde(default, skip_serializing_if = "MonitorMode::is_off")]
    pub monitor: MonitorMode,
    /// Per-source slide override (protocol 1.35, ADR-0100); absent when the collection
    /// defaults apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glide: Option<Glide>,
    /// The source is marked to record separately (protocol 1.46, ADR-0086); absent
    /// when it is not.
    #[serde(default, skip_serializing_if = "is_false")]
    pub record_separately: bool,
    /// Which feeds are marked, and the picture's form (protocol 1.52, ADR-0117);
    /// absent when none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_tracks: Option<RecordTracks>,
    /// What the mix hears while the program scene, in Own levels, has an entry for the
    /// source (protocol 1.48, ADR-0091); absent otherwise, when `volume` and `muted` are
    /// what is heard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heard: Option<Heard>,
    /// `L`: the latency of the source's running audio filters, in milliseconds
    /// (protocol 1.77, ADR-0130; absent when 0).
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub filter_latency_ms: f64,
    /// `h`: the offset the program hears the source's sound at, in milliseconds: the
    /// input's effective offset plus `filter_latency_ms` (protocol 1.77, ADR-0130
    /// decision 4). It equals `sync_offset_ms` while the input's limits and the filter
    /// compensation do not bite. Absent when 0.
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub effective_offset_ms: f64,
    /// The part of `filter_latency_ms` the input's advance limit left uncompensated,
    /// in milliseconds; for a source without an audio input (media, browser audio)
    /// all of it (protocol 1.77; absent when 0).
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub uncompensated_ms: f64,
    /// Output frames of the source's audio input that were silence because its audio
    /// had not arrived (protocol 1.77). Only in `source.list` answers, and only for a
    /// source with a running audio input; the count only grows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_late_frames: Option<u64>,
    /// Times the source's audio input resynchronised (protocol 1.77); present like
    /// `input_late_frames`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_resyncs: Option<u64>,
}

/// The volume and mute a source is heard at (protocol 1.48, ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Heard {
    /// A linear factor from 0 to 1.
    pub volume: f32,
    /// Muted.
    pub muted: bool,
}

/// Whether a source is heard on the monitor output (protocol 1.30, ADR-0097).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorMode {
    /// Not monitored.
    #[default]
    Off,
    /// Monitored and taken off the stream and recordings while a monitor plays.
    Only,
    /// Monitored and mixed.
    Also,
}

impl MonitorMode {
    #[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
    fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// `loudness.status` (protocol 1.47, ADR-0092 stage 1): the loudness of the main
/// canvas's mix after ITU-R BS.1770-4 and EBU R 128. Each value is `null` while it has
/// no data: the meter is off, its window has not filled since it started or was
/// reset, or the programme so far is below the -70 LUFS gate.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LoudnessStatus {
    /// The meter runs.
    pub enabled: bool,
    /// Momentary loudness (400 ms), LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub momentary: Option<f32>,
    /// Short-term loudness (3 s), LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_term: Option<f32>,
    /// Integrated loudness since the meter started or was reset, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrated: Option<f32>,
    /// Loudness range (EBU Tech 3342) over the same time, LU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<f32>,
    /// Highest true peak over the same time, dBTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_peak_max: Option<f32>,
    /// Highest momentary loudness over the same time, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub momentary_max: Option<f32>,
    /// Highest short-term loudness over the same time, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_term_max: Option<f32>,
    /// The saved target, LUFS (protocol 1.59; `settings.get` `loudness`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<f32>,
    /// The saved true-peak limit, dBTP (protocol 1.59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_peak_limit: Option<f32>,
}

/// `monitor.status` and `monitor.changed` (protocol 1.30, ADR-0097).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    /// Some audio adapter can play to a device on this computer.
    pub available: bool,
    /// The chosen device (`monitor.devices` `device`); `null`: System default.
    pub device: Option<String>,
    /// The monitor volume, a linear factor from 0 to 1.
    pub volume: f32,
    /// A source is monitored or soloed, so the device should play.
    pub wanted: bool,
    /// The device plays the monitor mix.
    pub playing: bool,
    /// How far the monitor plays behind real time, in milliseconds, while it plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// The soloed sources (session state, never saved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub solo: Vec<u32>,
    /// Why the device failed to open or play, until it plays again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One playback device of `monitor.devices` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDevice {
    /// Its id, for `monitor.set_device`.
    pub device: String,
    /// The name the system gives it.
    pub name: String,
    /// The system's default output.
    pub is_default: bool,
}

/// `monitor.devices` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDeviceList {
    /// The playback devices, best adapter first.
    pub devices: Vec<MonitorDevice>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_false(v: &bool) -> bool {
    !*v
}

fn default_pcm_bits() -> u16 {
    16
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_pcm_16(bits: &u16) -> bool {
    *bits == 16
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_zero_ms(v: &i32) -> bool {
    *v == 0
}

fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

/// One filter of a source (protocol 1.8, ADR-0062).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterInfo {
    /// Its id, unique in the collection.
    pub id: u32,
    /// Its type, `org.rbs.filter.<name>`.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Applied or not.
    pub enabled: bool,
    /// Its settings as it runs them on air (every field of a built-in type).
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// `filter.types`' answer (protocol 1.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterTypeList {
    /// The built-in filter types, in the order the app lists them.
    pub types: Vec<FilterTypeInfo>,
}

/// A filter type and its settings schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterTypeInfo {
    /// `org.rbs.filter.<name>` for a video filter, `org.rbs.audio-filter.<name>` for an
    /// audio filter.
    #[serde(rename = "type")]
    pub type_id: String,
    /// `"video"` or `"audio"` (protocol 1.16, ADR-0082); a 1.8 to 1.15 server omits it,
    /// which means video.
    #[serde(default = "video_domain")]
    pub domain: String,
    /// English display name.
    pub name: String,
    /// Its settings, in display order.
    pub fields: Vec<FilterField>,
    /// The delay, in milliseconds, an instance adds at the running audio graph's format
    /// and block size (protocol 1.77, ADR-0130 decision 2); the studio compensates it
    /// on sources with an audio input. Present for audio types, absent for video types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
}

fn video_domain() -> String {
    "video".to_owned()
}

/// One option of a `"choice"` setting (protocol 1.16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOption {
    /// The value stored in settings.
    pub key: String,
    /// English label.
    pub label: String,
}

/// One setting of a filter type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterField {
    /// Settings key.
    pub key: String,
    /// English label.
    pub label: String,
    /// `"number"`, `"color"`, `"toggle"`, `"choice"` (the last two since protocol 1.16) or `"source"` (protocol 1.19).
    pub kind: String,
    /// One sentence that explains the setting (protocol 1.16); absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// `"linear"` or `"log"` for a number (protocol 1.16); absent for other kinds. A
    /// log number is meant for a log-scaled slider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<String>,
    /// The options of a `"choice"` (protocol 1.16); absent for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FilterOption>>,
    /// Smallest number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    /// Largest number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
    /// The UI's increment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f32>,
    /// Shown after the value; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The default: a number, a `"#rrggbb"` / `"#rrggbbaa"` string, a boolean for a
    /// toggle, or an option key for a choice.
    pub default: serde_json::Value,
}

fn shown() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_shown(v: &bool) -> bool {
    *v
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_unlocked(v: &bool) -> bool {
    !*v
}

fn full_volume() -> f32 {
    1.0
}

/// A scene as `scene.list` lists it (protocol 1.6, ADR-0059).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneInfo {
    /// Its id in the scene collection.
    pub id: u32,
    /// Its name, unique in the collection.
    pub name: String,
    /// The scene keeps its own levels (protocol 1.48, ADR-0091); absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own_audio: bool,
}

/// How a slide maps unit time (protocol 1.35, ADR-0100). Fade-ins stay linear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GlideEasing {
    /// Straight lerp.
    #[serde(rename = "linear")]
    Linear,
    /// Smoothstep ease in and out.
    #[serde(rename = "ease-in-out")]
    #[default]
    EaseInOut,
}

/// Collection or source glide timing (protocol 1.35, ADR-0100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Glide {
    /// Milliseconds in `0..=5000`. `0` jumps.
    pub duration_ms: u32,
    /// Slide easing; fade-ins use the duration only.
    pub easing: GlideEasing,
}

impl Default for Glide {
    fn default() -> Self {
        Self {
            duration_ms: 400,
            easing: GlideEasing::EaseInOut,
        }
    }
}

impl Glide {
    /// True for the shipped default (400 ms, ease-in-out).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A source of the collection, wherever it is placed (protocol 1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Its id.
    pub id: u32,
    /// Its name.
    pub name: String,
    /// Its kind; `null` for a type this server cannot name. `image` and `color` are
    /// picture sources (protocol 1.34).
    pub kind: Option<SourceKind>,
    /// A global source of the studio's canvas (protocol 1.10, ADR-0065): `item.add`
    /// refuses it. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub global: bool,
    /// Per-source slide override (protocol 1.35, ADR-0100); absent when the collection
    /// defaults apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glide: Option<Glide>,
}

/// The studio canvas's master volume and mute (protocol 1.15, ADR-0081).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Master {
    /// A linear factor from 0 to 1.
    pub volume: f32,
    /// Muted; the volume is kept.
    pub muted: bool,
}

impl Default for Master {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

impl Master {
    /// True for full volume, not muted (left out of the answer).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `scene.list` and `scene.changed` (protocol 1.6): the scenes the studio's canvas may
/// show in the Scenes panel's order, the one on air, and every source of the
/// collection by name (what `item.add` can place).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneList {
    /// The scenes.
    pub scenes: Vec<SceneInfo>,
    /// The id of the scene on air.
    pub program: u32,
    /// The preview scene in studio mode (protocol 1.9, ADR-0061); absent outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<u32>,
    /// The canvas's master volume and mute (protocol 1.15, ADR-0081); absent when it is
    /// 1.0 and not muted.
    #[serde(default, skip_serializing_if = "Master::is_default")]
    pub master: Master,
    /// Collection morph defaults (protocol 1.35, ADR-0100); absent when 400 ms and
    /// ease-in-out.
    #[serde(default, skip_serializing_if = "Glide::is_default")]
    pub glide: Glide,
    /// Every source of the collection.
    pub sources: Vec<SourceEntry>,
}

/// A picture's place on the canvas (protocol 1.3): the layout the studio's canvas
/// shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemLayout {
    /// Top-left corner of the unrotated box, canvas pixels.
    pub position: [f32; 2],
    /// Box size in canvas pixels; negative flips.
    pub size: [f32; 2],
    /// Clockwise rotation in degrees about the box centre.
    pub rotation: f32,
    /// Source pixels cut from the left, top, right and bottom.
    pub crop: [u32; 4],
    /// 0 to 1.
    pub opacity: f32,
    /// Shape, border and shadow (ADR-0053).
    pub style: ItemStyle,
}

/// The outline of a picture inside its box (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    /// The box, with corners rounded by `radius`.
    #[default]
    Rectangle,
    /// The largest circle centred in the box.
    Circle,
    /// The ellipse that fills the box.
    Ellipse,
    /// An isosceles triangle, tip on the top edge of the box.
    Triangle,
    /// An arrow pointing toward the right edge of the box.
    Arrow,
}

/// Shape, border and shadow of a picture (ADR-0053). Lengths are in the item's box
/// units (canvas pixels for an item on the canvas); colours are `#rrggbb` or
/// `#rrggbbaa`, sRGB with straight alpha. `{}` is the plain picture.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemStyle {
    /// The outline.
    #[serde(default)]
    pub shape: ShapeKind,
    /// Corner radius; only for `rectangle`.
    #[serde(default)]
    pub radius: f32,
    /// A line inside the shape's edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<StyleBorder>,
    /// A shadow under the shape; saved, not drawn yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<StyleShadow>,
}

/// A border (ADR-0053).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleBorder {
    /// Width, more than 0.
    pub width: f32,
    /// `#rrggbb` or `#rrggbbaa`.
    pub color: String,
}

/// A shadow (ADR-0053).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleShadow {
    /// Offset, x right and y down.
    #[serde(default)]
    pub offset: [f32; 2],
    /// Blur radius, not negative.
    #[serde(default)]
    pub blur: f32,
    /// `#rrggbb` or `#rrggbbaa`.
    pub color: String,
}

/// `#rrggbb` (opaque) or `#rrggbbaa` as red, green, blue, alpha; `None` otherwise.
pub fn parse_color(text: &str) -> Option<[u8; 4]> {
    let hex = text.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha])
}

/// `#rrggbbaa`.
pub fn format_color(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

/// Something a source can capture: a display, window, camera or audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTarget {
    /// Name from the OS (a window title can be private: `source.targets` needs
    /// `control`).
    pub name: String,
    /// The adapter that listed it.
    pub adapter: String,
    /// Its id for that adapter.
    pub target: String,
    /// The OS default of its kind (audio endpoints).
    pub is_default: bool,
    /// Native size when known (displays and windows).
    pub size: Option<Dimensions>,
}

/// A video encoder the user can pick, with what its quality settings can be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderInfo {
    /// Its adapter id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// `cbr`, `cbr_padded`, `vbr`, `constant_quality`.
    pub rate_controls: Vec<String>,
    /// H.264 profile names.
    pub h264_profiles: Vec<String>,
    /// Most B-frames.
    pub b_frames_max: u32,
}

/// The studio: canvas, what can be added, encoders, whether recording is possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioInfo {
    /// The canvas.
    pub canvas: Canvas,
    /// Kinds of source the adapters can add.
    pub kinds: Vec<SourceKind>,
    /// Video encoders, best first.
    pub encoders: Vec<EncoderInfo>,
    /// Why this system cannot record, or `null` when it can.
    pub cannot_record: Option<String>,
}

/// A recording container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    /// Matroska (`.mkv`).
    Mkv,
    /// Fragmented MP4 (`.mp4`).
    Mp4,
    /// WAV (`.wav`): the mix as PCM, with no picture (protocol 1.42).
    Wav,
    /// FLAC (`.flac`): the mix, lossless, with no picture (protocol 1.43).
    Flac,
    /// QuickTime (`.mov`), written fragmented like MP4 (protocol 1.51, ADR-0085).
    Mov,
}

/// How the encoder spends bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateMode {
    /// Constant bitrate.
    Cbr,
    /// Variable bitrate with a peak.
    Vbr,
    /// Constant quality.
    ConstantQuality,
}

/// Speed versus quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// Best quality the machine sustains.
    Quality,
    /// The default.
    Balanced,
    /// Least GPU use.
    Speed,
}

/// A recording profile as `output.profiles` lists it (`read`): no folder, which only
/// `settings.get` (`admin`) shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputProfile {
    /// Its id, passed to `record.start`.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The container.
    pub container: Container,
    /// The video encoder's adapter id, or `null` for the best one.
    pub encoder: Option<String>,
    /// Rate control.
    pub rate_control: RateMode,
    /// Video bitrate in kbit/s (CBR rate or VBR target).
    pub video_kbps: u32,
    /// Audio bitrate in kbit/s.
    pub audio_kbps: u32,
}

/// Every setting of a recording profile (`settings.get`, `settings.set`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingProfile {
    /// Its id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Where recordings are written (absolute).
    pub folder: String,
    /// The container.
    pub container: Container,
    /// The video encoder's adapter id, or `null` for the best one.
    pub encoder: Option<String>,
    /// H.264, HEVC or AV1. Omitted when H.264, so a 1.33 client still reads the object.
    /// `"av1"` is protocol 1.40.
    #[serde(default, skip_serializing_if = "VideoCodec::is_h264")]
    pub video_codec: VideoCodec,
    /// Rate control.
    pub rate_control: RateMode,
    /// Video bitrate in kbit/s: the CBR rate, or the VBR target.
    pub video_kbps: u32,
    /// VBR peak in kbit/s; `null` is 1.5 times the target.
    pub max_video_kbps: Option<u32>,
    /// Constant-quality level, 0 (best) to 51.
    pub quality: u8,
    /// Seconds between keyframes.
    pub keyframe_seconds: u32,
    /// Codec profile (`high`, `main`, `baseline`); `null` lets the encoder pick.
    pub profile: Option<String>,
    /// Speed versus quality.
    pub preset: Preset,
    /// Most B-frames.
    pub b_frames: u32,
    /// With CBR, pad the video to its full bitrate.
    pub cbr_padding: bool,
    /// Audio bitrate in kbit/s.
    pub audio_kbps: u32,
    /// Sample width of a WAV or FLAC recording. WAV is 16, 24 or 32 (float). FLAC
    /// is 16 or 24. Omitted when 16 (protocol 1.42; FLAC is protocol 1.43).
    #[serde(default = "default_pcm_bits", skip_serializing_if = "is_pcm_16")]
    pub bit_depth: u16,
    /// Colour encoding. Omitted when [`Delivery::Video`] (protocol 1.37).
    #[serde(default, skip_serializing_if = "Delivery::is_video")]
    pub delivery: Delivery,
    /// Lifts only the darkest tones on a Video encode. Omitted when off (protocol 1.38).
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_shadows: bool,
    /// Encoded picture size. Omitted or `null`: the same as this recording's canvas
    /// (protocol 1.39, ADR-0102).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size: Option<Dimensions>,
    /// Write an editor project next to the recording when it stops (protocol 1.58,
    /// ADR-0089 decision 10). Omitted or `null`: off. Also what makes a plain
    /// recording keep its files' timing for a later `project.export`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_project: Option<ProjectFormat>,
    /// Markers: the reaction-time offset and the automatic markers (protocol 1.62,
    /// ADR-0089 decisions 3 and 4). Absent: the defaults.
    #[serde(default, skip_serializing_if = "RecordingMarkers::is_default")]
    pub markers: RecordingMarkers,
}

/// A recording profile's markers (protocol 1.62, ADR-0089 decisions 3 and 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingMarkers {
    /// Moves a manual marker this many milliseconds earlier, 0 to 30 000.
    pub lead_ms: u32,
    /// A marker each time the program scene changes (on by default).
    pub scene_changes: bool,
    /// A marker when a stream starts and when it ends (off by default).
    pub stream_events: bool,
    /// One marker per pause on its join in the editor project (on by default;
    /// protocol 1.63, ADR-0124 decision 10).
    pub pause_markers: bool,
}

impl Default for RecordingMarkers {
    fn default() -> Self {
        Self {
            lead_ms: 0,
            scene_changes: true,
            stream_events: false,
            pause_markers: true,
        }
    }
}

impl RecordingMarkers {
    /// The defaults, which the wire omits.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// An editor project format (protocol 1.58, ADR-0089 decision 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFormat {
    /// FCPXML 1.9, one `.fcpxml` file (Final Cut Pro).
    Fcpxml,
    /// FCP7 XML (xmeml version 5), one `.xml` file (DaVinci Resolve, Premiere Pro;
    /// protocol 1.60).
    Xmeml,
}

/// The video codec of a recording or a stream. H.264 is omitted on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// H.264.
    #[default]
    H264,
    /// HEVC (H.265).
    Hevc,
    /// AV1 (protocol 1.40). A 1.39 client treats the value as unknown.
    Av1,
}

impl VideoCodec {
    /// True for H.264, so the field can be omitted on the wire.
    fn is_h264(codec: &Self) -> bool {
        *codec == Self::H264
    }
}

/// How a recording or a stream encodes colour (protocol 1.37). Omitted when
/// [`Delivery::Video`], so an older file and a 1.36 client stay on Rec. 709 limited range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Rec. 709 camera encoding, limited range. YouTube, TV and streams.
    #[default]
    Video,
    /// sRGB transfer, full range. Matches a desktop display.
    Screen,
}

impl Delivery {
    /// True for [`Delivery::Video`], so the field can be omitted on the wire.
    fn is_video(delivery: &Self) -> bool {
        *delivery == Self::Video
    }
}

/// How a stream is encoded (`settings.get`, `settings.set`; protocol 1.2). The same
/// fields as a [`RecordingProfile`]'s encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoding {
    /// The video encoder's adapter id, or `null` for the best one.
    pub encoder: Option<String>,
    /// H.264, HEVC or AV1. Omitted when H.264, so a 1.33 client still reads the object.
    /// `"av1"` is protocol 1.40.
    #[serde(default, skip_serializing_if = "VideoCodec::is_h264")]
    pub video_codec: VideoCodec,
    /// Rate control.
    pub rate_control: RateMode,
    /// Video bitrate in kbit/s: the CBR rate, or the VBR target.
    pub video_kbps: u32,
    /// VBR peak in kbit/s; `null` is 1.5 times the target.
    pub max_video_kbps: Option<u32>,
    /// Constant-quality level, 0 (best) to 51.
    pub quality: u8,
    /// Seconds between keyframes.
    pub keyframe_seconds: u32,
    /// Codec profile (`high`, `main`, `baseline`); `null` lets the encoder pick.
    pub profile: Option<String>,
    /// Speed versus quality.
    pub preset: Preset,
    /// Most B-frames.
    pub b_frames: u32,
    /// With CBR, pad the video to its full bitrate.
    pub cbr_padding: bool,
    /// Audio bitrate in kbit/s.
    pub audio_kbps: u32,
    /// Colour encoding. Omitted when [`Delivery::Video`] (protocol 1.37).
    #[serde(default, skip_serializing_if = "Delivery::is_video")]
    pub delivery: Delivery,
    /// Lifts only the darkest tones on a Video encode. Omitted when off (protocol 1.38).
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_shadows: bool,
}

/// The stream settings (`settings.get`; protocol 1.2). The server's address is not
/// part of them: only the local user enters it, in the app, so an API client can neither
/// read it nor send the program and the stream key elsewhere. The key is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSettings {
    /// The saved server's protocol, or `null` when no server is saved.
    pub protocol: Option<StreamProtocol>,
    /// A stream key is saved in the system's credential store.
    pub key_saved: bool,
    /// Reconnect when the connection drops.
    pub reconnect: bool,
    /// SRT latency in ms; `null`: the transport's default (120 ms).
    pub srt_latency_ms: Option<u16>,
    /// How the stream is encoded.
    pub encoding: Encoding,
    /// Record when the stream starts, with the recording settings; ending the stream
    /// stops that recording (protocol 1.11; `false` from older servers).
    #[serde(default)]
    pub auto_record: bool,
    /// Encoded picture size. Omitted or `null`: the same as this stream's canvas
    /// (protocol 1.39, ADR-0102). Not inside `encoding`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size: Option<Dimensions>,
    /// What `stream.hold` and Break send when they name no picture (protocol 1.65,
    /// ADR-0124); `freeze` from older servers.
    #[serde(default)]
    pub hold_picture: crate::request::HoldPicture,
    /// The scene a hold with `picture: "scene"` shows when it names none (protocol
    /// 1.67, ADR-0124 phase 2b); `null` when none is set.
    #[serde(default)]
    pub hold_scene: Option<u32>,
    /// The sound such a hold sends when it names none (protocol 1.67); `silence` from
    /// older servers.
    #[serde(default)]
    pub hold_sound: crate::request::HoldSound,
}

/// The stream settings an API client may change (`settings.set`; protocol 1.2): all of
/// them but the server and the key. Unknown fields are ignored, so the
/// [`StreamSettings`] that `settings.get` returned can be sent back changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSettingsPatch {
    /// Reconnect when the connection drops.
    pub reconnect: bool,
    /// SRT latency in ms (1 or more); `null`: the transport's default.
    #[serde(default)]
    pub srt_latency_ms: Option<u16>,
    /// How the stream is encoded.
    pub encoding: Encoding,
    /// Record when the stream starts (protocol 1.11); absent: unchanged, so a 1.2
    /// client's patch never turns it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_record: Option<bool>,
    /// Encoded picture size (protocol 1.39). Absent or `null` stores the same as this
    /// stream's canvas, which clears a custom size. Not inside `encoding`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size: Option<Dimensions>,
    /// What a hold sends when it names no picture (protocol 1.65); absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_picture: Option<crate::request::HoldPicture>,
    /// The hold scene (protocol 1.67): absent leaves it unchanged, `null` clears it.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub hold_scene: Option<Option<u32>>,
    /// The hold sound (protocol 1.67); absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_sound: Option<crate::request::HoldSound>,
}

/// A field that is present, `null` included: `Some(value)`. An absent field takes the
/// `default` (`None`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Settings the control thread owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Recording profiles.
    pub profiles: Vec<RecordingProfile>,
    /// The canvas.
    pub canvas: Canvas,
    /// The stream settings (protocol 1.2); absent while the app is still starting and
    /// from servers that do not stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSettings>,
    /// Whether Record and Go live also act on the vertical outputs (protocol 1.42;
    /// read only here, set with `output.set_with_vertical`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<VerticalWithMain>,
    /// The loudness meter's target and true-peak limit (protocol 1.59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<LoudnessSettings>,
}

/// What a recording is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecordState {
    /// Nothing records.
    Idle {
        /// How the last recording of this server run ended, if one ran.
        last: Option<RecordingSummary>,
    },
    /// A recording runs.
    Recording {
        /// The profile it uses, or `null` for an explicit file.
        profile: Option<String>,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is recorded.
        audio_encoder: Option<String>,
        /// The separate files written next to the master (protocol 1.46, ADR-0086);
        /// absent when there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tracks: Vec<RecordTrack>,
        /// Marked sources whose file was not written (protocol 1.46); absent when
        /// none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<SkippedTrack>,
        /// Paused, or a pause asked for has not begun yet (protocol 1.61,
        /// ADR-0124). `state` stays `recording` while paused.
        #[serde(default)]
        paused: bool,
        /// Time left out by pauses so far, in milliseconds, as of this state (an
        /// open pause counted up to the change) (protocol 1.61).
        #[serde(default)]
        paused_ms: u64,
        /// Time written to the file so far, in milliseconds, as of this state: the
        /// time since the start less `paused_ms` (protocol 1.61).
        #[serde(default)]
        recorded_ms: u64,
    },
}

/// A marked source that has no file in a running recording (protocol 1.46).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedTrack {
    /// The source (`source.list`).
    pub source: u32,
    /// The feed that was not written.
    pub role: TrackRole,
    /// Why.
    pub reason: SkipReason,
}

/// The feeds of a source marked to record separately (protocol 1.52).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordTracks {
    /// Its picture.
    #[serde(default, skip_serializing_if = "is_false")]
    pub video: bool,
    /// Its sound.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audio: bool,
    /// The picture's form; `auto` when absent.
    #[serde(default, skip_serializing_if = "PictureForm::is_auto")]
    pub video_as: PictureForm,
}

/// The form of a marked picture in a source row (protocol 1.52). The request side is
/// `request::VideoAs`; this one reads a later server's value as `other`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PictureForm {
    /// Images for a still source, a video file otherwise.
    #[default]
    Auto,
    /// A video file.
    Video,
    /// PNG images.
    Image,
    /// A form this client does not know (a later minor).
    #[serde(other)]
    Other,
}

impl PictureForm {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

/// Why a marked source has no file (protocol 1.46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The program mix does not hear the source when the recording starts.
    NotHeard,
    /// More video files than this version writes beside the recording (protocol
    /// 1.52; a fixed number until the ADR-0087 estimate).
    Limit,
    /// The source has no picture when the recording starts (protocol 1.52).
    NoPicture,
    /// The recording has no picture (WAV or FLAC), so a video file has no encoder
    /// settings to match (protocol 1.52).
    AudioOnly,
    /// The file could not be opened (protocol 1.52).
    OpenFailed,
    /// A reason this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// One separate file of a running recording (protocol 1.46, ADR-0086).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordTrack {
    /// The file's name inside the recording's folder. No directory. For `image`, the
    /// first image (`<name>.png`; later ones are `<name>-2.png`, ...).
    pub file: String,
    /// What it carries. Only `audio` in 1.46; `video` and `image` from 1.52.
    pub role: TrackRole,
    /// The source it records (`source.list`).
    pub source: u32,
}

/// What a separate file carries (protocol 1.46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRole {
    /// The source's picture as a video file (written from 1.52).
    Video,
    /// The source's sound.
    Audio,
    /// A still source's picture as PNG images (protocol 1.52, ADR-0117).
    Image,
    /// A role this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// The canvas an output records or streams (protocol 1.29, ADR-0093 decision 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCanvas {
    /// The studio's main canvas.
    #[default]
    Main,
    /// The vertical canvas (`canvas.add_vertical`).
    Vertical,
}

impl OutputCanvas {
    /// Every canvas an output can use, main first.
    pub const ALL: [OutputCanvas; 2] = [OutputCanvas::Main, OutputCanvas::Vertical];

    /// Its number in the studio's set of canvases: the object id of its
    /// `record.canvas_state` and `stream.canvas_state` keys.
    pub const fn object(self) -> u32 {
        match self {
            OutputCanvas::Main => 0,
            OutputCanvas::Vertical => 1,
        }
    }
}

/// The canvases `record.start`, `record.stop`, `stream.start` and `stream.stop` act on:
/// one canvas, or every canvas the studio runs, main first (`all`, protocol 1.42,
/// ADR-0111 decision 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTarget {
    /// The studio's main canvas.
    #[default]
    Main,
    /// The vertical canvas.
    Vertical,
    /// Every canvas the studio runs, main first (protocol 1.42).
    All,
}

impl OutputTarget {
    /// The one canvas asked for; `None` for [`OutputTarget::All`].
    pub const fn single(self) -> Option<OutputCanvas> {
        match self {
            OutputTarget::Main => Some(OutputCanvas::Main),
            OutputTarget::Vertical => Some(OutputCanvas::Vertical),
            OutputTarget::All => None,
        }
    }
}

impl From<OutputCanvas> for OutputTarget {
    fn from(c: OutputCanvas) -> Self {
        match c {
            OutputCanvas::Main => OutputTarget::Main,
            OutputCanvas::Vertical => OutputTarget::Vertical,
        }
    }
}

/// What a call with `canvas: "all"` did with one canvas's output (protocol 1.42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputOutcome {
    /// Start: it started now.
    Started,
    /// Start: it was already starting or running.
    Running,
    /// Stop: it was stopped now.
    Stopped,
    /// Stop: it was not running (idle, or still ending). Pause and resume: no
    /// recording runs (protocol 1.61).
    NotRunning,
    /// Pause: it was paused now (protocol 1.61).
    Paused,
    /// Resume: it was resumed now (protocol 1.61); for a stream, back on the
    /// program (protocol 1.64).
    Resumed,
    /// Hold: the stream was put on hold now (protocol 1.64).
    Held,
    /// Start of a vertical stream with no server saved: skipped, not an error.
    NotConfigured,
    /// Start: refused; `error` says why. Pause and resume: refused (already in the
    /// asked state, or the pause cannot be made); `error` says why (protocol 1.61).
    Refused,
}

/// The error object of a refused entry: the same `code`, `message` and `data.kind` a
/// single-canvas call would answer with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputError {
    /// The JSON-RPC error code.
    pub code: i32,
    /// What went wrong, in words.
    pub message: String,
    /// `{ "kind": ... }`.
    pub data: OutputErrorData,
}

/// `data` of an [`OutputError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputErrorData {
    /// The machine-readable error.
    pub kind: crate::error::ErrorKind,
    /// The members a single-canvas call adds beside `kind` (protocol 1.54), such as a
    /// refused start's `reason`, `estimate` and `tracks`.
    #[serde(flatten)]
    pub more: serde_json::Map<String, serde_json::Value>,
}

impl From<&crate::error::ApiError> for OutputError {
    fn from(e: &crate::error::ApiError) -> Self {
        let mut more = e.wire_data();
        more.remove("kind");
        Self {
            code: e.kind.code(),
            message: e.message.clone(),
            data: OutputErrorData { kind: e.kind, more },
        }
    }
}

/// One canvas's entry in an `all` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    /// The canvas.
    pub canvas: OutputCanvas,
    /// Which output of the canvas, on `output.break_toggle` only (protocol 1.64),
    /// which answers for recordings and streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<OutputKind>,
    /// What happened.
    pub outcome: OutputOutcome,
    /// On `started`: the video encoder's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encoder: Option<String>,
    /// On `started`: the audio encoder's name, absent when no audio is encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encoder: Option<String>,
    /// On `refused`: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OutputError>,
    /// On a started main recording: the estimate of its plan (protocol 1.53).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<EstimateSummary>,
    /// On a started main recording: marked sources left without a file (protocol
    /// 1.53).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedTrack>,
}

/// The answer of `record.start`, `record.stop`, `stream.start` and `stream.stop` with
/// `canvas: "all"` (protocol 1.42): one entry per canvas, main first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputsResult {
    /// The entries.
    pub outputs: Vec<OutputEntry>,
}

/// Which output kind `output.set_with_vertical` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    /// Recording.
    Record,
    /// Stream.
    Stream,
}

/// Whether Record and Go live also start and stop the vertical output (protocol 1.42,
/// ADR-0111 decision 4). A server older than 1.42 has none: main only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerticalWithMain {
    /// Record also starts and stops the vertical recording.
    pub record_with_main: bool,
    /// Go live also starts and stops the vertical stream.
    pub stream_with_main: bool,
}

/// The loudness meter's target and true-peak limit (protocol 1.59, ADR-0092 decision
/// 5): part of `settings.get`, `settings.set` and `settings.changed`, and of
/// `loudness.status`. Values are finite; equality compares them bit for bit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LoudnessSettings {
    /// The target, LUFS (-40 to -5): the mark on the Master's loudness bar.
    pub target: f32,
    /// The true-peak limit, dBTP (-10 to 0): the Master's dot lights once the true
    /// peak has passed it since the meter started or was reset.
    pub true_peak_limit: f32,
}

impl PartialEq for LoudnessSettings {
    fn eq(&self, other: &Self) -> bool {
        self.target.to_bits() == other.target.to_bits()
            && self.true_peak_limit.to_bits() == other.true_peak_limit.to_bits()
    }
}

impl Eq for LoudnessSettings {}

/// A recording state of a canvas other than the main one (`record.canvas_state`,
/// protocol 1.29): the `record.state` object with `canvas` added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasRecordState {
    /// The canvas.
    pub canvas: OutputCanvas,
    /// Its recording state.
    #[serde(flatten)]
    pub state: RecordState,
}

/// A stream state of a canvas other than the main one (`stream.canvas_state`,
/// protocol 1.29): the `stream.state` object with `canvas` added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasStreamState {
    /// The canvas.
    pub canvas: OutputCanvas,
    /// Its stream state.
    #[serde(flatten)]
    pub state: StreamState,
}

/// How a recording ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingSummary {
    /// Why it stopped on its own, or `null` when it was asked to.
    pub failure: Option<String>,
    /// Video frames encoded.
    pub frames: u64,
    /// Video frames dropped.
    pub dropped: u64,
}

/// How a stream reaches its server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProtocol {
    /// RTMP (`rtmp://`), with a stream key.
    Rtmp,
    /// SRT (`srt://`).
    Srt,
}

/// What the stream is doing. The server's address is not part of it: it is a setting,
/// and the stream key is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StreamState {
    /// Nothing streams.
    Idle {
        /// How the last stream of this server run ended, if one ran.
        last: Option<StreamSummary>,
    },
    /// A stream runs; it connects, and reconnects, in the background.
    Live {
        /// How it reaches the server.
        protocol: StreamProtocol,
        /// The server takes the stream now.
        connected: bool,
        /// Times it connected so far (0: it has not reached the server yet).
        connects: u64,
        /// Reconnect attempts so far.
        reconnect_attempts: u64,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is sent.
        audio_encoder: Option<String>,
        /// On hold (protocol 1.64, ADR-0124): the connection stays up and a hold
        /// picture goes out with silence. Set from the call on, including while the
        /// hold waits for a recording sharing the encoder to pause.
        #[serde(default)]
        held: bool,
        /// Time on hold so far, in milliseconds, as of this state (an open hold
        /// counted up to the change) (protocol 1.64).
        #[serde(default)]
        held_ms: u64,
        /// What the hold sends while held (protocol 1.67); `null` while not held. A
        /// scene hold that fell back says `freeze`.
        #[serde(default)]
        hold_picture: Option<crate::request::HoldPicture>,
        /// The hold scene while held with one (protocol 1.67).
        #[serde(default)]
        hold_scene: Option<u32>,
        /// The sound asked for while held with a scene (protocol 1.67).
        #[serde(default)]
        hold_sound: Option<crate::request::HoldSound>,
        /// The scene hold fell back to the last frame and silence (protocol 1.67), with
        /// why in `hold_fallback_reason`.
        #[serde(default)]
        hold_fallback: bool,
        /// The hold scene's sound alone fell back to silence while its picture runs on
        /// (protocol 1.67), with why in `hold_fallback_reason`.
        #[serde(default)]
        hold_sound_fallback: bool,
        /// Why the hold fell back, while it does (protocol 1.67).
        #[serde(default)]
        hold_fallback_reason: Option<String>,
        /// Each destination of the stream (protocol 1.77, ADR-0127 decision 16), in
        /// the order Go live started them; the top-level `connected`, `connects` and
        /// `reconnect_attempts` sum the running ones.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        destinations: Vec<LiveDestination>,
    },
}

/// One destination of a live stream (protocol 1.77, ADR-0127 decision 16). Never its
/// server or key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveDestination {
    /// Its id in the output profile (`stream.destinations`).
    pub id: u32,
    /// Its name.
    pub name: String,
    /// Its service preset's id, or `custom`.
    pub service: String,
    /// Where it stands.
    pub state: DestinationState,
    /// Times it connected so far.
    pub connects: u64,
    /// Reconnect attempts so far.
    pub reconnect_attempts: u64,
    /// Items its transport dropped while the connection was slow or down.
    pub dropped: u64,
    /// Time since it first connected, in milliseconds, as of this state; 0 before.
    pub on_air_ms: u64,
    /// Why it failed; never the server or the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where one destination of a live stream stands (protocol 1.77).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationState {
    /// Started; not connected yet.
    Connecting,
    /// Connected and sending.
    Live,
    /// The connection dropped; it tries again.
    Reconnecting,
    /// Refused at start, gave up, or failed; the others go on.
    Failed,
    /// Stopped with `stream.destination.stop` while the others go on.
    Stopped,
}

/// One saved destination (`stream.destinations`, protocol 1.77, ADR-0127 decision 16).
/// Never its server or key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDestination {
    /// Its id, unique in the output profile and never reused.
    pub id: u32,
    /// Its name.
    pub name: String,
    /// Its service preset's id, or `custom`.
    pub service: String,
    /// The canvas it belongs to.
    pub canvas: OutputCanvas,
    /// Go live sends to it.
    pub enabled: bool,
    /// A server is saved for it (a key may still be missing; Go live says so).
    pub configured: bool,
    /// The stream's encoding, or its own.
    pub encoding: DestinationEncodingKind,
}

/// Whether a destination shares the stream's encoding (protocol 1.77).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationEncodingKind {
    /// The stream's encoder, shared.
    Shared,
    /// An encoder of its own.
    Own,
}

/// `stream.destinations` (protocol 1.77).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDestinations {
    /// The asked canvas's destinations (both canvases' without `canvas`), by id.
    pub destinations: Vec<StreamDestination>,
}

/// How a stream ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSummary {
    /// Why it stopped on its own, or `null` when it was asked to.
    pub failure: Option<String>,
    /// Video frames encoded.
    pub frames: u64,
    /// Video frames dropped, and items the transport dropped while the connection was
    /// slow or down.
    pub dropped: u64,
    /// Times it connected (0: it never reached the server).
    pub connects: u64,
}

/// One audio source's levels, for `audio.levels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceLevels {
    /// The source.
    pub source: u32,
    /// Peak per channel in dBFS. Silence is −120, the floor JSON can carry.
    pub peak_db: Vec<f32>,
    /// RMS per channel in dBFS, the root mean square since the previous sample.
    pub rms_db: Vec<f32>,
}

/// One entry of a user data library (`profile.list`, `collection.list`; protocol 1.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Its stable id, passed to `profile.switch` and `collection.switch`.
    pub id: String,
    /// Its name, unique in the library ignoring case.
    pub name: String,
}

/// The output profiles or scene collections (`profile.list`, `collection.list`;
/// protocol 1.4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Every entry, by name.
    pub entries: Vec<LibraryEntry>,
    /// The id of the one in use.
    pub current: String,
    /// A switch asked for through the control API is still being made. Once it is
    /// false, a `current` other than the id asked for means the app refused the
    /// switch or could not load the entry (its error line says why).
    #[serde(default)]
    pub pending: bool,
}

/// Whether the virtual camera is on this computer (protocol 1.12, ADR-0066).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcamInstall {
    /// Apps can list the camera.
    Added,
    /// The camera is not added.
    NotAdded,
    /// The installed part is from another version of RBS; adding again updates it.
    NeedsUpdate,
    /// This computer cannot have the camera.
    Unavailable,
}

/// What the virtual camera's session did with the frames since sending started
/// (the virtual camera spec, "Ring and stats").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VcamFrames {
    /// Frames handed to the consumer side.
    pub sent: u64,
    /// Frames a newer frame replaced before the sender took them.
    pub replaced: u64,
    /// Frames the session refused.
    pub refused: u64,
    /// Frames skipped or dropped for lack of room on the consumer side.
    pub consumer_busy: u64,
    /// Frames sent while no app read the camera.
    pub consumer_absent: u64,
    /// Frames dropped while the camera's format was being changed.
    pub format_pending: u64,
    /// Operating system errors.
    pub os_errors: u64,
}

/// `replay.status` and `replay.changed` (protocol 1.27, ADR-0088).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayStatus {
    /// The buffer runs and keeps the past.
    pub armed: bool,
    /// The usable past in whole seconds; `0` while disarmed.
    pub held_secs: u32,
    /// The wanted length in seconds.
    pub length_secs: u32,
    /// Payload bytes held.
    pub bytes: u64,
    /// The memory limit in bytes.
    pub memory_limit: u64,
    /// Memory, not the length setting, is why the past is short.
    pub memory_limited: bool,
    /// A clip is being cut or written.
    pub saving: bool,
    /// The file of the last clip saved since the server started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_saved: Option<String>,
    /// Why the last save failed, until a save succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// A level of recording protection (protocol 1.49, ADR-0090 decision 9). A client
/// treats a level it does not know as `warning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionLevel {
    /// Nothing could be read (the platform has no reading, or the probe timed out).
    /// Never raises the overall level.
    Unavailable,
    /// Fine.
    Ok,
    /// Worth knowing.
    Notice,
    /// Act soon.
    Warning,
    /// Acting now (at the disk reserve the recordings there stop cleanly).
    Critical,
}

/// What a protection alert is about (protocol 1.49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionKind {
    /// A disk a recording writes to is filling.
    Disk,
    /// An output loses frames.
    Lag,
    /// The render worker is late (every canvas).
    Render,
    /// The audio worker is late.
    Audio,
    /// System memory is low.
    Memory,
}

/// One disk (volume) at least one running recording writes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionDisk {
    /// Its level.
    pub level: ProtectionLevel,
    /// Free bytes at the last reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// Seconds until the reserve is reached at the measured rate, rounded to whole
    /// minutes; absent while the rate is unknown or free space does not fall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_left: Option<u64>,
    /// The canvases whose recording writes there.
    pub recordings: Vec<OutputCanvas>,
}

/// One running output's lag level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionOutput {
    /// Its canvas.
    pub canvas: OutputCanvas,
    /// Recording or stream.
    pub output: OutputKind,
    /// `ok` or `warning` in this version.
    pub level: ProtectionLevel,
}

/// System memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionMemory {
    /// Its level.
    pub level: ProtectionLevel,
    /// Available bytes at the last reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
}

/// One thing the user should know, with a plain sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionAlert {
    /// What it is about.
    pub kind: ProtectionKind,
    /// How bad.
    pub level: ProtectionLevel,
    /// The sentence for the user.
    pub text: String,
    /// The canvas, for a lag alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<OutputCanvas>,
    /// The output, for a lag alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputKind>,
}

/// `protection.status` and `protection.changed` (protocol 1.49, ADR-0090 decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionStatus {
    /// A guard runs: some recording or stream runs. While false the lists are empty.
    pub guarded: bool,
    /// The highest level of all, `ok` when none is higher (`unavailable` never
    /// raises it).
    pub level: ProtectionLevel,
    /// Disks of the running recordings.
    pub disks: Vec<ProtectionDisk>,
    /// Running outputs.
    pub outputs: Vec<ProtectionOutput>,
    /// The render worker.
    pub render: ProtectionLevel,
    /// The audio worker.
    pub audio: ProtectionLevel,
    /// System memory.
    pub memory: ProtectionMemory,
    /// Every item at `notice` or above, and a disk that could not be read, most
    /// severe first.
    pub alerts: Vec<ProtectionAlert>,
}

impl ProtectionStatus {
    /// The state while no guard runs.
    pub fn idle() -> Self {
        Self {
            guarded: false,
            level: ProtectionLevel::Ok,
            disks: Vec::new(),
            outputs: Vec::new(),
            render: ProtectionLevel::Ok,
            audio: ProtectionLevel::Ok,
            memory: ProtectionMemory {
                level: ProtectionLevel::Ok,
                available_bytes: None,
            },
            alerts: Vec::new(),
        }
    }
}

/// `vcam.status` and `vcam.changed` (protocol 1.12, ADR-0066 decision 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcamStatus {
    /// Whether the camera is added; `null` while the app has not read it yet.
    pub state: Option<VcamInstall>,
    /// Why, in words for the user, with `needs_update` and `unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// RBS sends the program to the camera now.
    pub sending: bool,
    /// Width in pixels: sent now, or saved for the next start.
    pub width: u32,
    /// Height in pixels: sent now, or saved for the next start.
    pub height: u32,
    /// Frames per second, rounded: sent now, or saved for the next start.
    pub rate: u32,
    /// While sending, why the camera's node failed, until it runs again.
    pub failure: Option<String>,
    /// While sending, the session's counters when the adapter shares them; always
    /// `null` in `vcam.changed`.
    pub frames: Option<VcamFrames>,
}

/// The level of a plan or of one resource in `record.estimate` (protocol 1.53,
/// ADR-0087 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateLevel {
    /// Below 70 % of a rate limit, or within a count limit.
    Fits,
    /// Between 70 % and 100 % of a rate limit.
    Heavy,
    /// Past the limit.
    Over,
    /// The limit is not known (a resource row only).
    Unknown,
    /// A level this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// A resource the estimate counts (protocol 1.53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateResource {
    /// Encoder sessions on one adapter.
    EncoderSessions,
    /// Pixels per second on one hardware adapter.
    EncoderThroughput,
    /// The busiest encode worker of the separate files.
    IsoWorkers,
    /// The render partition's share taken by the separate files' exports.
    IsoRenderShare,
    /// Software video encoders' pixel rate.
    CpuLoad,
    /// Export buffers of the separate files (informational).
    Memory,
    /// The recording folder's write speed.
    DiskSpeed,
    /// The recording folder's free space.
    DiskSpace,
    /// The most separate files of one kind.
    IsoCeiling,
    /// Sources kept active for their files (informational).
    ActiveSources,
    /// A resource this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// The unit of a resource row (protocol 1.53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateUnit {
    /// Encoder sessions.
    Sessions,
    /// Pixels per second.
    PixelsPerSecond,
    /// A share of one (a worker's time, the render budget).
    Ratio,
    /// Bytes per second.
    BytesPerSecond,
    /// Bytes.
    Bytes,
    /// Separate tracks.
    Tracks,
    /// Sources.
    Sources,
    /// A unit this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// Why a resource is not `fits`, or what is missing (protocol 1.53, a closed list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateCode {
    /// A limit no calibration can measure is not known.
    Unknown,
    /// No adapter can open the track.
    NoAdapter,
    /// The track's file could not open.
    OpenFailed,
    /// Encoder sessions are heavy or over.
    Sessions,
    /// A hardware encoder's session limit is not known.
    SessionsUnknown,
    /// Encoder throughput is heavy or over.
    Throughput,
    /// A track would fall to a software encoder.
    SoftwareFallback,
    /// The software encoders' load is heavy or over.
    CpuLoad,
    /// An encode worker of the separate files is heavy or over.
    Workers,
    /// The separate files' render share is heavy or over.
    RenderShare,
    /// The recording folder's speed or free space is not known.
    DiskUnknown,
    /// The recording folder is (nearly) full.
    DiskFull,
    /// The recording folder is too slow.
    DiskSlow,
    /// More separate tracks than the ceiling.
    Ceiling,
    /// A marked source cannot be kept active.
    SourceInactive,
    /// A code this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// A separate track named by the estimate: its source and what it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimateTrack {
    /// The source's id.
    pub source: u32,
    /// What the file carries.
    pub role: TrackRole,
}

/// One resource of `record.estimate` (protocol 1.53).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateRow {
    /// The resource.
    pub resource: EstimateResource,
    /// The adapter id for `encoder_sessions` and `encoder_throughput`; `video` or
    /// `audio` for `iso_ceiling`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// What the plan needs, in `unit`. Count units and bytes are whole numbers.
    #[serde(serialize_with = "whole_if_integral")]
    pub need: f64,
    /// The limit, `None` when unknown or unlimited (`level` says which).
    #[serde(serialize_with = "whole_if_integral_opt")]
    pub limit: Option<f64>,
    /// The unit of `need` and `limit`.
    pub unit: EstimateUnit,
    /// The level; absent on informational rows (`memory`, `active_sources`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<EstimateLevel>,
    /// The limit was measured on this computer (`calibration.toml`), not declared.
    pub measured: bool,
    /// The row is unknown and *Test this computer* can measure what is missing
    /// (protocol 1.57); false on known and informational rows. Absent from older
    /// servers, read as false.
    #[serde(default)]
    pub calibrate: bool,
}

/// Writes a whole finite number as an integer (`4`, not `4.0`).
fn whole_if_integral<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        s.serialize_i64(*v as i64)
    } else {
        s.serialize_f64(*v)
    }
}

fn whole_if_integral_opt<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => whole_if_integral(v, s),
        None => s.serialize_none(),
    }
}

/// A reason of `record.estimate` (protocol 1.53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimateReason {
    /// The code.
    pub code: EstimateCode,
    /// The resource it concerns, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<EstimateResource>,
    /// The track it concerns, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<EstimateTrack>,
}

/// How many separate tracks of one kind fit (protocol 1.53, ADR-0087 decision 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaxTracks {
    /// The total that fits; `None` when no resource that grows with them is known.
    pub count: Option<u32>,
    /// The resource that stops the count.
    pub limit: Option<EstimateResource>,
    /// Some growing resource is unknown: `count` is an upper bound.
    pub partial: bool,
    /// The growing resources whose limit is unknown.
    pub unknown: Vec<EstimateResource>,
    /// Video only: the most separate video files a start with these settings opens
    /// now, however many are marked (while the encoder's capacity is unknown, at
    /// most 3, or `known_count` when a measurement reached further). Absent without
    /// a picture file template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_most_now: Option<u32>,
    /// The largest total at which the encoder capacity rows (`encoder_sessions`,
    /// `cpu_load`) were known at every step (protocol 1.55); absent when none was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_count: Option<u32>,
}

/// Both kinds of [`MaxTracks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaxTracksByKind {
    /// Separate video files.
    pub video: MaxTracks,
    /// Separate sound files.
    pub audio: MaxTracks,
}

/// What one separate track adds (protocol 1.53).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackCost {
    /// The source's id.
    pub source: u32,
    /// What the file carries.
    pub role: TrackRole,
    /// What it adds per resource, keyed by resource name. A client skips a key it
    /// does not know.
    pub cost: std::collections::BTreeMap<String, f64>,
}

/// A code of `calibration.state` (protocol 1.55, ADR-0087 decision 4): why a series
/// or the write has no full result, or why a run failed. A client reads an unknown
/// code as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationCode {
    /// The adapter does not support the class.
    Unsupported,
    /// No free session at the first open: other programs may hold them.
    Busy,
    /// A session did not open.
    OpenFailed,
    /// An encode failed.
    EncodeFailed,
    /// The folder has no room for the write test.
    NoRoom,
    /// The write test failed.
    WriteFailed,
    /// The run's time ended it first.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
    /// A device was lost: nothing was written.
    DeviceLost,
    /// The run panicked.
    Panicked,
    /// The run ended without an answer.
    Ended,
    /// The run did not answer by its deadline.
    Hung,
    /// A code this client does not know.
    #[serde(other)]
    Other,
}

/// Why a calibration series stopped, which says what its `sessions` means (protocol
/// 1.55, spec `output-estimate.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationStop {
    /// An open was refused: the session cap.
    Refused,
    /// The next step was late: a throughput limit.
    Late,
    /// The plan's ceiling passed: no cap.
    Ceiling,
    /// The time ended it: a lower bound.
    Limit,
    /// A cancel ended it: a lower bound.
    Cancelled,
    /// An error after step 1 ended it: a lower bound.
    Error,
    /// A reason this client does not know.
    #[serde(other)]
    Other,
}

/// The part of a calibration run that runs (protocol 1.55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationPart {
    /// The encoder sessions.
    Encoder,
    /// The folder write.
    Write,
    /// The export of separate video files (protocol 1.57).
    Export,
    /// A part this client does not know.
    #[serde(other)]
    Other,
}

/// A class of video encodes (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationClass {
    /// Codec name.
    pub codec: String,
    /// Size class (`720`, `1080`, `1440`, `2160`, `above-2160`).
    pub size: String,
    /// Frame-rate class (`30`, `60`, `above-60`).
    pub fps: String,
    /// Rate-control class (`cbr`, `vbr`, `cq`).
    pub rate: String,
}

/// One series' result in `calibration.state` (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationEncoder {
    /// Adapter id.
    pub adapter: String,
    /// The class.
    #[serde(flatten)]
    pub class: CalibrationClass,
    /// The highest passing session count.
    pub sessions: u32,
    /// `sessions × width × height × fps`.
    pub pixels_per_second: u64,
    /// Why the series stopped.
    pub stopped_by: CalibrationStop,
}

/// A part with no full result (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationSkip {
    /// Why.
    pub code: CalibrationCode,
    /// The series' adapter; absent for the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The series' class; absent for the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<CalibrationClass>,
}

/// One measured export class (protocol 1.57, ADR-0087 step D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationExport {
    /// Size class: `720`, `1080`, `1440`, `2160` or `above-2160`.
    pub size: String,
    /// The export format.
    pub format: CalibrationExportFormat,
    /// The render thread's time of one export per frame, in microseconds (at least a
    /// frame period when it did not keep up).
    pub time_us: u64,
    /// The GPU's sustained time per export in microseconds, for diagnosis; judged by
    /// nothing. `null` with too few conversions.
    pub gpu_us: Option<u64>,
    /// No conversion found every readback in flight and the frames arrived.
    pub kept_up: bool,
}

/// The format of an export (protocol 1.57).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationExportFormat {
    /// 8-bit 4:2:0.
    Nv12,
    /// 10-bit 4:2:0.
    P010,
    /// A format this client does not know.
    #[serde(other)]
    Other,
}

/// An export class with no result (protocol 1.57).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationExportSkip {
    /// Size class.
    pub size: String,
    /// The export format.
    pub format: CalibrationExportFormat,
    /// Why.
    pub code: CalibrationExportCode,
}

/// Why an export class has no result (protocol 1.57): its own closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationExportCode {
    /// No throwaway GPU device could be made.
    GpuUnavailable,
    /// The export could not be built or failed.
    ExportFailed,
    /// The throwaway device was lost; the run went on.
    DeviceLost,
    /// The time budget ended it first, or the GPU did not answer in time.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
    /// A code this client does not know.
    #[serde(other)]
    Other,
}

/// `calibration.status` and the topic `calibration.state` (protocol 1.55, ADR-0087
/// decisions 4 and 7). No member carries a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CalibrationState {
    /// No run in this app run.
    #[default]
    Idle,
    /// A run is active.
    Running {
        /// Steps started so far.
        step: u32,
        /// The upper bound of steps.
        steps: u32,
        /// What runs now.
        part: CalibrationPart,
        /// The adapter measured now; `null` during the write.
        adapter: Option<String>,
        /// A cancel was asked for and the run is ending.
        cancelling: bool,
    },
    /// The last run ended; it stays until the next run.
    Done {
        /// Some part stopped early or was skipped for time.
        partial: bool,
        /// A cancel ended it.
        cancelled: bool,
        /// `calibration.toml` was written.
        saved: bool,
        /// The series' results.
        encoders: Vec<CalibrationEncoder>,
        /// The folder's synced write speed in bytes per second.
        write_speed: Option<u64>,
        /// The write stopped early.
        write_partial: bool,
        /// The parts with no full result.
        skipped: Vec<CalibrationSkip>,
        /// The export classes measured (protocol 1.57).
        #[serde(default)]
        exports: Vec<CalibrationExport>,
        /// The export classes with no result (protocol 1.57).
        #[serde(default)]
        exports_skipped: Vec<CalibrationExportSkip>,
    },
    /// The last run failed; nothing was written. It stays until the next run.
    Failed {
        /// Why.
        code: CalibrationCode,
        /// A short message with no path.
        message: String,
    },
    /// A state this client does not know.
    #[serde(other)]
    Other,
}

/// Why the start gate refused a `record.start` (protocol 1.54, ADR-0087 decisions 5
/// and 6): `error.data.reason` of a `failed` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// More video files than their encoder can take (hardware sessions or software
    /// load), under `on_hardware_limit: "ask"`.
    HardwareLimit,
    /// A limit that no setting lifts: another resource past its limit under `ask`, no
    /// encoder for the files, or a measured resource still over.
    OverLimit,
    /// The plan is heavy and the request did not set `acknowledge_heavy`.
    Heavy,
    /// A file could not open after the recording started, under `ask`; what opened
    /// was stopped and removed.
    OpenFailed,
    /// A reason this version does not know.
    #[serde(other)]
    Other,
}

/// The typed view of a refused `record.start` (protocol 1.54): the members of its
/// `error.data` beside `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRefusal {
    /// Why.
    pub reason: RefusalReason,
    /// The `record.estimate` answer of the plan that was judged.
    pub estimate: EstimateAnswer,
    /// The files concerned; empty when the whole plan was judged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<EstimateTrack>,
}

impl StartRefusal {
    /// The refusal an error carries, or `None` when it is not a refused start.
    pub fn from_error(e: &crate::error::ApiError) -> Option<Self> {
        let data = e.data.as_ref()?;
        data.get("reason")?;
        serde_json::from_value(serde_json::Value::Object(data.clone())).ok()
    }

    /// The error that carries this refusal: `failed` with `data`.
    pub fn into_error(self, message: impl Into<String>) -> crate::error::ApiError {
        let data = match serde_json::to_value(&self) {
            Ok(serde_json::Value::Object(map)) => map,
            // A refusal holds only strings, finite numbers and lists, which always
            // encode to an object; anything else leaves the error without data.
            _ => serde_json::Map::new(),
        };
        crate::error::ApiError::with_data(crate::error::ErrorKind::Failed, message, data)
    }
}

/// `record.estimate`'s answer (protocol 1.53, ADR-0087 decision 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateAnswer {
    /// The plan's level (`fits`, `heavy` or `over`), free disk space included.
    pub level: EstimateLevel,
    /// The level a start's gate judges: the same without the disk space row.
    pub gate_level: EstimateLevel,
    /// The resources whose limit is unknown.
    pub unknown: Vec<EstimateResource>,
    /// One row per resource (and adapter).
    pub resources: Vec<EstimateRow>,
    /// Coded reasons, for clients to localise.
    pub reasons: Vec<EstimateReason>,
    /// How many separate tracks fit.
    pub max_tracks: MaxTracksByKind,
    /// What each separate video and sound file of the plan adds.
    pub tracks: Vec<TrackCost>,
}

/// The estimate in `record.start`'s answer (protocol 1.53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimateSummary {
    /// The plan's level.
    pub level: EstimateLevel,
    /// The resources whose limit is unknown.
    #[serde(default)]
    pub unknown: Vec<EstimateResource>,
    /// Coded reasons.
    #[serde(default)]
    pub reasons: Vec<EstimateReason>,
}

/// Why a sync offset measurement ended without a result (protocol 1.66, ADR-0126
/// decision 5). A client reads an unknown reason as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMeasureFailure {
    /// Fewer than three claps were heard near a movement in the picture. The source's
    /// gate, noise suppression or ducker can remove claps.
    TooFewClaps,
    /// No clap was seen in the picture.
    NoPictureMotion,
    /// The claps disagree by more than one frame.
    Inconsistent,
    /// The program mix stopped hearing the source.
    NotHeard,
    /// The source or the reference closed.
    SourceClosed,
    /// The source's offset changed, or its input resynchronised.
    OffsetChanged,
    /// A filter of either source changed (the nodes measured were replaced).
    ChainChanged,
    /// The studio restarted (a canvas change, a collection switch).
    StudioRestarted,
    /// `sync.measure_cancel`.
    Cancelled,
    /// A reason this client does not know.
    #[serde(other)]
    Other,
}

/// Where a sync offset measurement is (`sync.measure.changed`, protocol 1.66).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncMeasureState {
    /// Running: the user claps in view of the reference.
    Running {
        /// Clap pairs found so far (a sound and a picture event).
        pairs: u32,
    },
    /// Ended with a result. Nothing was applied: a client applies it with
    /// `source.set_sync_offset`.
    Done {
        /// The offset to set, within `-200..=200` and what the input can advance.
        suggested_offset_ms: i32,
        /// The offset measured, before that clamp.
        measured_offset_ms: i32,
        /// The offset the input applied while measuring.
        effective_offset_ms: i32,
        /// The suggestion differs from the measured value.
        clamped: bool,
        /// Pairs the result rests on.
        pairs: u32,
        /// How much the pairs disagree (interquartile range), in milliseconds.
        spread_ms: f64,
    },
    /// Ended without a result; nothing changed.
    Failed {
        /// Why.
        reason: SyncMeasureFailure,
    },
    /// A state this client does not know.
    #[serde(other)]
    Other,
}

/// The topic `sync.measure.changed` (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncMeasureChanged {
    /// The id `sync.measure_start` answered.
    pub measurement: String,
    /// The measured source.
    pub source: u32,
    /// The reference picture source.
    pub reference: u32,
    /// Where it is.
    #[serde(flatten)]
    pub state: SyncMeasureState,
}

/// An audio device whose offset is remembered (protocol 1.66, ADR-0126 decision 7):
/// the source type and its `adapter` and `target` settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceKey {
    /// Source type id.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Adapter id.
    pub adapter: String,
    /// The adapter's device id.
    pub target: String,
}

/// How a remembered offset was set (protocol 1.66). A client reads an unknown value
/// as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSyncOrigin {
    /// Applied from a measurement.
    Measured,
    /// Set by hand or through `source.set_sync_offset` without a measurement.
    Manual,
    /// A value this client does not know.
    #[serde(other)]
    Other,
}

/// What a measured offset was measured against (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSyncReference {
    /// The reference's device, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<DeviceKey>,
    /// The reference's name when measured.
    pub name: String,
}

/// One entry of `sync.devices` (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSyncEntry {
    /// The device.
    pub key: DeviceKey,
    /// The remembered offset in milliseconds.
    pub offset_ms: i32,
    /// How it was set.
    pub origin: DeviceSyncOrigin,
    /// For a measured value, what it was measured against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<DeviceSyncReference>,
    /// When it was set, in seconds since the Unix epoch (UTC).
    pub set_at: u64,
}

/// `scene.duplicate`'s `sources` (protocol 1.68, ADR-0133 decision 4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateSources {
    /// The copy places the same sources (references), as before 1.68.
    #[default]
    Shared,
    /// The copy places copies of the scene's sources and their companions.
    Copied,
}

/// How pasted filters meet the target's own (protocol 1.68, ADR-0133 decision 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterPasteMode {
    /// Replace the target's filters of that kind (default).
    #[default]
    Replace,
    /// Add after them.
    Append,
}

/// A source and its new item (protocol 1.68).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedCopy {
    /// The copy's id.
    pub source: u32,
    /// Its item.
    pub item: u32,
}

/// An original and its copy (protocol 1.68).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopiedSource {
    /// The original's id.
    pub from: u32,
    /// The copy's id.
    pub to: u32,
}

/// `source.duplicate`'s answer (protocol 1.68, ADR-0133 decision 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDuplicated {
    /// The copy's id.
    pub source: u32,
    /// Its item.
    pub item: u32,
    /// False while the copy is in standby (a camera copy starts there).
    pub active: bool,
    /// Copies of the original's companions, each muted.
    pub companions: Vec<PlacedCopy>,
}

/// `scene.duplicate`'s answer (protocol 1.43; `sources` from 1.68).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneDuplicated {
    /// The new scene's id.
    pub scene: u32,
    /// With `sources: "copied"`, each original and its copy; absent otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<CopiedSource>,
}

/// A group not written for a target, and why (protocol 1.68).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedGroup {
    /// The group's id.
    pub group: String,
    /// `not_applicable`, `unchanged`, `locked`, `no_picture_size` or `out_of_picture`.
    pub reason: String,
}

/// What `source.reset` or `source.paste_attributes` did to one target (protocol 1.68).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeTarget {
    /// The source.
    pub source: u32,
    /// The item used; absent when the source has none in the scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u32>,
    /// Groups written in the undo step.
    pub changed: Vec<String>,
    /// Live groups written outside it (not undone by `edit.undo`).
    pub live: Vec<String>,
    /// Groups not written.
    pub skipped: Vec<SkippedGroup>,
}

/// `source.reset`'s and `source.paste_attributes`' answer (protocol 1.68).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributesApplied {
    /// One entry per target.
    pub targets: Vec<AttributeTarget>,
}

/// What the attribute clipboard holds (protocol 1.68, ADR-0133 decision 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributesClipboard {
    /// The source copied.
    pub source: u32,
    /// Its name when copied.
    pub name: String,
    /// Its type id.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Groups whose copied value is not the default.
    pub groups: Vec<String>,
}

/// `text.fonts`' answer (protocol 1.75, ADR-0129 decision 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextFonts {
    /// The installed font families, sorted, without duplicates.
    pub families: Vec<String>,
}

/// The topic `source.attributes_clipboard.changed` (protocol 1.68).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributesClipboardChanged {
    /// What it holds; `null` once cleared (a collection switch).
    pub clipboard: Option<AttributesClipboard>,
}
