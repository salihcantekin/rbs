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
}

/// How a source is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// One source of the program scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Its id in the scene collection.
    pub id: u32,
    /// Its name, unique in the collection.
    pub name: String,
    /// Its kind; `null` for a type this server cannot add.
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
    /// `org.rbs.filter.<name>`.
    #[serde(rename = "type")]
    pub type_id: String,
    /// English display name.
    pub name: String,
    /// Its settings, in display order.
    pub fields: Vec<FilterField>,
}

/// One setting of a filter type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterField {
    /// Settings key.
    pub key: String,
    /// English label.
    pub label: String,
    /// `"number"` or `"color"`.
    pub kind: String,
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
    /// The default: a number, or a `"#rrggbb"` / `"#rrggbbaa"` string.
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

/// A source of the collection, wherever it is placed (protocol 1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Its id.
    pub id: u32,
    /// Its name.
    pub name: String,
    /// Its kind; `null` for a type this server cannot add.
    pub kind: Option<SourceKind>,
    /// A global source of the studio's canvas (protocol 1.10, ADR-0065): `item.add`
    /// refuses it. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub global: bool,
}

/// `scene.list` and `scene.changed` (protocol 1.6): the scenes the studio's canvas may
/// show in the Scenes panel's order, the one on air, and every source of the
/// collection by name (what `item.add` can place).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneList {
    /// The scenes.
    pub scenes: Vec<SceneInfo>,
    /// The id of the scene on air.
    pub program: u32,
    /// The preview scene in studio mode (protocol 1.9, ADR-0061); absent outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<u32>,
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
}

/// How a stream is encoded (`settings.get`, `settings.set`; protocol 1.2). The same
/// fields as a [`RecordingProfile`]'s encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoding {
    /// The video encoder's adapter id, or `null` for the best one.
    pub encoder: Option<String>,
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
    },
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
    /// Peak per channel in dBFS.
    pub peak_db: Vec<f32>,
    /// RMS per channel in dBFS.
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
