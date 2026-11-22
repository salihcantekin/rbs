//! `settings.get`'s [`Settings`], with the output settings it carries since protocol
//! 1.100 (ADR-0164 decision 5): Separate files and the replay buffer. The vertical
//! recording and stream sections reuse [`RecordingProfile`], [`StreamSettings`] and
//! [`StreamSettingsPatch`].

use serde::{Deserialize, Serialize};

use super::{
    Canvas, Dimensions, Encoding, LoudnessSettings, RecordingProfile, StreamProtocol,
    VerticalWithMain, present,
};
use crate::request::HardwareLimitPolicy;

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
    pub stream: Option<Box<StreamSettings>>,
    /// Whether Record and Go live also act on the vertical outputs (protocol 1.42;
    /// read only here, set with `output.set_with_vertical`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<VerticalWithMain>,
    /// The loudness meter's target and true-peak limit (protocol 1.59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<LoudnessSettings>,
    /// Switch Preview and Program after a transition (protocol 1.100); absent from
    /// older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_after_transition: Option<bool>,
    /// Separate files (protocol 1.100); absent from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate_files: Option<SeparateFilesSettings>,
    /// The replay buffer (protocol 1.100); absent from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplaySettings>,
    /// The vertical recording's own settings (protocol 1.100): `null` when the
    /// vertical recording uses the main profile; absent from older servers.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub vertical_profile: Option<Option<Box<RecordingProfile>>>,
    /// The vertical stream's settings (protocol 1.100): `null` when it is not set up;
    /// absent from older servers.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub vertical_stream: Option<Option<Box<StreamSettings>>>,
}

/// `settings.separate_files` (protocol 1.100): Settings › Recording › Separate files.
/// Refused with `conflict` while a recording runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeparateFilesSettings {
    /// Record and `record.start` without `iso` write the marked sources' files.
    pub enabled: bool,
    /// What a `record.start` with the 1.54 parameters does with video files past
    /// their encoder's capacity when it does not say. `cpu` is `invalid_params` in a
    /// patch until the CPU encoder for separate files exists (ADR-0119 step (c)).
    pub on_hardware_limit: HardwareLimitPolicy,
}

/// `settings.replay` (protocol 1.100): the replay buffer's settings. A change applies
/// at the next arm; a running buffer keeps its settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySettings {
    /// Arm the buffer when RBS starts.
    pub arm_on_start: bool,
    /// Wanted length of the past in seconds (`invalid_params` outside the bounds the
    /// spec gives).
    pub length_secs: u32,
    /// Most memory the buffer may use, in MiB.
    pub memory_mib: u32,
    /// Keep the sound too.
    pub audio: bool,
    /// Where clips go, absolute; absent or `null`: the recording profile's folder. A
    /// relative path is `invalid_params`; one that is not local needs the
    /// irreversible-actions permission.
    #[serde(default)]
    pub folder: Option<String>,
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
    /// The bus the stream sends, by name (protocol 1.103, ADR-0170 decision 3); every
    /// destination sends it. Omitted when `main`, and `main` from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bus: Option<String>,
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
    /// The bus the stream sends (protocol 1.103); absent: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bus: Option<String>,
}
