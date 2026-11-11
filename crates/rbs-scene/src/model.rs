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
    /// The canvas's global scene (ADR-0065): its items are drawn above, and heard
    /// with, every scene the canvas shows. `None` when the canvas has none.
    pub globals: Option<SceneId>,
    /// The canvas's master volume and mute, the last stage of its mix (ADR-0081). Shared
    /// by preview and program; a live control that edits of the canvas itself ignore.
    pub master: MasterAudio,
    /// The canvas whose program scene this canvas shows (ADR-0093 decision 3). `None`:
    /// its own program scene. The followed canvas follows nothing itself.
    pub follow: Option<CanvasId>,
    /// What a follower shows while the followed program scene may not be shown on it
    /// (a scene scoped to another canvas, U15). Meaningful only with `follow`.
    pub fallback: Fallback,
    /// Where the canvas's sound comes from (ADR-0093 decision 5).
    pub audio_from: AudioFrom,
    /// Scene list order for this canvas (scene document format 11). Empty means id
    /// order, which is what a document from before format 11 has. Ids that are not
    /// showable on the canvas are ignored.
    pub order: Vec<SceneId>,
}

/// What a following canvas shows when the followed canvas's program scene is scoped to
/// that canvas alone (ADR-0093 decision 3).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    /// Keep showing the last scene it showed (the default).
    #[default]
    KeepLast,
    /// A chosen scene, which must be showable on the following canvas.
    Scene(SceneId),
}

/// The source of a canvas's sound (ADR-0093 decision 5). Only the main canvas's mix
/// exists in the first version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioFrom {
    /// The studio's main canvas mix.
    #[default]
    Main,
}

/// Volume and mute of a canvas's whole mix (ADR-0081). Like a source's volume it stops
/// at 0 dB; a boost is an audio filter.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterAudio {
    /// Linear amplitude factor in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub volume: f32,
    /// A muted master silences the canvas's outputs; the volume is kept.
    pub muted: bool,
}

impl MasterAudio {
    /// `volume`, muted or not.
    pub const fn new(volume: f32, muted: bool) -> Self {
        Self { volume, muted }
    }

    /// True if the volume is finite and in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub fn is_valid(&self) -> bool {
        self.volume.is_finite() && (0.0..=MAX_SOURCE_VOLUME).contains(&self.volume)
    }

    /// True for full volume, not muted.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The linear gain the mixer applies: the volume, or 0 while muted.
    pub fn gain(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }
}

impl Default for MasterAudio {
    fn default() -> Self {
        Self::new(MAX_SOURCE_VOLUME, false)
    }
}

impl Canvas {
    /// A canvas with no program or preview scene. Edits issue ids themselves; this is
    /// for documents ([`crate::SceneCollection::from_parts`]).
    pub fn new(id: CanvasId, name: &str, size: Size, frame_rate: FrameRate) -> Self {
        Self {
            id,
            name: name.to_owned(),
            size,
            frame_rate,
            program: None,
            preview: None,
            globals: None,
            master: MasterAudio::default(),
            follow: None,
            fallback: Fallback::default(),
            audio_from: AudioFrom::default(),
            order: Vec::new(),
        }
    }
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

impl Filter {
    /// An enabled filter with `settings`, for documents.
    pub fn new(id: FilterId, type_id: NodeTypeId, settings: Settings) -> Self {
        Self {
            id,
            type_id,
            enabled: true,
            settings,
        }
    }
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
    /// Volume and mute of its sound in the canvas mixers (ADR-0059). One value for the
    /// source wherever it is placed; changing it never makes a new source node.
    pub audio: SourceAudio,
    /// What the source was pointed at, as the user saw it (`target_app`, `target_title`):
    /// hints for finding it again, not settings (ADR-0077). The binder never compares
    /// them, so changing them never makes a new source node.
    pub hints: BTreeMap<String, String>,
}

/// The hint key that marks an audio source as the companion of a picture source
/// (ADR-0096): the value is the picture's source id in decimal.
pub const COMPANION_OF_HINT: &str = "companion_of";

/// The hint that the user deactivated this source (`source.set_active`). The value is
/// `"1"`. Hiding a source does not set it: a hidden source in an active scene stays
/// open, so showing it again is immediate. Deactivating releases the device.
pub const STANDBY_HINT: &str = "standby";

impl Source {
    /// The picture source this source is the "Capture audio too" companion of, if its
    /// hints say so (ADR-0096). A hint: nothing but visibility and deletion follow it.
    pub fn companion_of(&self) -> Option<SourceId> {
        self.hints
            .get(COMPANION_OF_HINT)?
            .parse::<u32>()
            .ok()
            .map(SourceId)
    }

