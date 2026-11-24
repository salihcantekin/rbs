//! Canvas layout requests and results.

use serde::{Deserialize, Serialize};

use super::VerticalLayout;
use crate::types::OutputCanvas;

/// `canvas.auto_layout` (protocol 1.44, ADR-0093 decision 4): arranges a scene's
/// visible source items for the vertical canvas, one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CanvasAutoLayout {
    /// The canvas to arrange for; only `"vertical"` in 1.44.
    pub canvas: OutputCanvas,
    /// The arrangement.
    pub mode: VerticalLayout,
    /// The scene; the one the vertical canvas shows now when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
}

/// `canvas.auto_layout`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AutoLayoutDone {
    /// The number of layouts written; 0 is not an error.
    pub items: u32,
}
