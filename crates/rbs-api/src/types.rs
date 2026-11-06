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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Its id in the scene collection.
    pub id: u32,
    /// Its name, unique in the collection.
    pub name: String,
    /// Its kind; `null` for a type this server cannot add.
    pub kind: Option<SourceKind>,
    /// How it is doing.
    pub status: SourceStatus,
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
