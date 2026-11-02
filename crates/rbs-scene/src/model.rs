//! The value types of a scene collection.

use std::collections::BTreeMap;
use std::fmt;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub(crate) u32);

        impl $name {
            /// An id from its raw value, for example from a document or a control API
            /// command. Lookups with an id that was never issued simply find nothing.
            pub const fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            /// The raw id.
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "#{}"), self.0)
            }
        }
    };
}

id_type!(
    /// Identifies a canvas within a collection. Never reused.
    CanvasId,
    "canvas"
);
id_type!(
    /// Identifies a source within a collection. Never reused.
    SourceId,
    "source"
);
id_type!(
    /// Identifies a scene within a collection. Never reused.
    SceneId,
    "scene"
);
id_type!(
    /// Identifies a scene item (one placement in one scene). Never reused.
    ItemId,
    "item"
);
id_type!(
    /// Identifies a filter on a source. Never reused.
    FilterId,
    "filter"
);

/// One setting value. Settings schemas come with the plugin SDK; until then values are
/// stored as given and validated by the node type.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    /// A flag.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A number. Must be finite.
    Float(f64),
    /// Text.
    Text(String),
}

/// Settings of a source or filter, by key. Ordered, so documents and diffs are stable.
pub type Settings = BTreeMap<String, SettingValue>;

/// An output picture size and frame rate (glossary: Canvas).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Canvas {
    /// Its id.
    pub id: CanvasId,
    /// Display name.
    pub name: String,
    /// Picture size in pixels.
    pub size: Size,
    /// Frame rate of the canvas tick.
    pub frame_rate: FrameRate,
    /// Scene on air on this canvas.
    pub program: Option<SceneId>,
    /// Scene in preview (studio mode); `None` when studio mode is off.
    pub preview: Option<SceneId>,
}

/// An effect applied to a source (glossary: Filter).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// Its id.
    pub id: FilterId,
    /// Node type of the filter.
    pub type_id: NodeTypeId,
    /// Disabled filters are skipped, not removed.
    pub enabled: bool,
    /// Settings.
    pub settings: Settings,
}

/// Something that produces video or audio, shared by every scene that places it
/// (glossary: Source).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Its id.
    pub id: SourceId,
    /// Display name, unique in the collection.
    pub name: String,
    /// Node type of the source.
    pub type_id: NodeTypeId,
    /// Settings.
    pub settings: Settings,
    /// Filters, applied in order.
    pub filters: Vec<Filter>,
}

/// Who may show a scene (UI rule U15).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SceneScope {
    /// Only this canvas shows it, directly or nested.
    Canvas(CanvasId),
    /// Every canvas may show it.
    Shared,
}

/// A layout of sources shown on a canvas (glossary: Scene).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// Its id.
    pub id: SceneId,
    /// Display name, unique in the collection.
    pub name: String,
    /// Which canvases may show it.
    pub scope: SceneScope,
    /// Items, bottom first.
    pub items: Vec<SceneItem>,
}

/// What a scene item places.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemContent {
    /// A source.
    Source(SourceId),
    /// Another scene, drawn inside the item's box (nested scene).
    Scene(SceneId),
}

/// Pixels cut from each edge of a source before it is placed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Crop {
    /// From the left edge.
    pub left: u32,
    /// From the top edge.
    pub top: u32,
    /// From the right edge.
    pub right: u32,
    /// From the bottom edge.
    pub bottom: u32,
}

impl Crop {
    /// A crop.
    pub const fn new(left: u32, top: u32, right: u32, bottom: u32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// True if nothing is cut.
    pub const fn is_none(&self) -> bool {
        self.left == 0 && self.top == 0 && self.right == 0 && self.bottom == 0
    }
}

/// Where an item sits on a canvas: a box at `position` (top-left, canvas pixels) of
/// `size`, rotated clockwise by `rotation` degrees about its centre.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Top-left corner of the unrotated box, in canvas pixels.
    pub position: [f32; 2],
    /// Box size in canvas pixels; may be negative to flip.
    pub size: [f32; 2],
    /// Clockwise rotation in degrees about the box centre.
    pub rotation: f32,
    /// Source pixels cut before placing. Only for source items.
    pub crop: Crop,
    /// 0.0 (invisible) to 1.0 (opaque).
    pub opacity: f32,
}

impl Layout {
    /// A box at `position` of `size`, no rotation, no crop, opaque.
    pub const fn new(position: [f32; 2], size: [f32; 2]) -> Self {
        Self {
            position,
            size,
            rotation: 0.0,
            crop: Crop::new(0, 0, 0, 0),
            opacity: 1.0,
        }
    }

    /// The same layout rotated by `degrees`.
    pub const fn with_rotation(mut self, degrees: f32) -> Self {
        self.rotation = degrees;
        self
    }

    /// The same layout with `crop`.
    pub const fn with_crop(mut self, crop: Crop) -> Self {
        self.crop = crop;
        self
    }

    /// The same layout with `opacity`.
    pub const fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// True if every number is finite and opacity is within 0..=1.
    pub fn is_valid(&self) -> bool {
        self.position.iter().all(|v| v.is_finite())
            && self.size.iter().all(|v| v.is_finite())
            && self.rotation.is_finite()
            && (0.0..=1.0).contains(&self.opacity)
    }
}

/// One placement of a source or scene in a scene (a "scene item"). The same source in
/// two scenes is two items sharing one source.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct SceneItem {
    /// Its id.
    pub id: ItemId,
    /// What it places.
    pub content: ItemContent,
    /// Hidden items are not drawn and do not keep their source Active.
    pub visible: bool,
    /// Layout used on every canvas without an override.
    pub layout: Layout,
    /// Per-canvas layouts for shared scenes shown on several canvases (U15), ordered
    /// by canvas id.
    pub overrides: BTreeMap<CanvasId, Layout>,
}

impl SceneItem {
    /// The layout on `canvas`: its override, or the default layout.
    pub fn layout_on(&self, canvas: CanvasId) -> &Layout {
        self.overrides.get(&canvas).unwrap_or(&self.layout)
    }
}
