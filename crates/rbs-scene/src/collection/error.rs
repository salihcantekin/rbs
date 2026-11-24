//! Why an edit was refused.

use std::fmt;

use rbs_graph::node::NodeTypeId;

use super::{MAX_CANVAS_SIDE, MAX_FRAME_RATE, MAX_LAYERS, MAX_NESTING_DEPTH};
use crate::activity::MAX_LINGER_S;
use crate::model::{
    BusId, CanvasId, FilterId, GroupId, ItemId, MAX_BUSES, MAX_GLIDE_MS, MAX_SHOW_FOR_MS,
    MAX_SYNC_OFFSET_MS, SceneId, SourceId,
};

/// Why an edit was refused. The collection is unchanged after any error.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneError {
    /// No canvas with this id.
    UnknownCanvas(CanvasId),
    /// No source with this id.
    UnknownSource(SourceId),
    /// No scene with this id.
    UnknownScene(SceneId),
    /// No item with this id.
    UnknownItem(ItemId),
    /// No filter with this id on the source.
    UnknownFilter(FilterId),
    /// No group with this id in the scene (ADR-0182).
    UnknownGroup(GroupId),
    /// A group has fewer than two members (ADR-0182).
    GroupTooSmall(GroupId),
    /// A group's members are not next to each other in layer order (ADR-0182).
    GroupNotContiguous(GroupId),
    /// Items of two scenes cannot be grouped (ADR-0182).
    GroupScope,
    /// Another canvas, source or scene already has this name.
    DuplicateName(String),
    /// Names must not be empty or only whitespace.
    EmptyName,
    /// The node type id is not reverse-DNS style.
    BadTypeId(NodeTypeId),
    /// A canvas must be at least 1x1.
    EmptyCanvas,
    /// A layout has a non-finite number, an opacity outside 0..=1, or an invalid style
    /// or scroll.
    InvalidLayout,
    /// A float setting is not finite.
    InvalidSetting(String),
    /// Crop applies to source items only.
    CropOnScene(ItemId),
    /// Shape and border apply to source items only (ADR-0053): a nested scene is not
    /// drawn as one picture.
    StyleOnScene(ItemId),
    /// Scroll applies to source items only (ADR-0150).
    ScrollOnScene(ItemId),
    /// The scene would contain itself, directly or through other scenes.
    NestingCycle(SceneId),
    /// The scene's scope does not allow it here: a canvas scene shown on another
    /// canvas, or nested into a scene that other canvases may show (U15).
    ScopeViolation(SceneId),
    /// An index is past the end of the list.
    IndexOutOfRange(usize),
    /// The canvas still owns scenes; remove them or make them shared first.
    CanvasHasScenes(CanvasId),
    /// A canvas's follow setting is not allowed (ADR-0093): it follows itself, an
    /// unknown canvas or another follower, or its fallback scene is not one it may show.
    InvalidFollow(CanvasId),
    /// An undo record could not be applied because the collection changed in a way
    /// that conflicts with it (undo records must be applied in reverse order).
    UndoConflict,
    /// All ids of this kind have been issued.
    IdsExhausted,
    /// Scenes nest more than [`MAX_NESTING_DEPTH`] levels deep.
    NestingTooDeep(SceneId),
    /// A scene would flatten to more than [`MAX_LAYERS`] layers.
    TooManyLayers(SceneId),
    /// Two filters share an id.
    DuplicateFilter(FilterId),
    /// A document lists two canvases, sources, scenes or items with the same id
    /// ([`SceneCollection::from_parts`]): the id, as `canvas#3`, `item#7`, ...
    DuplicateId(String),
    /// A canvas side is larger than [`MAX_CANVAS_SIDE`].
    CanvasTooLarge,
    /// A canvas frame rate is above [`MAX_FRAME_RATE`] frames per second.
    FrameRateTooHigh,
    /// A source volume is not finite or outside `0.0..=1.0` (ADR-0059).
    InvalidVolume,
    /// A source sync offset is outside `-200..=200` ms (ADR-0062).
    InvalidSyncOffset,
    /// A glide duration is above [`MAX_GLIDE_MS`] (ADR-0100).
    InvalidGlide,
    /// A source's linger is above [`MAX_LINGER_S`] (ADR-0152).
    InvalidActivity,
    /// `show_for_ms` is set and is not in `1..=MAX_SHOW_FOR_MS` (ADR-0105).
    InvalidShowFor,
    /// A transition duration is above [`MAX_GLIDE_MS`] (ADR-0128).
    InvalidTransition,
    /// Chrome was set on a source that is not a display (ADR-0104).
    ChromeNotDisplay,
    /// A global scene (ADR-0065) would be shown as program or preview, nested, nest a
    /// scene, or not be scoped to its own canvas.
    GlobalScene(SceneId),
    /// The scene is in Shared levels, so it has no entries of its own to edit
    /// (ADR-0091).
    SharedLevels(SceneId),
    /// The edit would place the stinger source in a scene, or make a placed source the
    /// stinger (ADR-0140 decision 1).
    StingerPlaced(SourceId),
    /// No bus with this id on the canvas (ADR-0097 decision 9).
    UnknownBus(BusId),
    /// The canvas already has [`MAX_BUSES`] buses, `main` included (ADR-0170).
    TooManyBuses,
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SceneError::UnknownCanvas(id) => write!(f, "unknown {id}"),
            SceneError::UnknownSource(id) => write!(f, "unknown {id}"),
            SceneError::UnknownScene(id) => write!(f, "unknown {id}"),
            SceneError::UnknownItem(id) => write!(f, "unknown {id}"),
            SceneError::UnknownFilter(id) => write!(f, "unknown {id}"),
            SceneError::DuplicateName(n) => write!(f, "name {n:?} is already used"),
            SceneError::EmptyName => f.write_str("name is empty"),
            SceneError::BadTypeId(t) => write!(f, "type id {t:?} is not reverse-DNS"),
            SceneError::EmptyCanvas => f.write_str("canvas size must be at least 1x1"),
            SceneError::InvalidLayout => f.write_str(
                "layout is not finite, opacity is out of range, or the style or scroll is invalid",
            ),
            SceneError::InvalidSetting(k) => write!(f, "setting {k:?} is not finite"),
            SceneError::CropOnScene(id) => write!(f, "{id} places a scene; crop is for sources"),
            SceneError::StyleOnScene(id) => {
                write!(f, "{id} places a scene; shape and border are for sources")
            }
            SceneError::ScrollOnScene(id) => {
                write!(f, "{id} places a scene; scroll is for sources")
            }
            SceneError::NestingCycle(id) => write!(f, "{id} would contain itself"),
            SceneError::ScopeViolation(id) => write!(f, "{id} is not allowed here by its scope"),
            SceneError::IndexOutOfRange(i) => write!(f, "index {i} out of range"),
            SceneError::CanvasHasScenes(id) => write!(f, "{id} still owns scenes"),
            SceneError::InvalidFollow(id) => write!(f, "{id} cannot follow like that"),
            SceneError::GlobalScene(id) => write!(
                f,
                "{id} is a global scene: it belongs to its canvas, is never shown or nested, and holds only sources"
            ),
            SceneError::UndoConflict => f.write_str("undo record conflicts with the collection"),
            SceneError::IdsExhausted => f.write_str("ids exhausted"),
            SceneError::NestingTooDeep(id) => {
                write!(f, "{id} nests more than {MAX_NESTING_DEPTH} levels")
            }
            SceneError::TooManyLayers(id) => {
                write!(f, "{id} would flatten to more than {MAX_LAYERS} layers")
            }
            SceneError::DuplicateFilter(id) => write!(f, "{id} is used twice"),
            SceneError::DuplicateId(id) => write!(f, "{id} is listed twice"),
            SceneError::CanvasTooLarge => {
                write!(f, "canvas sides can be at most {MAX_CANVAS_SIDE} pixels")
            }
            SceneError::FrameRateTooHigh => {
                write!(
                    f,
                    "canvas frame rates can be at most {MAX_FRAME_RATE} per second"
                )
            }
            SceneError::InvalidVolume => f.write_str("volume must be a number from 0 to 1"),
            SceneError::InvalidSyncOffset => write!(
                f,
                "sync offset must be from -{MAX_SYNC_OFFSET_MS} to {MAX_SYNC_OFFSET_MS} ms"
            ),
            SceneError::InvalidGlide => {
                write!(f, "glide duration must be from 0 to {MAX_GLIDE_MS} ms")
            }
            SceneError::InvalidActivity => {
                write!(f, "linger must be from 0 to {MAX_LINGER_S} s")
            }
            SceneError::InvalidTransition => {
                write!(f, "transition duration must be from 0 to {MAX_GLIDE_MS} ms")
            }
            SceneError::InvalidShowFor => {
                write!(f, "show duration must be from 1 to {MAX_SHOW_FOR_MS} ms")
            }
            SceneError::ChromeNotDisplay => f.write_str("only a display source can store chrome"),
            SceneError::SharedLevels(id) => write!(f, "{id} uses the sources' own levels"),
            SceneError::StingerPlaced(id) => {
                write!(f, "{id} is the stinger and cannot be placed in a scene")
            }
            SceneError::UnknownBus(id) => write!(f, "unknown {id}"),
            SceneError::UnknownGroup(id) => write!(f, "unknown {id}"),
            SceneError::GroupTooSmall(id) => write!(f, "{id} needs at least two items"),
            SceneError::GroupNotContiguous(id) => {
                write!(f, "the items of {id} must be next to each other")
            }
            SceneError::GroupScope => write!(f, "items of two scenes cannot share a group"),
            SceneError::TooManyBuses => {
                write!(
                    f,
                    "a canvas can have at most {MAX_BUSES} buses, main included"
                )
            }
        }
    }
}

impl std::error::Error for SceneError {}
