//! The value types of a scene collection.

use crate::activity::{OnReturn, SourceActivity};
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
    /// Identifies a filter on a source or a bus. Never reused.
    FilterId,
    "filter"
);
id_type!(
    /// Identifies a group of items inside a scene (ADR-0182). Never reused inside the
    /// collection.
    GroupId,
    "group"
);
id_type!(
    /// Identifies a bus of a canvas (ADR-0097 decision 9). Never reused inside the
    /// collection; [`BusId::MAIN`] is the canvas's master.
    BusId,
    "bus"
);

mod bus;
pub use bus::{Bus, MAIN_BUS_NAME, MAX_BUSES, Sends};

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
    /// Pixel space stored layouts are written in (ADR-0110). Equal to [`Self::size`]
    /// until the canvas size changes; a size change does not rewrite it or the layouts.
    pub layout_size: Size,
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
    /// Buses other than `main` (ADR-0097 decision 9, ADR-0170), at most
    /// [`MAX_BUSES`]` - 1`, in Mixer order. `main` is [`Self::master`]. Live controls:
    /// edits of the canvas itself keep them, as they keep the master.
    pub buses: Vec<Bus>,
    /// Each source's send levels on this canvas; a source without an entry has the
    /// default sends (`main` at 1.0, every other bus at 0).
    pub sends: BTreeMap<SourceId, Sends>,
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
            layout_size: size,
            frame_rate,
            program: None,
            preview: None,
            globals: None,
            master: MasterAudio::default(),
            follow: None,
            fallback: Fallback::default(),
            audio_from: AudioFrom::default(),
            order: Vec::new(),
            buses: Vec::new(),
            sends: BTreeMap::new(),
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

/// Display capture (`org.rbs.source.display`). Only this type may store chrome (ADR-0104).
pub const DISPLAY_SOURCE_TYPE: &str = "org.rbs.source.display";

/// Edge insets of a source region, in full-picture pixels (ADR-0103).
///
/// `left` and `right` are pixels cut from those edges of the delivered picture, and
/// the same for `top` and `bottom`. Absent on the source means the whole picture.
/// The stored insets are not rewritten when the picture shrinks; the sampled rect is
/// their intersection with the picture and with the item crop.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceRegion {
    /// Pixels cut from the left.
    pub left: u32,
    /// Pixels cut from the top.
    pub top: u32,
    /// Pixels cut from the right.
    pub right: u32,
    /// Pixels cut from the bottom.
    pub bottom: u32,
}

impl SourceRegion {
    /// Insets in full-picture pixels.
    pub const fn new(left: u32, top: u32, right: u32, bottom: u32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// `[left, top, right, bottom]`, the control-API order.
    pub const fn to_array(self) -> [u32; 4] {
        [self.left, self.top, self.right, self.bottom]
    }

    /// The region named by [`Self::to_array`].
    pub const fn from_array(edges: [u32; 4]) -> Self {
        let [left, top, right, bottom] = edges;
        Self::new(left, top, right, bottom)
    }

    /// Width and height of this region inside `picture`, after clamping each inset
    /// so it stays inside the picture (`w = picture_w − left − right`). `None` when
    /// that intersection leaves no pixel.
    pub fn size(self, picture: (u32, u32)) -> Option<(u32, u32)> {
        let (pw, ph) = picture;
        if pw == 0 || ph == 0 {
            return None;
        }
        let left = self.left.min(pw);
        let right = self.right.min(pw - left);
        let top = self.top.min(ph);
        let bottom = self.bottom.min(ph - top);
        let width = pw - left - right;
        let height = ph - top - bottom;
        (width >= 1 && height >= 1).then_some((width, height))
    }

    /// Intersection with an item crop. Both are full-picture edge insets; the wider
    /// inset on each edge wins (ADR-0103 decision 2).
    pub fn intersect_crop(self, item: Crop) -> Crop {
        Crop::new(
            self.left.max(item.left),
            self.top.max(item.top),
            self.right.max(item.right),
            self.bottom.max(item.bottom),
        )
    }

    /// The item crop measured inside this region: insets beyond the region's own.
    /// Scale-keeping (fit, recrop) uses these with [`Self::size`].
    pub fn crop_inside(self, item: Crop) -> Crop {
        Crop::new(
            item.left.saturating_sub(self.left),
            item.top.saturating_sub(self.top),
            item.right.saturating_sub(self.right),
            item.bottom.saturating_sub(self.bottom),
        )
    }
}

/// How a display's work area is placed in the item box (ADR-0104).
///
/// This chooses a layout (`Fit` letterboxes, `Cover` fills). It is not a second
/// scale pass in the compositor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChromeFill {
    /// The smallest centred box that covers the canvas (layout fill).
    #[default]
    Cover,
    /// The largest centred box that fits the canvas (layout fit).
    Fit,
}

