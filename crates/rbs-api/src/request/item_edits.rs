//! Layer order and the type-specific item edits: `item.move`, `scene.move` (with
//! `to_item` / `to_scene` since protocol 1.100), `item.fit_text_box`,
//! `item.set_filled`, `item.set_shown_region` and `collection.set_stinger_file`
//! (protocol 1.100, ADR-0164 decision 6).

use serde::{Deserialize, Serialize};

use super::{is_main, is_zero_u64};
use crate::types::OutputCanvas;

/// Which way `item.move` or `scene.move` steps in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LayerDirection {
    /// Over the next source with a picture (or one place up in the scene list).
    Up,
    /// Under the next source with a picture (or one place down in the scene list).
    Down,
    /// To the top of the picture layers (or the start of the scene list). Protocol 1.36.
    Front,
    /// To the bottom of the picture layers (or the end of the scene list). Protocol 1.36.
    Back,
}

/// `item.move` (protocol 1.32; `front` / `back` since 1.36; `to_item` since 1.100):
/// layer order among sources with a picture. Exactly one of `direction` and
/// `to_item` is given, else `invalid_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// `collection.set_stinger_file` (protocol 1.100, ADR-0140): a new media source for
/// `path` that no scene places, made the stinger with the transition set to Stinger.
/// One undo step; answers `source.add`'s `{source}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetStingerFile {
    /// A local absolute path. A relative path is `invalid_params`; any other path
    /// needs the irreversible-actions permission.
    pub path: String,
}

/// A shape for `source.add` kind `color` (protocol 1.100): its box and radius are
/// those of the window's Add source › Shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Stamp {
    /// A circle.
    Circle,
    /// A square.
    Square,
    /// A rectangle, 1.6 times as wide as high.
    Rectangle,
    /// A rectangle with rounded corners.
    RoundedRectangle,
    /// An ellipse.
    Ellipse,
    /// A triangle.
    Triangle,
    /// An arrow pointing right.
    Arrow,
}

/// `canvas.rehearse` (protocol 1.100, ADR-0135 decision 11): the transition a
/// `canvas.transition` would run now, played on the Preview picture only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Rehearse {
    /// `"main"` (the default); the vertical canvas has no Preview picture.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// `item.set_visible` (protocol 1.6; `gesture` since 1.105).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetItemVisible {
    /// The item's id (`source.list`).
    pub item: u32,
    /// Shown.
    pub visible: bool,
    /// Calls with the same non-zero gesture and edit kind that follow each other are
    /// one undo step (a group's eye, protocol 1.105, ADR-0164 decision 2); 0, the
    /// default, is a step of its own. A gesture never refuses a call.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.set_locked` (protocol 1.6; `gesture` since 1.105).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetItemLocked {
    /// The item's id (`source.list`).
    pub item: u32,
    /// Locked.
    pub locked: bool,
    /// As on `item.set_visible`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `item.remove` (protocol 1.10, ADR-0065): Remove from scene; `gesture` since 1.105.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ItemRef {
    /// The item's id.
    pub item: u32,
    /// As on `item.set_visible`: removals with the same non-zero gesture and edit kind
    /// that follow each other are one undo step (Delete on a group's row).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}
