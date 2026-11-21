//! `settings.set`'s patch, with the output fields of protocol 1.100 (ADR-0164
//! decision 5).

use serde::{Deserialize, Serialize};

use crate::types::{Canvas, RecordingProfile, StreamSettingsPatch};

/// `settings.set`: the sections given replace the current ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsPatch {
    /// New recording profiles.
    #[serde(default)]
    pub profiles: Option<Vec<RecordingProfile>>,
    /// A new canvas; the studio restarts with it. Refused while recording.
    #[serde(default)]
    pub canvas: Option<Canvas>,
    /// New stream settings (protocol 1.2). Refused while a stream runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSettingsPatch>,
    /// The loudness meter's target and true-peak limit (protocol 1.59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<crate::types::LoudnessSettings>,
    /// Switch Preview and Program after a transition (protocol 1.100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_after_transition: Option<bool>,
    /// Separate files (protocol 1.100). Refused while a recording runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate_files: Option<crate::types::SeparateFilesSettings>,
    /// The replay buffer (protocol 1.100); applies at the next arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<Box<crate::types::ReplaySettings>>,
    /// The vertical recording's own settings (protocol 1.100): one profile, with the
    /// `profiles` rules; `null` removes them, and the main profile applies. Refused
    /// while a vertical recording uses them.
    #[serde(
        default,
        deserialize_with = "crate::types::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub vertical_profile: Option<Option<Box<RecordingProfile>>>,
    /// The vertical stream's settings (protocol 1.100), the `stream` patch shape: no
    /// server and no key are written or erased. `null` is `invalid_params`. Refused
    /// while the vertical stream runs.
    #[serde(
        default,
        deserialize_with = "crate::types::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub vertical_stream: Option<Option<Box<StreamSettingsPatch>>>,
}