    /// The user deactivated this source, so it is not opened even while a scene that
    /// places it is on air or in preview.
    pub fn standby(&self) -> bool {
        self.hints.get(STANDBY_HINT).is_some_and(|v| v == "1")
    }
}

/// The largest source volume: 1.0 (0 dB), like a fader. A boost is an audio filter.
pub const MAX_SOURCE_VOLUME: f32 = 1.0;

/// The largest sync offset of a source's sound, either way, in milliseconds (ADR-0062).
/// Equal to the audio input's default limit (`rbs_sources::DEFAULT_MAX_AUDIO_OFFSET`).
pub const MAX_SYNC_OFFSET_MS: i32 = 200;

/// Volume, mute and sync offset of a source's sound (ADR-0059, ADR-0062).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceAudio {
    /// Linear amplitude factor in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub volume: f32,
    /// Muted sound is not mixed; the volume is kept.
    pub muted: bool,
    /// Milliseconds in `-MAX_SYNC_OFFSET_MS..=MAX_SYNC_OFFSET_MS`: positive delays the
    /// sound, negative advances it.
    pub sync_offset_ms: i32,
    /// Not drawn in the Mixer; still heard (ADR-0080). A view choice: it never changes
    /// the mix.
    pub mixer_hidden: bool,
    /// Whether the source is heard on the monitor output (ADR-0097 decision 5). Saved
    /// with the source; a machine without a monitor device treats `Only` as `Off`.
    pub monitor: SourceMonitor,
}

/// Whether a source is heard on the monitor output (ADR-0097 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceMonitor {
    /// Not monitored; the stream and recordings carry it.
    #[default]
    Off,
    /// Monitored only: taken off the stream and recordings (its send ramps to 0) while a
    /// monitor output is active; heard in both while none is.
    Only,
    /// Monitored and mixed.
    Also,
}

impl SourceMonitor {
    /// The lowercase name used in documents and the control API: `off`, `only`, `also`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Only => "only",
            Self::Also => "also",
        }
    }

    /// The mode named by [`SourceMonitor::as_str`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "only" => Some(Self::Only),
            "also" => Some(Self::Also),
            _ => None,
        }
    }
}

impl SourceAudio {
    /// `volume`, muted or not, with no sync offset.
    pub const fn new(volume: f32, muted: bool) -> Self {
        Self {
            volume,
            muted,
            sync_offset_ms: 0,
            mixer_hidden: false,
            monitor: SourceMonitor::Off,
        }
    }

    /// The same sound with this sync offset.
    pub const fn with_sync_offset(self, sync_offset_ms: i32) -> Self {
        Self {
            sync_offset_ms,
            ..self
        }
    }

    /// The same sound with this monitor mode (ADR-0097).
    pub const fn with_monitor(self, monitor: SourceMonitor) -> Self {
        Self { monitor, ..self }
    }

    /// The same sound, hidden from the Mixer or not (ADR-0080).
    pub const fn with_mixer_hidden(self, mixer_hidden: bool) -> Self {
        Self {
            mixer_hidden,
            ..self
        }
    }

    /// True if the volume is finite and in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub fn volume_is_valid(&self) -> bool {
        self.volume.is_finite() && (0.0..=MAX_SOURCE_VOLUME).contains(&self.volume)
    }

    /// True if the sync offset is in range.
    pub fn sync_offset_is_valid(&self) -> bool {
        (-MAX_SYNC_OFFSET_MS..=MAX_SYNC_OFFSET_MS).contains(&self.sync_offset_ms)
    }

    /// True if every value is in range.
    pub fn is_valid(&self) -> bool {
        self.volume_is_valid() && self.sync_offset_is_valid()
    }

    /// True for full volume, not muted, no sync offset, shown in the Mixer, not monitored.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for SourceAudio {
    fn default() -> Self {
        Self::new(MAX_SOURCE_VOLUME, false)
    }
}

impl Source {
    /// A source without filters, for documents.
    pub fn new(id: SourceId, name: &str, type_id: NodeTypeId, settings: Settings) -> Self {
        Self {
            id,
            name: name.to_owned(),
            type_id,
            settings,
            filters: Vec::new(),
            audio: SourceAudio::default(),
            hints: BTreeMap::new(),
        }
    }
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

impl Scene {
    /// An empty scene, for documents.
    pub fn new(id: SceneId, name: &str, scope: SceneScope) -> Self {
        Self {
            id,
            name: name.to_owned(),
            scope,
            items: Vec::new(),
        }
    }
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

/// The outline of an item's picture inside its box (ADR-0053).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// The box, with rounded corners of `radius` box units (0: square corners). A radius
    /// larger than half the shorter side is drawn as half the shorter side.
    Rectangle {
        /// Corner radius, not negative.
        radius: f32,
    },
    /// The largest circle centred in the box.
    Circle,
    /// The ellipse that fills the box.
    Ellipse,
}

impl Default for Shape {
    fn default() -> Self {
        Self::Rectangle { radius: 0.0 }
    }
}

/// A line drawn inside the edge of an item's shape (ADR-0053).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    /// Width in box units, more than 0.
    pub width: f32,
    /// sRGB colour with straight alpha: red, green, blue, alpha.
    pub color: [u8; 4],
}

impl Border {
    /// A border of `width` box units in `color` (sRGB, straight alpha).
    pub const fn new(width: f32, color: [u8; 4]) -> Self {
        Self { width, color }
    }
}

