//! OBS import requests.

use serde::{Deserialize, Serialize};

/// What `import.obs.check` and `import.obs.run` bring (protocol 1.82, ADR-0131
/// decision 12): the parts the desktop wizard offers. Every part is on when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
