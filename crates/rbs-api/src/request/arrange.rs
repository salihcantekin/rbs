//! The multi-item arrange requests (protocol 1.106, ADR-0182 "Align and distribute"):
//! `item.align_items` and `item.distribute`.

use serde::{Deserialize, Serialize};

use super::{Axis, is_main, is_zero_u64};
use crate::types::OutputCanvas;

/// The edge or centre line `item.align_items` lines the items up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlignEdge {
    /// Left edges meet.
    Left,
    /// Horizontal centres meet.
    Center,
    /// Right edges meet.
    Right,
    /// Top edges meet.
    Top,
    /// Vertical centres meet.
    Middle,
    /// Bottom edges meet.
    Bottom,
}

/// What `item.align_items` lines the items up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlignTo {
    /// The area the items cover together (the default).
    #[default]
    Selection,
    /// The canvas's edges.
    Canvas,
}

impl AlignTo {
    pub(super) fn is_selection(&self) -> bool {
        *self == Self::Selection
    }
}

/// `item.align_items` (protocol 1.106): lines up two or more items of the scene the
/// canvas edits, or one item with the canvas. One undo step. A locked item is
/// `conflict`; fewer than two items without `to: "canvas"`, a repeated item or an item
/// without a picture is `invalid_params`; an unknown item is `not_found`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AlignItems {
    /// The item ids.
    pub items: Vec<u32>,
    /// The edge or centre line.
    pub edge: AlignEdge,
    /// What to line up to; absent: the selection.
    #[serde(default, skip_serializing_if = "AlignTo::is_selection")]
    pub to: AlignTo,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step; 0, the default, is a step of its own.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.distribute` (protocol 1.106): spreads three or more items so the gaps between
/// neighbours along `axis` are equal; the two outermost stay. One undo step; the errors
/// are those of `item.align_items`, and fewer than three items is `invalid_params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DistributeItems {
    /// The item ids.
    pub items: Vec<u32>,
    /// The direction the items are spread along.
    pub axis: Axis,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// As in `item.align_items`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}