impl ChromeFill {
    /// `"cover"` or `"fit"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Fit => "fit",
        }
    }

    /// The fill named by [`Self::as_str`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "cover" => Some(Self::Cover),
            "fit" => Some(Self::Fit),
            _ => None,
        }
    }
}

/// Taskbar or Dock insets of a display source (ADR-0104).
///
/// Only [`DISPLAY_SOURCE_TYPE`] may store this. The four insets are capture pixels.
/// Applying chrome writes [`DisplayChrome::region`] onto the source; `fill` stays
/// out of that rectangle.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayChrome {
    /// Pixels cut from the top.
    pub top: u32,
    /// Pixels cut from the right.
    pub right: u32,
    /// Pixels cut from the bottom.
    pub bottom: u32,
    /// Pixels cut from the left.
    pub left: u32,
    /// How Fit places the region on a canvas. Not part of the stored region.
    pub fill: ChromeFill,
}

impl DisplayChrome {
    /// Insets with cover fill.
    pub const fn new(top: u32, right: u32, bottom: u32, left: u32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
            fill: ChromeFill::Cover,
        }
    }

    /// The same insets with `fill`.
    pub const fn with_fill(mut self, fill: ChromeFill) -> Self {
        self.fill = fill;
        self
    }

    /// The source region those insets describe. Fill is not included.
    pub const fn region(self) -> SourceRegion {
        SourceRegion::new(self.left, self.top, self.right, self.bottom)
    }

    /// Insets of a work rectangle `(x, y, width, height)` inside `picture`, in the
    /// same pixels. `None` when `work` is absent (Reset is a no-op) or the rectangle
    /// does not sit inside the picture.
    pub fn from_work(
        picture: (u32, u32),
        work: Option<(u32, u32, u32, u32)>,
        fill: ChromeFill,
    ) -> Option<Self> {
        let (x, y, width, height) = work?;
        let (pw, ph) = picture;
        let right_edge = x.checked_add(width)?;
        let bottom_edge = y.checked_add(height)?;
        if width == 0 || height == 0 || right_edge > pw || bottom_edge > ph {
            return None;
        }
        Some(Self {
            top: y,
            right: pw - right_edge,
            bottom: ph - bottom_edge,
            left: x,
            fill,
        })
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
    /// Optional slide override on a program-scene change (ADR-0100). `None` uses the
    /// collection glide. Fade-ins never use this.
    pub glide: Option<SourceGlide>,
    /// What the source was pointed at, as the user saw it (`target_app`, `target_title`):
    /// hints for finding it again, not settings (ADR-0077). The binder never compares
    /// them, so changing them never makes a new source node.
    pub hints: BTreeMap<String, String>,
    /// Capture region in full-picture pixels (ADR-0103). `None` is the whole picture.
    /// Not settings: the binder does not compare it, so editing it does not replace
    /// the capture node.
    pub region: Option<SourceRegion>,
    /// Display chrome (ADR-0104). `None` is off. Only a display source may set it.
    /// Not settings, same as [`Self::region`].
    pub chrome: Option<DisplayChrome>,
    /// Separate recording of this source (ADR-0086). Both false is the default and is
    /// left out of the file. The binder does not read it: a set flag adds no node
    /// until an ISO recording starts.
    pub record_separately: RecordSeparately,
    /// When the source runs and what it does when it returns (ADR-0152). Every key
    /// absent is the type's default, left out of the file. Not settings: the binder
    /// does not compare it, so changing it never replaces the source's node.
    pub activity: SourceActivity,
}

/// Which of a source's feeds are recorded as their own files (ADR-0086).
///
/// The two switches are independent. A picture source uses `video`, an audio source
/// uses `audio`, and a source with both (a media file) may set either. Both false
/// means the source is only in the master recording.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordSeparately {
    /// The source's picture, as its own file.
    pub video: bool,
    /// The source's sound, as its own file.
    pub audio: bool,
    /// The form of the picture's file (ADR-0117 decision 1). Kept only while `video`
    /// is set: clearing every feed drops it.
    pub video_as: VideoAs,
}

