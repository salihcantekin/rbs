//! OBS import result types.

use serde::{Deserialize, Serialize};

use super::LibraryEntry;

/// `import.obs.scan` (protocol 1.82): what OBS's settings folders on the server's
/// computer hold, by name only (never a path).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObsScan {
    /// Scene collection names, as `import.obs.check` takes them.
    pub collections: Vec<String>,
    /// Profile names.
    pub profiles: Vec<String>,
}

/// What an OBS import brings exactly (protocol 1.82).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObsCounts {
    /// Scenes.
    pub scenes: u32,
    /// Sources.
    pub sources: u32,
    /// Items placed in scenes.
    pub items: u32,
    /// Filters.
    pub filters: u32,
}

/// One OBS object or setting not brought exactly (protocol 1.82, ADR-0131
/// decision 11). Never holds a credential or a full path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObsReportEntry {
    /// `approximated`, `skipped` or `not-chosen`.
    pub outcome: String,
    /// `scene`, `source`, `filter`, `item`, `transition`, `hotkey`, `profile` or
    /// `setting`.
    pub area: String,
    /// The OBS name.
    pub obs_name: String,
    /// The OBS type id; absent for a setting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub obs_type: String,
    /// The scenes it is in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenes: Vec<String>,
    /// Why, in one sentence.
    pub reason: String,
    /// What the user can do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// `import.obs.check` (protocol 1.82): the report; nothing was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObsCheck {
    /// The OBS collection's name.
    pub name: String,
    /// What comes across exactly.
    pub exact: ObsCounts,
    /// What does not, in the order met.
    pub entries: Vec<ObsReportEntry>,
}

/// `import.obs.run` (protocol 1.82): what was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObsRun {
    /// The new scene collection (`collection.switch` takes its id).
    pub collection: LibraryEntry,
    /// The new output profile, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<LibraryEntry>,
    /// Hotkeys added as shortcuts.
    pub shortcuts_added: u32,
    /// The check's report.
    pub report: ObsCheck,
    /// What writing left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ObsReportEntry>,
}
