//! Scrolling layers on the wire (protocol 1.94, ADR-0150; `on_return` 1.99, ADR-0152).

use serde::{Deserialize, Serialize};

use super::OutputCanvas;
use super::serde_defaults::*;

/// Which way a scrolling picture moves in its box (protocol 1.94, ADR-0150), in the
/// box's own frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    /// Right to left, like a news ticker.
    #[default]
    Left,
    /// Left to right.
    Right,
    /// Bottom to top, like credits.
    Up,
    /// Top to bottom.
    Down,
}

/// How often a scrolling picture crosses its box (ADR-0150).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ScrollRepeat {
    /// Copies follow each other forever.
    #[default]
    Loop,
    /// Once, then the box stays empty.
    Once,
}

/// Where a scroll run starts (ADR-0150).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ScrollStart {
    /// Where the picture is placed without a scroll.
    #[default]
    Inside,
    /// Entering from the edge the motion comes from.
    Outside,
}

/// What a scroll's run does when a shown scene draws its item again (protocol 1.99,
/// ADR-0152 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ScrollReturn {
    /// Starts over.
    #[default]
    Restart,
    /// Where it would be had it kept moving, while the item's source stays placed.
    Continue,
}

/// A picture moving through its item's box (protocol 1.94, ADR-0150). Lengths and
/// speed in the item's box units (canvas pixels for an item on the canvas); defaults
/// are left out.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ItemScroll {
    /// Which way it moves (`left`).
    #[serde(default, skip_serializing_if = "is_default")]
    pub direction: ScrollDirection,
    /// Units per second, 1 to 4000.
    pub speed: f32,
    /// Empty units between two copies, 0 to 16384 (0).
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub gap: f32,
    /// `loop` (the default) or `once`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub repeat: ScrollRepeat,
    /// `inside` (the default) or `outside`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub start: ScrollStart,
    /// Soft edges at both ends of the box, 0 to 2048 (0).
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub fade: f32,
    /// How long a pause, resume or speed change eases, 0 to 5000 ms (300).
    #[serde(
        default = "default_ramp_ms",
        skip_serializing_if = "is_default_ramp_ms"
    )]
    pub ramp_ms: u32,
    /// `restart` (the default) or `continue` (protocol 1.99).
    #[serde(default, skip_serializing_if = "is_default")]
    pub on_return: ScrollReturn,
}

/// `item.scroll_finished` (protocol 1.94, ADR-0150 decision 11): a `Once` scroll's
/// picture has left its box. Published once per run, with every item that used it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollFinished {
    /// The canvas.
    pub canvas: OutputCanvas,
    /// The items, in id order.
    pub items: Vec<u32>,
}