/// The form of a source's separate picture file (ADR-0117 decision 1).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoAs {
    /// Images for a still source, a video file otherwise, decided at each start.
    #[default]
    Auto,
    /// Always a video file.
    Video,
    /// Images; only a still source can be recorded this way.
    Image,
}

impl VideoAs {
    /// The document and control-API name: `"auto"`, `"video"` or `"image"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Video => "video",
            Self::Image => "image",
        }
    }

    /// Parse a document or API string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "video" => Some(Self::Video),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

impl RecordSeparately {
    /// Both switches, the picture's form automatic. Either may be set on its own.
    pub const fn new(video: bool, audio: bool) -> Self {
        Self {
            video,
            audio,
            video_as: VideoAs::Auto,
        }
    }

    /// The same switches with the picture's form `video_as`.
    #[must_use]
    pub const fn with_video_as(mut self, video_as: VideoAs) -> Self {
        self.video_as = video_as;
        self
    }

    /// True when neither feed is recorded separately.
    pub const fn is_none(self) -> bool {
        !self.video && !self.audio
    }
}

/// Most milliseconds a glide may last (ADR-0100).
pub const MAX_GLIDE_MS: u32 = 5_000;

/// How a slide maps unit time `0..=1` (ADR-0100). Fade-ins stay linear in the compositor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlideEasing {
    /// Straight lerp.
    Linear,
    /// Smoothstep ease in and out (today's slide).
    #[default]
    EaseInOut,
}

impl GlideEasing {
    /// The document and control-API name: `"linear"` or `"ease-in-out"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::EaseInOut => "ease-in-out",
        }
    }

    /// Parse a document or API string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "linear" => Some(Self::Linear),
            "ease-in-out" => Some(Self::EaseInOut),
            _ => None,
        }
    }
}

/// Collection-wide morph on a program-scene change (ADR-0100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionGlide {
    /// Milliseconds in `0..=MAX_GLIDE_MS`. `0` jumps.
    pub duration_ms: u32,
    /// Slide easing; fade-ins use the duration only (linear in the compositor).
    pub easing: GlideEasing,
}

impl Default for CollectionGlide {
    fn default() -> Self {
        Self {
            duration_ms: 400,
            easing: GlideEasing::EaseInOut,
        }
    }
}

impl CollectionGlide {
    /// True when every field is in range.
    pub fn is_valid(self) -> bool {
        self.duration_ms <= MAX_GLIDE_MS
    }

    /// True for the shipped default (400 ms, ease-in-out).
    pub fn is_default(self) -> bool {
        self == Self::default()
    }
}

/// Per-source slide override (ADR-0100). Same bounds as [`CollectionGlide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceGlide {
    /// Milliseconds in `0..=MAX_GLIDE_MS`.
    pub duration_ms: u32,
    /// Slide easing for this source.
    pub easing: GlideEasing,
}

impl SourceGlide {
    /// True when every field is in range.
    pub fn is_valid(self) -> bool {
        self.duration_ms <= MAX_GLIDE_MS
    }
}

/// What a program-scene change looks like (ADR-0128 decision 2). Duration 0 is a cut for
/// every kind.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransitionKind {
    /// The new layers on the next frame; no glide and no fade-in.
    Cut,
    /// The incoming scene, drawn whole, fades in over the outgoing one.
    Fade,
    /// Shared sources slide, new ones fade in, leaving ones fade out (the collection
    /// glide, ADR-0100). A new collection starts with it (owner, 2026-11-16).
    #[default]
    Move,
    /// The incoming scene slides in over the outgoing one.
    Swipe,
    /// The incoming scene slides in and pushes the outgoing one out.
    Slide,
    /// A media file plays over the change, and the scenes switch at its cut point
    /// (ADR-0140). Its duration is the file's play range.
    Stinger,
}

impl TransitionKind {
    /// Every kind, in the order the UI lists them.
    pub const ALL: [Self; 6] = [
        Self::Cut,
        Self::Fade,
        Self::Move,
        Self::Swipe,
        Self::Slide,
        Self::Stinger,
    ];

    /// The document and control-API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cut => "cut",
            Self::Fade => "fade",
            Self::Move => "move",
            Self::Swipe => "swipe",
            Self::Slide => "slide",
            Self::Stinger => "stinger",
        }
    }

    /// Parses a document or API name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// The edge a Swipe or Slide moves towards (ADR-0128 decision 1): the incoming scene
/// comes in from the opposite edge.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SwipeDirection {
    /// Towards the left edge (in from the right).
    #[default]
    Left,
    /// Towards the right edge.
    Right,
    /// Towards the top edge.
    Up,
    /// Towards the bottom edge.
    Down,
}

