//! The item transform requests: `item.flip`, `item.rotate`, `item.fit`, `item.align`
//! and `item.reset_transform` (protocol 1.18, ADR-0084), and `item.set_rotation`
//! (protocol 1.100, ADR-0166).

use serde::{Deserialize, Serialize};

use super::{is_main, is_zero_u64};
use crate::types::OutputCanvas;

/// A direction on the canvas, as the user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

/// `item.flip` (protocol 1.18, ADR-0084): mirror the item on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FlipItem {
    /// The item's id.
    pub item: u32,
    /// The mirror axis.
    pub axis: Axis,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.rotate` (protocol 1.18, ADR-0084): turn the item clockwise about its centre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RotateItem {
    /// The item's id.
    pub item: u32,
    /// Degrees clockwise: a non-zero multiple of 90 from -360 to 360.
    pub degrees: i32,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// How `item.fit` places the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FitKind {
    /// The largest box that fits the canvas, aspect kept.
    Fit,
    /// The smallest box that covers the canvas, aspect kept.
    Fill,
    /// The canvas size, aspect not kept.
    Stretch,
}

/// `item.fit` (protocol 1.18, ADR-0084).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FitItem {
    /// The item's id.
    pub item: u32,
    /// How to place it.
    pub mode: FitKind,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// Which axes `item.align` centres on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlignAxis {
    /// Both.
    Both,
    /// Left to right only.
    Horizontal,
    /// Top to bottom only.
    Vertical,
}

/// `item.align` (protocol 1.18, ADR-0084): centre the item on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AlignItem {
    /// The item's id.
    pub item: u32,
    /// Centre on this axis only; both when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<AlignAxis>,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.reset_transform` (protocol 1.18, ADR-0084).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResetItemTransform {
    /// The item's id.
    pub item: u32,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`, the
    /// vertical canvas's own layout (protocol 1.44, ADR-0093 decision 4).
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (one drag, protocol 1.100, ADR-0164 decision 2); 0, the default,
    /// is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.set_rotation` (protocol 1.100, ADR-0166): the item's clockwise rotation about
/// its box's centre. The box, flips and crop stay. One undo step, or part of
/// `gesture`'s.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetItemRotation {
    /// The item's id.
    pub item: u32,
    /// Degrees clockwise, -360 to 360 inclusive, stored wrapped into `0..360`; 0
    /// resets the rotation. Anything else is `invalid_params`.
    pub degrees: f64,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (ADR-0164 decision 2); 0, the default, is a step of its own.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}
