//! Control requests for scrolling items (ADR-0150).

use serde::{Deserialize, Serialize};

use super::{is_main, is_zero_u64};
use crate::types::{ItemScroll, OutputCanvas};

/// `item.set_scroll` (protocol 1.94, ADR-0150 decision 11): sets or clears the saved
/// scroll of a source item's layout on a canvas. One undo step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetItemScroll {
    /// The item's id (`source.list`).
    pub item: u32,
    /// The scroll; `null` clears it.
    pub scroll: Option<ItemScroll>,
    /// `"main"` (the default) or `"vertical"`, as for the other item edits.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.scroll_pause`, `item.scroll_resume` and `item.scroll_restart` (protocol 1.94,
/// ADR-0150 decision 11): live, not saved, not undone. They act on the item's run on
/// the canvas, so every item sharing that run moves too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ScrollItem {
    /// The item's id.
    pub item: u32,
    /// `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.scroll_speed` (protocol 1.94, ADR-0150 decision 11): live speed of the item's
/// run until a scroll edit, a restart or the run's end.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetScrollSpeed {
    /// The item's id.
    pub item: u32,
    /// Units per second, 1 to 4000 (0 is refused: pause is the call for it).
    pub speed: f32,
    /// `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}