impl SwipeDirection {
    /// Every direction.
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// The document and control-API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    /// Parses a document or API name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }
}

/// Fade's settings (ADR-0128 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadeSettings {
    /// Milliseconds in `0..=MAX_GLIDE_MS`.
    pub duration_ms: u32,
    /// How unit time is mapped.
    pub easing: GlideEasing,
    /// An opaque sRGB colour to fade through; `None` is a crossfade.
    pub via: Option<[u8; 3]>,
}

impl Default for FadeSettings {
    fn default() -> Self {
        Self {
            duration_ms: 300,
            easing: GlideEasing::Linear,
            via: None,
        }
    }
}

/// Swipe's and Slide's settings (ADR-0128 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwipeSettings {
    /// Milliseconds in `0..=MAX_GLIDE_MS`.
    pub duration_ms: u32,
    /// How unit time is mapped.
    pub easing: GlideEasing,
    /// The edge the picture moves towards.
    pub direction: SwipeDirection,
}

impl Default for SwipeSettings {
    fn default() -> Self {
        Self {
            duration_ms: 500,
            easing: GlideEasing::EaseInOut,
            direction: SwipeDirection::Left,
        }
    }
}

/// Stinger's settings (ADR-0140 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StingerSettings {
    /// The media-file source played over the change. It is placed in no scene. `None`
    /// runs a Stinger change as a Cut.
    pub source: Option<SourceId>,
    /// The cut point, in milliseconds from the start of the source's play range;
    /// `None` uses the point found from the file (ADR-0140 decision 3).
    pub cut_ms: Option<u32>,
}

/// The collection's transition (ADR-0128 decision 1): the selected kind and each kind's
/// own settings, so changing kind loses no tuned value. Move's settings are the
/// collection glide (ADR-0100), not a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectionTransition {
    /// The kind a program change uses in studio mode (the Transition rail's choice).
    pub kind: TransitionKind,
    /// The kind a program change uses outside studio mode (ADR-0153; default Move).
    /// Each kind's settings below are shared by both.
    pub direct: TransitionKind,
    /// Fade's settings.
    pub fade: FadeSettings,
    /// Swipe's settings.
    pub swipe: SwipeSettings,
    /// Slide's settings.
    pub slide: SwipeSettings,
    /// Stinger's settings (ADR-0140).
    pub stinger: StingerSettings,
}

/// A scene's override of the transition into it (ADR-0135 decision 8): the kind, the
/// duration, or both. An absent part is the collection's. The kind's other settings
/// (easing, colour, direction) are always the collection's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SceneTransition {
    /// The kind; `None` is the collection's kind.
    pub kind: Option<TransitionKind>,
    /// Milliseconds in `0..=MAX_GLIDE_MS`; `None` is the kind's duration in the
    /// collection. With Move it replaces the glide's duration and every per-source
    /// override for that change.
    pub duration_ms: Option<u32>,
}

impl SceneTransition {
    /// True when the duration is in range.
    pub fn is_valid(self) -> bool {
        self.duration_ms.is_none_or(|d| d <= MAX_GLIDE_MS)
    }

    /// True when it overrides nothing (stored as no override).
    pub fn is_empty(self) -> bool {
        self.kind.is_none() && self.duration_ms.is_none()
    }
}

impl CollectionTransition {
    /// True when every duration is in range.
    pub fn is_valid(self) -> bool {
        self.fade.duration_ms <= MAX_GLIDE_MS
            && self.swipe.duration_ms <= MAX_GLIDE_MS
            && self.slide.duration_ms <= MAX_GLIDE_MS
    }

    /// True for a new collection's transition.
    pub fn is_default(self) -> bool {
        self == Self::default()
    }

    /// The selected kind's duration, with `glide` for Move and 0 for Cut. Stinger's
    /// duration is its file's play range, which the model does not know: 0 here.
    pub fn duration_ms(self, glide: CollectionGlide) -> u32 {
        match self.kind {
            TransitionKind::Cut => 0,
            TransitionKind::Fade => self.fade.duration_ms,
            TransitionKind::Move => glide.duration_ms,
            TransitionKind::Swipe => self.swipe.duration_ms,
            TransitionKind::Slide => self.slide.duration_ms,
            TransitionKind::Stinger => 0,
        }
    }

