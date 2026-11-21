//! `settings.get`'s [`Settings`], with the output settings it carries since protocol
//! 1.100 (ADR-0164 decision 5): Separate files and the replay buffer. The vertical
//! recording and stream sections reuse [`RecordingProfile`], [`StreamSettings`] and
//! [`super::StreamSettingsPatch`].

use serde::{Deserialize, Serialize};

use super::{
    Canvas, LoudnessSettings, RecordingProfile, StreamSettings, VerticalWithMain, present,
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
    pub stream: Option<StreamSettings>,
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
