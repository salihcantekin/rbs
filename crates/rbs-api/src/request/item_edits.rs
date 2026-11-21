//! Layer order and the type-specific item edits: `item.move`, `scene.move` (with
//! `to_item` / `to_scene` since protocol 1.100), `item.fit_text_box`,
//! `item.set_filled` and `item.set_shown_region` (protocol 1.100, ADR-0164 decision 6).

use serde::{Deserialize, Serialize};

use super::{LayerDirection, is_main};
use crate::types::OutputCanvas;

/// `item.move` (protocol 1.32; `front` / `back` since 1.36; `to_item` since 1.100):
/// layer order among sources with a picture. Exactly one of `direction` and
/// `to_item` is given, else `invalid_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveItem {
    /// The item's id.
    pub item: u32,
    /// Over, under, to the front or to the back among sources with a picture.
    /// Sound-only sources are skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<LayerDirection>,
    /// The picture layer in the same scene whose place the item takes; the ones
    /// between shift by one toward its old place (protocol 1.100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_item: Option<u32>,
}

/// `scene.move` (protocol 1.33; `to_scene` since 1.100): the canvas's scene list.
/// Exactly one of `direction` and `to_scene` is given, else `invalid_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveScene {
    /// The scene's id.
    pub scene: u32,
    /// Toward the top of the list, or toward the bottom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<LayerDirection>,
    /// The scene whose place it takes; the ones between shift by one toward its old
    /// place (protocol 1.100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scene: Option<u32>,
}

/// `item.fit_text_box` (protocol 1.100): a text source's box fitted to its text.
/// Every item that places the source keeps its centre and its scale. One undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitTextBox {
    /// An item of the text source.
    pub item: u32,
    /// The canvas the item is named on: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.set_filled` (protocol 1.100): whether a colour source draws its fill. The
/// fill is the source's setting, shared by every scene that places it. Turning it off
/// adds a border to this item's style on `canvas` when it has none. One undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemFilled {
    /// An item of the colour source.
    pub item: u32,
    /// Draw the fill.
    pub on: bool,
    /// The canvas whose style gets the border: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// A part of a video source's picture, shown as large as fits the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShownRegion {
    /// The whole picture, fitted like a new source.
    Full,
    /// The centre, as large as the canvas (or the picture, if smaller).
    CanvasCentred,
    /// The left half.
    LeftHalf,
    /// The right half.
    RightHalf,
}

/// `item.set_shown_region` (protocol 1.100, ADR-0058 Show buttons): the item's crop
/// and box show `region`. One undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetItemShownRegion {
    /// The item's id.
    pub item: u32,
    /// Which part.
    pub region: ShownRegion,
    /// The canvas whose layout changes: `"main"` (the default) or `"vertical"`.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}