    /// What a document of format 18 or older means (ADR-0128 decision 3): Move, or Cut
    /// when its glide was 0 ms, in studio mode; Move outside it (ADR-0153).
    pub fn from_legacy(glide: CollectionGlide) -> Self {
        let kind = if glide.duration_ms == 0 {
            TransitionKind::Cut
        } else {
            TransitionKind::Move
        };
        // Outside studio mode it is Move, like every older document (ADR-0153).
        Self {
            kind,
            ..Self::default()
        }
    }

    /// The kind a program change uses: `kind` in studio mode, `direct` outside it
    /// (ADR-0153).
    pub fn kind_for(self, studio_mode: bool) -> TransitionKind {
        if studio_mode { self.kind } else { self.direct }
    }
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
            glide: None,
            hints: BTreeMap::new(),
            region: None,
            chrome: None,
            record_separately: RecordSeparately::default(),
            activity: SourceActivity::default(),
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
    /// The scene's own levels (ADR-0091): `None` is Shared levels, the sources' own
    /// volume and mute apply; `Some` is Own levels.
    pub audio: Option<SceneAudio>,
    /// The transition used when this scene comes on air (ADR-0135 decision 8); `None`
    /// uses the collection's.
    pub transition_in: Option<SceneTransition>,
    /// The scene's groups (ADR-0182), in creation order. Their members are items of
    /// this scene whose `group` names the group.
    pub groups: Vec<Group>,
}

/// A group of two or more contiguous items of one scene (ADR-0182). The members are
/// the scene's items whose [`SceneItem::group`] is this group's id; the group has no
/// transform of its own.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// Its id.
    pub id: GroupId,
    /// Display name, non-blank and unique in the scene.
    pub name: String,
    /// Saved group Scroll settings for each canvas.
    pub scrolls: BTreeMap<CanvasId, GroupScroll>,
}

impl Group {
    /// A group, for documents.
    pub fn new(id: GroupId, name: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            scrolls: BTreeMap::new(),
        }
    }
}

impl Scene {
    /// An empty scene in Shared levels, for documents.
    pub fn new(id: SceneId, name: &str, scope: SceneScope) -> Self {
        Self {
            id,
            name: name.to_owned(),
            scope,
            items: Vec::new(),
            audio: None,
            transition_in: None,
            groups: Vec::new(),
        }
    }

    /// The same scene with these levels (ADR-0091; `None`: Shared levels).
    pub fn with_audio(self, audio: Option<SceneAudio>) -> Self {
        Self { audio, ..self }
    }

    /// True while the scene is in Own levels (ADR-0091).
    pub fn own_levels(&self) -> bool {
        self.audio.is_some()
    }
}

/// A scene's own volume and mute for one source (ADR-0091). An absent field uses the
/// source's own value.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SceneLevel {
    /// Linear amplitude factor in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub volume: Option<f32>,
    /// Muted or not.
    pub muted: Option<bool>,
}

impl SceneLevel {
    /// A level with these fields.
    pub const fn new(volume: Option<f32>, muted: Option<bool>) -> Self {
        Self { volume, muted }
    }

    /// True if the volume, when set, is finite and in range.
    pub fn is_valid(&self) -> bool {
        self.volume
            .is_none_or(|v| v.is_finite() && (0.0..=MAX_SOURCE_VOLUME).contains(&v))
    }

    /// True if neither field is set: the entry changes nothing.
    pub const fn is_empty(&self) -> bool {
        self.volume.is_none() && self.muted.is_none()
    }

    /// `audio` with this level's fields in place of its volume and mute.
    pub fn over(&self, audio: SourceAudio) -> SourceAudio {
        SourceAudio {
            volume: self.volume.unwrap_or(audio.volume),
            muted: self.muted.unwrap_or(audio.muted),
            ..audio
        }
    }
}

/// A scene's own levels (Own levels, ADR-0091): an entry per source the scene sets.
/// A source without an entry keeps its own volume and mute.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneAudio {
    /// The entries, by source.
    pub levels: BTreeMap<SourceId, SceneLevel>,
}

impl SceneAudio {
    /// Own levels with these entries.
    pub fn new(levels: BTreeMap<SourceId, SceneLevel>) -> Self {
        Self { levels }
    }

    /// The scene's entry for `source`, if it has one.
    pub fn level(&self, source: SourceId) -> Option<&SceneLevel> {
        self.levels.get(&source)
    }

