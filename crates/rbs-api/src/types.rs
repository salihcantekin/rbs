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

/// `loudness.status` (protocol 1.51, ADR-0092 stage 1): the loudness of the main
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

/// Why a marked source has no file (protocol 1.46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The program mix does not hear the source when the recording starts.
    NotHeard,
    /// A reason this client does not know (a later minor).
    #[serde(other)]
    Other,
}

/// One separate file of a running recording (protocol 1.46, ADR-0086).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordTrack {
    /// The file's name inside the recording's folder. No directory.
    pub file: String,
    /// What it carries. Only `audio` in 1.46.
    pub role: TrackRole,
    /// The source it records (`source.list`).
    pub source: u32,
}

/// What a separate file carries (protocol 1.46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRole {
    /// The source's picture (not written in 1.46).
    Video,
    /// The source's sound.
    Audio,
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
    /// Stop: it was not running (idle, or still ending).
    NotRunning,
    /// Start of a vertical stream with no server saved: skipped, not an error.
    NotConfigured,
    /// Start: refused; `error` says why.
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
}

impl From<&crate::error::ApiError> for OutputError {
    fn from(e: &crate::error::ApiError) -> Self {
        Self {
            code: e.kind.code(),
            message: e.message.clone(),
            data: OutputErrorData { kind: e.kind },
        }
    }
}

/// One canvas's entry in an `all` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    /// The canvas.
    pub canvas: OutputCanvas,
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
    },
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
