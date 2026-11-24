//! A capture target as `source.targets` lists it (protocol 1.101 fields: ADR-0168).

use serde::{Deserialize, Serialize};

use super::Dimensions;

/// Something a source can capture: a display, window, camera or audio endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// The application that owns a window, when the OS tells it (protocol 1.101,
    /// ADR-0168 decision 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Its rank among the user's recent picks for this kind, 0 the newest
    /// (`settings.recent_targets`; protocol 1.101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<u32>,
    /// A camera's own microphone, when it is not a source yet (protocol 1.101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic: Option<String>,
    /// The sound that goes with a display or window can be added too (protocol
    /// 1.101).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sound: bool,
    /// Not a target yet: adding it runs the OS picker on the computer running RBS,
    /// so a client skips it (`target` is empty; protocol 1.101).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub picker: bool,
}