    /// `audio` (the source's own) with the scene's entry for `source` applied.
    pub fn heard(&self, source: SourceId, audio: SourceAudio) -> SourceAudio {
        match self.levels.get(&source) {
            Some(level) => level.over(audio),
            None => audio,
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
    /// An isosceles triangle with its tip on the top edge of the box and its base
    /// along the bottom.
    Triangle,
    /// An arrow pointing toward the right edge of the box.
    Arrow,
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

    /// Lengths multiplied by `factor` (ADR-0110). A factor of 1, or one that is not
    /// finite and positive, leaves the style unchanged.
    pub fn scaled(self, factor: f32) -> Self {
        if !(factor.is_finite() && factor > 0.0) || factor == 1.0 {
            return self;
        }
        let mut out = self;
        if let Shape::Rectangle { radius } = &mut out.shape {
            *radius *= factor;
        }
        if let Some(b) = &mut out.border {
            b.width *= factor;
        }
        if let Some(sh) = &mut out.shadow {
            sh.offset = sh.offset.map(|v| v * factor);
            sh.blur *= factor;
        }
        out
    }

    /// True if every length is finite, radius and blur are not negative, and a border is
    /// wider than 0.
    pub fn is_valid(&self) -> bool {
        let shape = match self.shape {
            Shape::Rectangle { radius } => radius.is_finite() && radius >= 0.0,
            Shape::Circle | Shape::Ellipse | Shape::Triangle | Shape::Arrow => true,
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

/// Which way a scrolling picture moves in the item's box (ADR-0150), in the box's own
/// frame: a rotated box scrolls along its rotated axis.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

/// How often the picture crosses the window (ADR-0150).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollRepeat {
    /// Copies follow each other forever, `gap` apart.
    #[default]
    Loop,
    /// The picture crosses the window once, then the box stays empty.
    Once,
}

/// Where a scroll run starts (ADR-0150).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollStart {
    /// The picture starts where it is placed without a scroll.
    #[default]
    Inside,
    /// The picture enters from the edge the motion comes from.
    Outside,
}

/// Slowest scroll speed, layout units per second (ADR-0150).
pub const MIN_SCROLL_SPEED: f32 = 1.0;
/// Fastest scroll speed, layout units per second.
pub const MAX_SCROLL_SPEED: f32 = 4000.0;
/// Widest gap between two copies, layout units.
pub const MAX_SCROLL_GAP: f32 = 16384.0;
/// Longest edge fade, layout units.
pub const MAX_SCROLL_FADE: f32 = 2048.0;
/// Longest ease of a pause, resume or speed change, milliseconds.
pub const MAX_SCROLL_RAMP_MS: u32 = 5000;
/// The ease a new scroll gets, milliseconds.
pub const DEFAULT_SCROLL_RAMP_MS: u32 = 300;

/// A picture that moves through its item's box (ADR-0150). The box is a window onto the
/// picture and its copies; across the direction the picture fills the box with its
/// aspect ratio kept. Lengths and speed are in the units of [`Layout::size`] and scale
/// with the box.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scroll {
    /// Which way the picture moves.
    pub direction: ScrollDirection,
    /// Layout units per second, `MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED`.
    pub speed: f32,
    /// Empty layout units between two copies, `0..=MAX_SCROLL_GAP`.
    pub gap: f32,
    /// Forever or once.
    pub repeat: ScrollRepeat,
    /// Where the run starts.
    pub start: ScrollStart,
    /// Soft edges at both ends of the window, layout units, `0..=MAX_SCROLL_FADE`. The
    /// compositor clamps it to half the window.
    pub fade: f32,
    /// How long a pause, resume or speed change eases (`EaseInOut`), milliseconds,
    /// `0..=MAX_SCROLL_RAMP_MS`.
    pub ramp_ms: u32,
    /// What the run does when a shown scene draws the item again after none did
    /// (ADR-0152 decision 6): `Restart` (the default) starts it over; `Continue` keeps
    /// the run while the item's source stays placed, so it picks up where it would be.
    /// `Resume` is not offered.
    pub on_return: OnReturn,
}

/// Saved Scroll of a composed group. Group runs end when no snapshot uses them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupScroll {
    /// Direction of motion.
    pub direction: ScrollDirection,
    /// Layout units per second.
    pub speed: f32,
    /// Gap between copies in layout units.
    pub gap: f32,
    /// Repetition mode.
    pub repeat: ScrollRepeat,
    /// Starting position.
    pub start: ScrollStart,
    /// Edge fade in layout units.
    pub fade: f32,
    /// Pause and speed transition duration in milliseconds.
    pub ramp_ms: u32,
}

impl GroupScroll {
    /// A looping group Scroll with the ordinary Scroll defaults.
    pub fn new(direction: ScrollDirection, speed: f32) -> Self {
        Self::from_scroll(Scroll::new(direction, speed))
    }