/// A shadow under an item's shape (ADR-0053). Saved and validated; drawn from a later
/// step of ADR-0053 on.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// Offset in box units, x right and y down.
    pub offset: [f32; 2],
    /// Blur radius in box units, not negative.
    pub blur: f32,
    /// sRGB colour with straight alpha.
    pub color: [u8; 4],
}

impl Shadow {
    /// A shadow.
    pub const fn new(offset: [f32; 2], blur: f32, color: [u8; 4]) -> Self {
        Self {
            offset,
            blur,
            color,
        }
    }
}

/// How an item's picture is shaped and framed on the canvas (ADR-0053). Lengths are in
/// the item's box units (those of [`Layout::size`]), so the shape scales, rotates and
/// flips with the picture. The default draws the picture as it is.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ItemStyle {
    /// The outline.
    pub shape: Shape,
    /// A line inside the outline's edge.
    pub border: Option<Border>,
    /// A shadow under the shape.
    pub shadow: Option<Shadow>,
}

impl ItemStyle {
    /// A style with `shape` and no border.
    pub const fn new(shape: Shape) -> Self {
        Self {
            shape,
            border: None,
            shadow: None,
        }
    }

    /// The same style with `border`.
    pub const fn with_border(mut self, border: Option<Border>) -> Self {
        self.border = border;
        self
    }

    /// The same style with `shadow`.
    pub const fn with_shadow(mut self, shadow: Option<Shadow>) -> Self {
        self.shadow = shadow;
        self
    }

    /// True if it is the default: no rounding, no border, no shadow.
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    /// True if every length is finite, radius and blur are not negative, and a border is
    /// wider than 0.
    pub fn is_valid(&self) -> bool {
        let shape = match self.shape {
            Shape::Rectangle { radius } => radius.is_finite() && radius >= 0.0,
            Shape::Circle | Shape::Ellipse => true,
        };
        let border = self
            .border
            .is_none_or(|b| b.width.is_finite() && b.width > 0.0);
        let shadow = self.shadow.is_none_or(|s| {
            s.offset.iter().all(|v| v.is_finite()) && s.blur.is_finite() && s.blur >= 0.0
        });
        shape && border && shadow
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
    /// Shape, border and shadow of the picture (ADR-0053). Only for source items.
    pub style: ItemStyle,
}

impl Layout {
    /// A box at `position` of `size`, no rotation, no crop, opaque, no style.
    pub const fn new(position: [f32; 2], size: [f32; 2]) -> Self {
        Self {
            position,
            size,
            rotation: 0.0,
            crop: Crop::new(0, 0, 0, 0),
            opacity: 1.0,
            style: ItemStyle::new(Shape::Rectangle { radius: 0.0 }),
        }
    }

    /// A `source_w` x `source_h` picture as large as fits in a `box_w` x `box_h` canvas
    /// with its aspect ratio kept, centred (letterbox or pillarbox). An empty source
    /// size fills the canvas.
    pub fn fit(box_w: u32, box_h: u32, source_w: u32, source_h: u32) -> Self {
        let (bw, bh) = (box_w as f32, box_h as f32);
        if source_w == 0 || source_h == 0 {
            return Self::new([0.0, 0.0], [bw, bh]);
        }
        let (sw, sh) = (source_w as f32, source_h as f32);
        // The side that limits keeps the canvas size exactly.
        let (w, h) =
            if u64::from(source_w) * u64::from(box_h) >= u64::from(source_h) * u64::from(box_w) {
                (bw, bw * sh / sw)
            } else {
                (bh * sw / sh, bh)
            };
        Self::new([(bw - w) / 2.0, (bh - h) / 2.0], [w, h])
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

    /// The same layout with `style`.
    pub const fn with_style(mut self, style: ItemStyle) -> Self {
        self.style = style;
        self
    }

    /// True if every number is finite, opacity is within 0..=1 and the style is valid.
    pub fn is_valid(&self) -> bool {
        self.position.iter().all(|v| v.is_finite())
            && self.size.iter().all(|v| v.is_finite())
            && self.rotation.is_finite()
            && (0.0..=1.0).contains(&self.opacity)
            && self.style.is_valid()
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
    /// Locked items cannot be selected, moved, resized or cropped with the pointer on
    /// the picture panel (ADR-0059). Properties, the control API and undo still change
    /// them; flattening and binding ignore the flag.
    pub locked: bool,
    /// Layout used on every canvas without an override.
    pub layout: Layout,
    /// Per-canvas layouts for shared scenes shown on several canvases (U15), ordered
    /// by canvas id.
    pub overrides: BTreeMap<CanvasId, Layout>,
}

impl SceneItem {
    /// A visible item with `layout` on every canvas, for documents.
    pub fn new(id: ItemId, content: ItemContent, layout: Layout) -> Self {
        Self {
            id,
            content,
            visible: true,
            locked: false,
            layout,
            overrides: BTreeMap::new(),
        }
    }

    /// The layout on `canvas`: its override, or the default layout.
    pub fn layout_on(&self, canvas: CanvasId) -> &Layout {
        self.overrides.get(&canvas).unwrap_or(&self.layout)
    }
}
