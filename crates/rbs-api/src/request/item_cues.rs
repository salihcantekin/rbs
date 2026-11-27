//! The timed-show item requests: `item.set_show_for` and `item.show` (ADR-0105).

use serde::{Deserialize, Serialize};

/// `item.set_show_for` (ADR-0105). Undoable document state. No protocol minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetItemShowFor {
    /// The item's id (`source.list`).
    pub item: u32,
    /// Milliseconds in `1..=600000`, or `null` / absent to clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_for_ms: Option<u32>,
}

/// `item.show` (ADR-0105). Live session cue. Does not change `visible` and is not
/// undone. No protocol minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ShowItem {
    /// The item's id (`source.list`).
    pub item: u32,
}