    /// Converts the shared settings of an item Scroll.
    pub fn from_scroll(scroll: Scroll) -> Self {
        Self {
            direction: scroll.direction,
            speed: scroll.speed,
            gap: scroll.gap,
            repeat: scroll.repeat,
            start: scroll.start,
            fade: scroll.fade,
            ramp_ms: scroll.ramp_ms,
        }
    }

    /// The shared settings as an item Scroll with the default return policy.
    pub fn as_scroll(self) -> Scroll {
        Scroll::new(self.direction, self.speed)
            .with_gap(self.gap)
            .with_repeat(self.repeat)
            .with_start(self.start)
            .with_fade(self.fade)
            .with_ramp_ms(self.ramp_ms)
    }

    /// Whether all settings are within the saved Scroll ranges.
    pub fn is_valid(&self) -> bool {
        self.as_scroll().is_valid()
    }
}

impl Scroll {
    /// A looping scroll in `direction` at `speed`, starting inside, no gap, no fade, the
    /// default ramp.
    pub const fn new(direction: ScrollDirection, speed: f32) -> Self {
        Self {
            direction,
            speed,
            gap: 0.0,
            repeat: ScrollRepeat::Loop,
            start: ScrollStart::Inside,
            fade: 0.0,
            ramp_ms: DEFAULT_SCROLL_RAMP_MS,
            on_return: OnReturn::Restart,
        }
    }

    /// The same scroll with `gap`.
    pub const fn with_gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    /// The same scroll with `repeat`.
    pub const fn with_repeat(mut self, repeat: ScrollRepeat) -> Self {
        self.repeat = repeat;
        self
    }

    /// The same scroll with `start`.
    pub const fn with_start(mut self, start: ScrollStart) -> Self {
        self.start = start;
        self
    }

    /// The same scroll with `fade`.
    pub const fn with_fade(mut self, fade: f32) -> Self {
        self.fade = fade;
        self
    }

    /// The same scroll with `ramp_ms`.
    pub const fn with_ramp_ms(mut self, ramp_ms: u32) -> Self {
        self.ramp_ms = ramp_ms;
        self
    }

    /// The same scroll with `on_return`.
    pub const fn with_on_return(mut self, on_return: OnReturn) -> Self {
        self.on_return = on_return;
        self
    }

    /// True if every value is within its range (NaN is not).
    pub fn is_valid(&self) -> bool {
        (MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(&self.speed)
            && (0.0..=MAX_SCROLL_GAP).contains(&self.gap)
            && (0.0..=MAX_SCROLL_FADE).contains(&self.fade)
            && self.ramp_ms <= MAX_SCROLL_RAMP_MS
            && matches!(self.on_return, OnReturn::Restart | OnReturn::Continue)
    }

    /// Speed, gap and fade multiplied by `factor`, as the box is scaled (ADR-0150
    /// decision 2). A factor of 1, or one that is not finite and positive, leaves the
    /// scroll unchanged. The result may leave the saved ranges; it is drawn, not stored.
    pub fn scaled(self, factor: f32) -> Self {
        if !(factor.is_finite() && factor > 0.0) || factor == 1.0 {
            return self;
        }
        let mut out = self;
        out.speed *= factor;
        out.gap *= factor;
        out.fade *= factor;
        out
    }

    /// Speed, gap and fade brought into their saved ranges (NaN becomes the lower end).
    pub fn clamped(self) -> Self {
        let fit = |v: f32, lo: f32, hi: f32| if v.is_nan() { lo } else { v.clamp(lo, hi) };
        let mut out = self;
        out.speed = fit(self.speed, MIN_SCROLL_SPEED, MAX_SCROLL_SPEED);
        out.gap = fit(self.gap, 0.0, MAX_SCROLL_GAP);
        out.fade = fit(self.fade, 0.0, MAX_SCROLL_FADE);
        out.ramp_ms = self.ramp_ms.min(MAX_SCROLL_RAMP_MS);
        out
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
    /// The picture moving through the box (ADR-0150). Only for source items.
    pub scroll: Option<Scroll>,
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
            scroll: None,
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

    /// The same layout with `scroll`.
    pub const fn with_scroll(mut self, scroll: Option<Scroll>) -> Self {
        self.scroll = scroll;
        self
    }

    /// True if every number is finite, opacity is within 0..=1 and the style and the
    /// scroll are valid.
    pub fn is_valid(&self) -> bool {
        self.position.iter().all(|v| v.is_finite())
            && self.size.iter().all(|v| v.is_finite())
            && self.rotation.is_finite()
            && (0.0..=1.0).contains(&self.opacity)
            && self.style.is_valid()
            && self.scroll.is_none_or(|s| s.is_valid())
    }

    /// This box, written in a canvas of `from`, as it is drawn on a canvas of `to`
    /// (ADR-0110). The whole `from` rectangle is fitted, centred, into `to` with one
    /// scale (the smaller side ratio) and the box moves with it. Equal sizes return
    /// `self` unchanged. Sides of 0 leave it unchanged.
    pub fn placed_in(&self, from: (u32, u32), to: (u32, u32)) -> Self {
        let Some((scale, ox, oy)) = layout_fit(from, to) else {
            return *self;
        };
        let mut out = *self;
        out.position = [self.position[0] * scale + ox, self.position[1] * scale + oy];
        out.size = [self.size[0] * scale, self.size[1] * scale];
        out.style = self.style.scaled(scale);
        out.scroll = self.scroll.map(|s| s.scaled(scale));
        out
    }

    /// The box to store so [`Self::placed_in`] draws `self` (ADR-0110). `self` is in
    /// `to` pixels; the result is in `from` pixels. Inverse of [`Self::placed_in`].
    pub fn stored_in(&self, from: (u32, u32), to: (u32, u32)) -> Self {
        let Some((scale, ox, oy)) = layout_fit(from, to) else {
            return *self;
        };
        let inv = 1.0 / scale;
        let mut out = *self;
        out.position = [(self.position[0] - ox) * inv, (self.position[1] - oy) * inv];
        out.size = [self.size[0] * inv, self.size[1] * inv];
        out.style = self.style.scaled(inv);
        // Stored values must stay within the saved ranges.
        out.scroll = self.scroll.map(|s| s.scaled(inv).clamped());
        out
    }
}

/// Scale and top-left of `from` fitted, centred, into `to`. `None` when the sizes are
/// equal or a side is 0: the caller keeps the layout as it is.
fn layout_fit(from: (u32, u32), to: (u32, u32)) -> Option<(f32, f32, f32)> {
    if from == to || from.0 == 0 || from.1 == 0 || to.0 == 0 || to.1 == 0 {
        return None;
    }
    let (fw, fh) = (from.0 as f32, from.1 as f32);
    let (tw, th) = (to.0 as f32, to.1 as f32);
    let scale = (tw / fw).min(th / fh);
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    Some((scale, (tw - fw * scale) / 2.0, (th - fh * scale) / 2.0))
}

/// Longest timed show, in milliseconds (ADR-0105). Absent means no duration.
pub const MAX_SHOW_FOR_MS: u32 = 600_000;

/// True when `show_for_ms` is absent or in `1..=MAX_SHOW_FOR_MS`.
pub fn show_for_is_valid(show_for_ms: Option<u32>) -> bool {
    match show_for_ms {
        None => true,
        Some(ms) => (1..=MAX_SHOW_FOR_MS).contains(&ms),
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
    /// The lasting eye. Hidden items are not drawn, except while a timed-show cue
    /// names this item (ADR-0105). The cue does not write this flag.
    pub visible: bool,
    /// How long `item.show` draws this item while it is hidden, in milliseconds
    /// (ADR-0105). `None` means Show does nothing. When set, `1..=MAX_SHOW_FOR_MS`.
    pub show_for_ms: Option<u32>,
    /// Locked items cannot be selected, moved, resized or cropped with the pointer on
    /// the picture panel (ADR-0059). Properties, the control API and undo still change
    /// them; flattening and binding ignore the flag.
    pub locked: bool,
    /// Layout used on every canvas without an override.
    pub layout: Layout,
    /// Per-canvas layouts for shared scenes shown on several canvases (U15), ordered
    /// by canvas id.
    pub overrides: BTreeMap<CanvasId, Layout>,
    /// The group this item belongs to, if any (ADR-0182).
    pub group: Option<GroupId>,
}

impl SceneItem {
    /// A visible item with `layout` on every canvas, for documents.
    pub fn new(id: ItemId, content: ItemContent, layout: Layout) -> Self {
        Self {
            id,
            content,
            visible: true,
            show_for_ms: None,
            locked: false,
            layout,
            overrides: BTreeMap::new(),
            group: None,
        }
    }

    /// The layout on `canvas`: its override, or the default layout.
    pub fn layout_on(&self, canvas: CanvasId) -> &Layout {
        self.overrides.get(&canvas).unwrap_or(&self.layout)
    }
}
