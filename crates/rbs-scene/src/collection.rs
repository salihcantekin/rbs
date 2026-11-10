//! The scene collection: validated edits, each returning its own undo.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;

use crate::arrange::{AutoItem, AutoLayout, auto_layout};
use crate::model::{
    AudioFrom, Canvas, CanvasId, Fallback, Filter, FilterId, ItemContent, ItemId, Layout,
    MAX_SYNC_OFFSET_MS, MasterAudio, Scene, SceneId, SceneItem, SceneScope, SettingValue, Settings,
    Source, SourceAudio, SourceId, SourceMonitor,
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
    /// Another canvas, source or scene already has this name.
    DuplicateName(String),
    /// Names must not be empty or only whitespace.
    EmptyName,
    /// The node type id is not reverse-DNS style.
    BadTypeId(NodeTypeId),
    /// A canvas must be at least 1x1.
    EmptyCanvas,
    /// A layout has a non-finite number or an opacity outside 0..=1.
    InvalidLayout,
    /// A float setting is not finite.
    InvalidSetting(String),
    /// Crop applies to source items only.
    CropOnScene(ItemId),
    /// Shape and border apply to source items only (ADR-0053): a nested scene is not
    /// drawn as one picture.
    StyleOnScene(ItemId),
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
    /// A global scene (ADR-0065) would be shown as program or preview, nested, nest a
    /// scene, or not be scoped to its own canvas.
    GlobalScene(SceneId),
}

/// Most levels of scenes in a nesting chain, counting the shown scene. Bounds the
/// recursion of [`SceneCollection::flatten`].
pub const MAX_NESTING_DEPTH: usize = 16;

/// Largest canvas side, in pixels. Documents are shared and so untrusted; no GPU path
/// or encoder handles more.
pub const MAX_CANVAS_SIDE: u32 = 16_384;

/// Highest canvas frame rate, in frames per second.
pub const MAX_FRAME_RATE: u32 = 1000;

/// Most layers one scene may flatten to (counting hidden items, so showing an item
/// never fails). Diamond-shaped nesting multiplies layers, so this is checked on every
/// edit rather than left to the compositor.
pub const MAX_LAYERS: u64 = 4096;

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
            SceneError::InvalidLayout => {
                f.write_str("layout is not finite, opacity is out of range or the style is invalid")
            }
            SceneError::InvalidSetting(k) => write!(f, "setting {k:?} is not finite"),
            SceneError::CropOnScene(id) => write!(f, "{id} places a scene; crop is for sources"),
            SceneError::StyleOnScene(id) => {
                write!(f, "{id} places a scene; shape and border are for sources")
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
        }
    }
}

impl std::error::Error for SceneError {}

/// An inverse edit. Applying it with [`SceneCollection::undo`] restores the state
/// before the edit and returns the record that redoes it. Records must be applied in
/// reverse order of the edits that produced them (a stack); out-of-order use is refused
/// with [`SceneError::UndoConflict`] or ordinary validation errors, never applied
/// half-way.
#[derive(Debug, Clone, PartialEq)]
pub struct Undo(Op);

/// One primitive, invertible change.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    InsertCanvas {
        canvas: Canvas,
        overrides: Vec<(ItemId, Layout)>,
        /// Its global scene, removed with it (ADR-0065).
        globals: Option<Scene>,
    },
    RemoveCanvas(CanvasId),
    /// Replaces only a canvas's master volume and mute (ADR-0081).
    SetCanvasMaster {
        canvas: CanvasId,
        expected: MasterAudio,
        new: MasterAudio,
    },
    /// Replaces a canvas only if it currently equals `expected`, so an undo used out
    /// of order is refused instead of reverting later changes. Everything but its
    /// `master`, which stays as it is (a live control with its own record).
    ReplaceCanvas {
        expected: Canvas,
        new: Canvas,
    },
    InsertSource {
        source: Source,
        placements: Vec<(SceneId, usize, SceneItem)>,
    },
    RemoveSource(SourceId),
    /// Several edits that undo and redo as one (a picture deleted with its companions,
    /// ADR-0096). The inverse runs them in reverse order.
    Batch(Vec<Op>),
    /// Replaces everything of a source but its `audio`, which stays as it is.
    ReplaceSource {
        expected: Source,
        new: Source,
    },
    /// Replaces only a source's volume, mute and sync offset.
    SetSourceAudio {
        source: SourceId,
        expected: SourceAudio,
        new: SourceAudio,
    },
    InsertScene {
        scene: Scene,
        placements: Vec<(SceneId, usize, SceneItem)>,
        shown: Vec<(CanvasId, bool, bool)>,
        /// Canvases whose global scene it was (ADR-0065).
        globals: Vec<CanvasId>,
    },
    RemoveScene(SceneId),
    ReplaceSceneHeader {
        id: SceneId,
        expected: (String, SceneScope),
        new: (String, SceneScope),
    },
    InsertItem {
        scene: SceneId,
        index: usize,
        item: SceneItem,
    },
    RemoveItem(ItemId),
    ReplaceItem {
        expected: SceneItem,
        new: SceneItem,
    },
    /// Several `ReplaceItem`s as one step (auto layout, ADR-0093).
    ReplaceItems(Vec<(SceneItem, SceneItem)>),
    MoveItem {
        item: ItemId,
        from: usize,
        to: usize,
    },
}

/// Counters for ids; ids are never reused, so undo can restore an object under its old
/// id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NextIds {
    canvas: u32,
    source: u32,
    scene: u32,
    item: u32,
    filter: u32,
}

/// The next id of each kind a collection will issue: what a document stores so ids are
/// not reused after a reload ([`SceneCollection::id_counters`],
/// [`SceneCollection::from_parts`]).
///
/// Adding a kind of id adds a field, which is a breaking change for callers that build
/// it with a struct expression; that is intended, since a document then needs the new
/// counter too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdCounters {
    /// Next canvas id.
    pub canvas: u32,
    /// Next source id.
    pub source: u32,
    /// Next scene id.
    pub scene: u32,
    /// Next item id.
    pub item: u32,
    /// Next filter id.
    pub filter: u32,
}

fn issue(counter: &mut u32) -> Result<u32, SceneError> {
    let id = *counter;
    *counter = counter.checked_add(1).ok_or(SceneError::IdsExhausted)?;
    Ok(id)
}

/// A set of canvases, sources and scenes saved together (glossary: Scene collection).
///
/// Every edit validates the whole result before changing anything, so the collection
/// always satisfies the invariants in the scene model spec, and returns an [`Undo`].
///
/// Equality compares content (canvases, sources, scenes) and ignores the id counters,
/// which undo never winds back.
#[derive(Debug, Clone, Default)]
pub struct SceneCollection {
    canvases: BTreeMap<CanvasId, Canvas>,
    sources: BTreeMap<SourceId, Source>,
    scenes: BTreeMap<SceneId, Scene>,
    next: NextIds,
}

impl PartialEq for SceneCollection {
    fn eq(&self, other: &Self) -> bool {
        self.canvases == other.canvases
            && self.sources == other.sources
            && self.scenes == other.scenes
    }
}

fn check_name(name: &str) -> Result<(), SceneError> {
    if name.trim().is_empty() {
        Err(SceneError::EmptyName)
    } else {
        Ok(())
    }
}

fn check_type(type_id: &NodeTypeId) -> Result<(), SceneError> {
    if type_id.is_well_formed() {
        Ok(())
    } else {
        Err(SceneError::BadTypeId(type_id.clone()))
    }
}

fn check_settings(settings: &Settings) -> Result<(), SceneError> {
    for (key, value) in settings {
        if let SettingValue::Float(v) = value
            && !v.is_finite()
        {
            return Err(SceneError::InvalidSetting(key.clone()));
        }
    }
    Ok(())
}

impl SceneCollection {
    /// An empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// A collection from its parts, as a document stores them: the validating loader.
    ///
    /// Names are trimmed like edits trim them. Ids must be unique per kind (items across
    /// every scene); the whole result must satisfy every invariant of the scene model
    /// spec, so a loaded collection is as valid as an edited one. A counter below an id
    /// in use is raised past the highest one, so a document with stale or missing
    /// counters never makes the collection reuse an id it holds.
    pub fn from_parts(
        canvases: Vec<Canvas>,
        sources: Vec<Source>,
        scenes: Vec<Scene>,
        counters: IdCounters,
    ) -> Result<Self, SceneError> {
        fn above(counter: u32, id: u32) -> Result<u32, SceneError> {
            Ok(counter.max(id.checked_add(1).ok_or(SceneError::IdsExhausted)?))
        }
        let mut next = NextIds {
            canvas: counters.canvas,
            source: counters.source,
            scene: counters.scene,
            item: counters.item,
            filter: counters.filter,
        };
        let mut col = SceneCollection::new();
        for mut c in canvases {
            c.name = c.name.trim().to_owned();
            next.canvas = above(next.canvas, c.id.0)?;
            let id = c.id;
            if col.canvases.insert(id, c).is_some() {
                return Err(SceneError::DuplicateId(id.to_string()));
            }
        }
        for mut s in sources {
            s.name = s.name.trim().to_owned();
            next.source = above(next.source, s.id.0)?;
            for f in &s.filters {
                next.filter = above(next.filter, f.id.0)?;
            }
            let id = s.id;
            if col.sources.insert(id, s).is_some() {
                return Err(SceneError::DuplicateId(id.to_string()));
            }
        }
        let mut items = BTreeSet::new();
        for mut s in scenes {
            s.name = s.name.trim().to_owned();
            next.scene = above(next.scene, s.id.0)?;
            for i in &s.items {
                next.item = above(next.item, i.id.0)?;
                if !items.insert(i.id) {
                    return Err(SceneError::DuplicateId(i.id.to_string()));
                }
            }
            let id = s.id;
            if col.scenes.insert(id, s).is_some() {
                return Err(SceneError::DuplicateId(id.to_string()));
            }
        }
        col.next = next;
        col.validate()?;
        Ok(col)
    }

    /// The next id of each kind this collection will issue.
    pub fn id_counters(&self) -> IdCounters {
        let n = &self.next;
        IdCounters {
            canvas: n.canvas,
            source: n.source,
            scene: n.scene,
            item: n.item,
            filter: n.filter,
        }
    }

    // ----- queries -----

    /// Canvases by id.
    pub fn canvases(&self) -> impl Iterator<Item = &Canvas> {
        self.canvases.values()
    }

    /// Sources by id.
    pub fn sources(&self) -> impl Iterator<Item = &Source> {
        self.sources.values()
    }

    /// Scenes by id.
    pub fn scenes(&self) -> impl Iterator<Item = &Scene> {
        self.scenes.values()
    }

    /// A canvas.
    pub fn canvas(&self, id: CanvasId) -> Option<&Canvas> {
        self.canvases.get(&id)
    }

    /// A source.
    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.sources.get(&id)
    }

    /// A scene.
    pub fn scene(&self, id: SceneId) -> Option<&Scene> {
        self.scenes.get(&id)
    }

    /// An item and the scene that holds it.
    pub fn item(&self, id: ItemId) -> Option<(SceneId, &SceneItem)> {
        self.scenes
            .values()
            .find_map(|s| s.items.iter().find(|i| i.id == id).map(|i| (s.id, i)))
    }

    /// Scenes canvas `canvas` may show: its own and shared ones (U15: the scene list
    /// shows which).
    pub fn scenes_for(&self, canvas: CanvasId) -> impl Iterator<Item = &Scene> {
        self.scenes.values().filter(move |s| match s.scope {
            SceneScope::Shared => true,
            SceneScope::Canvas(c) => c == canvas,
        })
    }

    /// Items (scene, item) that place source `id`.
    pub fn placements_of(&self, id: SourceId) -> Vec<(SceneId, ItemId)> {
        self.items_where(|c| c == ItemContent::Source(id))
    }

    fn items_where(&self, pred: impl Fn(ItemContent) -> bool) -> Vec<(SceneId, ItemId)> {
        self.scenes
            .values()
            .flat_map(|s| {
                s.items
                    .iter()
                    .filter(|i| pred(i.content))
                    .map(move |i| (s.id, i.id))
            })
            .collect()
    }

    // ----- edits -----

    /// Adds a canvas.
    pub fn add_canvas(
        &mut self,
        name: &str,
        size: Size,
        frame_rate: FrameRate,
    ) -> Result<(CanvasId, Undo), SceneError> {
        let mut next = self.next.clone();
        let id = CanvasId(issue(&mut next.canvas)?);
        let canvas = Canvas {
            id,
            name: name.trim().to_owned(),
            size,
            frame_rate,
            program: None,
            preview: None,
            globals: None,
            master: MasterAudio::default(),
            follow: None,
            fallback: Fallback::default(),
            audio_from: AudioFrom::default(),
        };
        let undo = self.apply_with(
            Op::InsertCanvas {
                canvas,
                overrides: Vec::new(),
                globals: None,
            },
            next,
        )?;
        Ok((id, undo))
    }

    /// Removes a canvas. Fails while scenes are scoped to it; per-canvas layouts for
    /// it are dropped (and restored by the undo).
    pub fn remove_canvas(&mut self, id: CanvasId) -> Result<Undo, SceneError> {
        self.apply(Op::RemoveCanvas(id))
    }

    /// Renames a canvas.
    pub fn rename_canvas(&mut self, id: CanvasId, name: &str) -> Result<Undo, SceneError> {
        let mut canvas = self.get_canvas(id)?.clone();
        canvas.name = name.trim().to_owned();
        self.replace_canvas(canvas)
    }

    /// Changes a canvas's size and frame rate.
    pub fn set_canvas_format(
        &mut self,
        id: CanvasId,
        size: Size,
        frame_rate: FrameRate,
    ) -> Result<Undo, SceneError> {
        let mut canvas = self.get_canvas(id)?.clone();
        canvas.size = size;
        canvas.frame_rate = frame_rate;
        self.replace_canvas(canvas)
    }

    /// Makes `canvas` show the program scene of `follow` (ADR-0093), or its own with
    /// `None`. `InvalidFollow` when `follow` is the canvas itself, unknown, itself a
    /// follower, or followed by another canvas, or when a fallback scene cannot be shown
    /// on `canvas`.
    pub fn set_canvas_follow(
        &mut self,
        canvas: CanvasId,
        follow: Option<CanvasId>,
        fallback: Fallback,
    ) -> Result<Undo, SceneError> {
        let mut c = self.get_canvas(canvas)?.clone();
        c.follow = follow;
        c.fallback = fallback;
        self.replace_canvas(c)
    }

    /// The program scene each following canvas shows now (ADR-0093 decision 3): the
    /// followed canvas's program scene when the follower may show it, else its fallback
    /// (`Scene`, or `KeepLast`: the scene in `last`, then its own program scene). A
    /// follower with nothing to show is left out. `last` holds what each follower showed
    /// the previous time.
    pub fn followed_programs(
        &self,
        last: &BTreeMap<CanvasId, SceneId>,
    ) -> BTreeMap<CanvasId, SceneId> {
        let mut shown = BTreeMap::new();
        for c in self.canvases.values() {
            let Some(target) = c.follow.and_then(|f| self.canvases.get(&f)) else {
                continue;
            };
            let direct = target.program.filter(|s| {
                self.scenes
                    .get(s)
                    .is_some_and(|s| allowed_on(s.scope, c.id))
            });
            let scene = direct.or(match c.fallback {
                Fallback::Scene(s) => Some(s),
                Fallback::KeepLast => last
                    .get(&c.id)
                    .copied()
                    .filter(|s| self.scenes.contains_key(s))
                    .or(c.program),
            });
            if let Some(scene) = scene {
                shown.insert(c.id, scene);
            }
        }
        shown
    }

    /// Sets a canvas's master volume, a linear factor in `0.0..=1.0` (ADR-0081;
    /// `InvalidVolume` otherwise). A live control: the binder ignores it.
    pub fn set_canvas_master_volume(
        &mut self,
        id: CanvasId,
        volume: f32,
    ) -> Result<Undo, SceneError> {
        let expected = self.get_canvas(id)?.master;
        let new = MasterAudio { volume, ..expected };
        if !new.is_valid() {
            return Err(SceneError::InvalidVolume);
        }
        self.apply(Op::SetCanvasMaster {
            canvas: id,
            expected,
            new,
        })
    }

    /// Mutes or unmutes a canvas's master (ADR-0081); the volume is kept.
    pub fn set_canvas_master_muted(
        &mut self,
        id: CanvasId,
        muted: bool,
    ) -> Result<Undo, SceneError> {
        let expected = self.get_canvas(id)?.master;
        let new = MasterAudio { muted, ..expected };
        self.apply(Op::SetCanvasMaster {
            canvas: id,
            expected,
            new,
        })
    }

    /// Puts `scene` on air on `canvas` (`None`: nothing on air). A cut; transitions
    /// are run by the engine between the old and new program.
    pub fn set_program(
        &mut self,
        canvas: CanvasId,
        scene: Option<SceneId>,
    ) -> Result<Undo, SceneError> {
        let mut c = self.get_canvas(canvas)?.clone();
        c.program = scene;
        self.replace_canvas(c)
    }

    /// Sets the preview scene of `canvas` (`None` turns studio mode off).
    pub fn set_preview(
        &mut self,
        canvas: CanvasId,
        scene: Option<SceneId>,
    ) -> Result<Undo, SceneError> {
        let mut c = self.get_canvas(canvas)?.clone();
        c.preview = scene;
        self.replace_canvas(c)
    }

    /// Makes `scene` the global scene of `canvas` (ADR-0065), or clears it with
    /// `None`. The scene must be scoped to that canvas, shown nowhere, nested nowhere
    /// and hold only sources (`GlobalScene`).
    pub fn set_globals(
        &mut self,
        canvas: CanvasId,
        scene: Option<SceneId>,
    ) -> Result<Undo, SceneError> {
        let mut c = self.get_canvas(canvas)?.clone();
        c.globals = scene;
        self.replace_canvas(c)
    }

    /// The scenes `canvas` may show as program or preview, in id order: those
    /// `scenes_for` gives, without global scenes (ADR-0065). Scene lists, switching
    /// and program fallbacks use this; code that must reach every item of the canvas
    /// (a rescale) uses `scenes_for`.
    pub fn showable_scenes(&self, canvas: CanvasId) -> impl Iterator<Item = &Scene> {
        let globals = self.globals_of(canvas);
        self.scenes_for(canvas)
            .filter(move |s| Some(s.id) != globals)
    }

    /// The global scene of `canvas`, if it has one (ADR-0065).
    pub fn globals_of(&self, canvas: CanvasId) -> Option<SceneId> {
        self.canvases.get(&canvas).and_then(|c| c.globals)
    }

    /// True when `scene` is the global scene of some canvas.
    pub fn is_global_scene(&self, scene: SceneId) -> bool {
        self.canvases.values().any(|c| c.globals == Some(scene))
    }

    /// Adds a source that is not yet placed in any scene.
    pub fn add_source(
        &mut self,
        name: &str,
        type_id: NodeTypeId,
        settings: Settings,
    ) -> Result<(SourceId, Undo), SceneError> {
        let mut next = self.next.clone();
        let id = SourceId(issue(&mut next.source)?);
        let source = Source {
            id,
            name: name.trim().to_owned(),
            type_id,
            settings,
            filters: Vec::new(),
            audio: SourceAudio::default(),
            hints: Default::default(),
        };
        let undo = self.apply_with(
            Op::InsertSource {
                source,
                placements: Vec::new(),
            },
            next,
        )?;
        Ok((id, undo))
    }

    /// Deletes a source and removes it from every scene (glossary: Delete source).
    ///
    /// The companions of the source ("Capture audio too", ADR-0096) go with it, in the
    /// same undo record.
    pub fn delete_source(&mut self, id: SourceId) -> Result<Undo, SceneError> {
        let companions: Vec<SourceId> = self
            .sources
            .values()
            .filter(|s| s.companion_of() == Some(id))
            .map(|s| s.id)
            .collect();
        if companions.is_empty() {
            return self.apply(Op::RemoveSource(id));
        }
        // The picture must exist; the companions are removed first so a failed
        // validation leaves the collection untouched (`apply_with` works on a copy).
        self.get_source(id)?;
        let ops = companions
            .into_iter()
            .chain([id])
            .map(Op::RemoveSource)
            .collect();
        self.apply(Op::Batch(ops))
    }

    /// Renames a source.
    pub fn rename_source(&mut self, id: SourceId, name: &str) -> Result<Undo, SceneError> {
        let mut source = self.get_source(id)?.clone();
        source.name = name.trim().to_owned();
        self.replace_source(source, None)
    }

    /// Replaces a source's settings.
    pub fn set_source_settings(
        &mut self,
        id: SourceId,
        settings: Settings,
    ) -> Result<Undo, SceneError> {
        let mut source = self.get_source(id)?.clone();
        source.settings = settings;
        self.replace_source(source, None)
    }

    /// Replaces a source's settings and its hints in one step (a re-pick: undo restores
    /// both). The binder compares settings only, so hints alone never restart a source.
    pub fn set_source_settings_and_hints(
        &mut self,
        id: SourceId,
        settings: Settings,
        hints: std::collections::BTreeMap<String, String>,
    ) -> Result<Undo, SceneError> {
        let mut source = self.get_source(id)?.clone();
        source.settings = settings;
        source.hints = hints;
        self.replace_source(source, None)
    }

    /// Sets a source's volume, a linear factor in `0.0..=1.0` (ADR-0059; `InvalidVolume`
    /// otherwise). The binder ignores it: the source keeps its node.
    pub fn set_source_volume(&mut self, id: SourceId, volume: f32) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio { volume, ..expected };
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Mutes or unmutes a source's sound (ADR-0059); the volume is kept.
    pub fn set_source_muted(&mut self, id: SourceId, muted: bool) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio { muted, ..expected };
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Sets whether a source is heard on the monitor output (ADR-0097 decision 5). A live
    /// control like the mute: the binder ignores it and the application sends it to the
    /// source's mixer strip.
    pub fn set_source_monitor(
        &mut self,
        id: SourceId,
        monitor: SourceMonitor,
    ) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio {
            monitor,
            ..expected
        };
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Hides a source from the Mixer or shows it again (ADR-0080). A view choice like the
    /// mute: the sound and the volume are untouched and the binder ignores it.
    pub fn set_source_mixer_hidden(
        &mut self,
        id: SourceId,
        mixer_hidden: bool,
    ) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio {
            mixer_hidden,
            ..expected
        };
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Sets the sync offset of a source's sound in milliseconds (ADR-0062;
    /// `InvalidSyncOffset` outside `-MAX_SYNC_OFFSET_MS..=MAX_SYNC_OFFSET_MS`). A live
    /// control like the volume: the binder ignores it.
    pub fn set_source_sync_offset(
        &mut self,
        id: SourceId,
        sync_offset_ms: i32,
    ) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = expected.with_sync_offset(sync_offset_ms);
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Replaces a filter's settings (ADR-0062). The model stores what it is given;
    /// callers check them against the filter type first.
    pub fn set_filter_settings(
        &mut self,
        source: SourceId,
        id: FilterId,
        settings: Settings,
    ) -> Result<Undo, SceneError> {
        let mut s = self.get_source(source)?.clone();
        let index = Self::filter_index(&s, id)?;
        s.filters[index].settings = settings;
        self.replace_source(s, None)
    }

    /// Adds a filter at the end of a source's chain.
    pub fn add_filter(
        &mut self,
        source: SourceId,
        type_id: NodeTypeId,
        settings: Settings,
    ) -> Result<(FilterId, Undo), SceneError> {
        let mut next = self.next.clone();
        let id = FilterId(issue(&mut next.filter)?);
        let mut s = self.get_source(source)?.clone();
        s.filters.push(Filter {
            id,
            type_id,
            enabled: true,
            settings,
        });
        let undo = self.replace_source(s, Some(next))?;
        Ok((id, undo))
    }

    /// Removes a filter.
    pub fn remove_filter(&mut self, source: SourceId, id: FilterId) -> Result<Undo, SceneError> {
        let mut s = self.get_source(source)?.clone();
        let index = Self::filter_index(&s, id)?;
        s.filters.remove(index);
        self.replace_source(s, None)
    }

    /// Enables or disables a filter.
    pub fn set_filter_enabled(
        &mut self,
        source: SourceId,
        id: FilterId,
        enabled: bool,
    ) -> Result<Undo, SceneError> {
        let mut s = self.get_source(source)?.clone();
        let index = Self::filter_index(&s, id)?;
        s.filters[index].enabled = enabled;
        self.replace_source(s, None)
    }

    /// Moves a filter to position `index` in the chain.
    pub fn move_filter(
        &mut self,
        source: SourceId,
        id: FilterId,
        index: usize,
    ) -> Result<Undo, SceneError> {
        let mut s = self.get_source(source)?.clone();
        let from = Self::filter_index(&s, id)?;
        if index >= s.filters.len() {
            return Err(SceneError::IndexOutOfRange(index));
        }
        let filter = s.filters.remove(from);
        s.filters.insert(index, filter);
        self.replace_source(s, None)
    }

    /// Adds an empty scene.
    pub fn add_scene(
        &mut self,
        name: &str,
        scope: SceneScope,
    ) -> Result<(SceneId, Undo), SceneError> {
        let mut next = self.next.clone();
        let id = SceneId(issue(&mut next.scene)?);
        let scene = Scene {
            id,
            name: name.trim().to_owned(),
            scope,
            items: Vec::new(),
        };
        let undo = self.apply_with(
            Op::InsertScene {
                scene,
                placements: Vec::new(),
                shown: Vec::new(),
                globals: Vec::new(),
            },
            next,
        )?;
        Ok((id, undo))
    }

    /// Removes a scene. Items that nest it in other scenes are removed, and canvases
    /// showing it show nothing; the undo restores all of it.
    pub fn remove_scene(&mut self, id: SceneId) -> Result<Undo, SceneError> {
        self.apply(Op::RemoveScene(id))
    }

    /// Renames a scene.
    pub fn rename_scene(&mut self, id: SceneId, name: &str) -> Result<Undo, SceneError> {
        let scene = self.get_scene(id)?;
        let expected = (scene.name.clone(), scene.scope);
        let new = (name.trim().to_owned(), scene.scope);
        self.apply(Op::ReplaceSceneHeader { id, expected, new })
    }

    /// Changes which canvases may show a scene. Refused if a canvas that shows it, or a
    /// scene that nests it, would no longer be allowed to.
    pub fn set_scene_scope(&mut self, id: SceneId, scope: SceneScope) -> Result<Undo, SceneError> {
        let scene = self.get_scene(id)?;
        let expected = (scene.name.clone(), scene.scope);
        let new = (scene.name.clone(), scope);
        self.apply(Op::ReplaceSceneHeader { id, expected, new })
    }

    /// Places a source or scene in `scene` at `index` (0 is the bottom; `None` puts it
    /// on top).
    pub fn add_item(
        &mut self,
        scene: SceneId,
        content: ItemContent,
        layout: Layout,
        index: Option<usize>,
    ) -> Result<(ItemId, Undo), SceneError> {
        let mut next = self.next.clone();
        let id = ItemId(issue(&mut next.item)?);
        let index = match index {
            Some(i) => i,
            None => self.get_scene(scene)?.items.len(),
        };
        let item = SceneItem {
            id,
            content,
            visible: true,
            locked: false,
            layout,
            overrides: BTreeMap::new(),
        };
        let undo = self.apply_with(Op::InsertItem { scene, index, item }, next)?;
        Ok((id, undo))
    }

    /// Removes an item from its scene only (glossary: Remove from scene).
    pub fn remove_item(&mut self, id: ItemId) -> Result<Undo, SceneError> {
        self.apply(Op::RemoveItem(id))
    }

    /// Moves an item to position `index` in its scene (0 is the bottom).
    pub fn move_item(&mut self, id: ItemId, index: usize) -> Result<Undo, SceneError> {
        let (scene, _) = self.item(id).ok_or(SceneError::UnknownItem(id))?;
        let from = self
            .get_scene(scene)?
            .items
            .iter()
            .position(|i| i.id == id)
            .ok_or(SceneError::UnknownItem(id))?;
        self.apply(Op::MoveItem {
            item: id,
            from,
            to: index,
        })
    }

    /// Shows or hides an item.
    pub fn set_item_visible(&mut self, id: ItemId, visible: bool) -> Result<Undo, SceneError> {
        let mut item = self.get_item(id)?.clone();
        item.visible = visible;
        self.replace_item(item)
    }

    /// Locks or unlocks an item (ADR-0059): a locked item cannot be changed with the
    /// pointer on the picture panel. Nothing here refuses edits of locked items.
    pub fn set_item_locked(&mut self, id: ItemId, locked: bool) -> Result<Undo, SceneError> {
        let mut item = self.get_item(id)?.clone();
        item.locked = locked;
        self.replace_item(item)
    }

    /// Sets an item's layout: the default (`canvas` is `None`) or the one for one
    /// canvas (U15: independent layout per canvas).
    pub fn set_item_layout(
        &mut self,
        id: ItemId,
        canvas: Option<CanvasId>,
        layout: Layout,
    ) -> Result<Undo, SceneError> {
        let mut item = self.get_item(id)?.clone();
        match canvas {
            None => item.layout = layout,
            Some(c) => {
                self.get_canvas(c)?;
                item.overrides.insert(c, layout);
            }
        }
        self.replace_item(item)
    }

    /// Arranges the visible source items of `scene` for `canvas` (ADR-0093 decision 4):
    /// each item gets a layout of its own for that canvas, in one undo step. `picture`
    /// gives the size of a source's uncropped picture (`None`: the item keeps its
    /// layout); `focus` the cut point of an item (`0.0` to `1.0`, default the centre).
    /// Nested scenes and hidden items are left alone.
    pub fn auto_layout(
        &mut self,
        canvas: CanvasId,
        scene: SceneId,
        mode: AutoLayout,
        picture: &dyn Fn(SourceId) -> Option<(u32, u32)>,
        focus: &BTreeMap<ItemId, f32>,
    ) -> Result<Undo, SceneError> {
        let target = self.get_canvas(canvas)?;
        let size = (target.size.width, target.size.height);
        let s = self.get_scene(scene)?;
        let mut chosen: Vec<(&SceneItem, (u32, u32))> = Vec::new();
        for item in &s.items {
            if let (true, ItemContent::Source(src)) = (item.visible, item.content)
                && let Some(p) = picture(src)
            {
                chosen.push((item, p));
            }
        }
        let inputs: Vec<AutoItem> = chosen
            .iter()
            .map(|(item, p)| AutoItem {
                layout: item.overrides.get(&canvas).copied().unwrap_or(item.layout),
                picture: *p,
                focus: focus.get(&item.id).copied().unwrap_or(0.5),
            })
            .collect();
        let layouts = auto_layout(mode, size, &inputs);
        let mut pairs = Vec::new();
        for ((item, _), layout) in chosen.iter().zip(layouts) {
            let Some(layout) = layout else { continue };
            let mut new = (*item).clone();
            new.overrides.insert(canvas, layout);
            pairs.push(((*item).clone(), new));
        }
        self.apply(Op::ReplaceItems(pairs))
    }

    /// Drops an item's layout for `canvas`, so it uses the default layout there.
    pub fn clear_item_layout(&mut self, id: ItemId, canvas: CanvasId) -> Result<Undo, SceneError> {
        let mut item = self.get_item(id)?.clone();
        item.overrides.remove(&canvas);
        self.replace_item(item)
    }

    /// Applies an undo (or redo) record and returns the record that reverses it.
    pub fn undo(&mut self, undo: Undo) -> Result<Undo, SceneError> {
        self.apply(undo.0)
    }

    // ----- internals -----

    fn replace_canvas(&mut self, new: Canvas) -> Result<Undo, SceneError> {
        let expected = self.get_canvas(new.id)?.clone();
        self.apply(Op::ReplaceCanvas { expected, new })
    }

    fn replace_source(&mut self, new: Source, next: Option<NextIds>) -> Result<Undo, SceneError> {
        let expected = self.get_source(new.id)?.clone();
        let next = next.unwrap_or_else(|| self.next.clone());
        self.apply_with(Op::ReplaceSource { expected, new }, next)
    }

    fn replace_item(&mut self, new: SceneItem) -> Result<Undo, SceneError> {
        let expected = self.get_item(new.id)?.clone();
        self.apply(Op::ReplaceItem { expected, new })
    }

    fn get_canvas(&self, id: CanvasId) -> Result<&Canvas, SceneError> {
        self.canvases.get(&id).ok_or(SceneError::UnknownCanvas(id))
    }

    fn get_source(&self, id: SourceId) -> Result<&Source, SceneError> {
        self.sources.get(&id).ok_or(SceneError::UnknownSource(id))
    }

    fn get_scene(&self, id: SceneId) -> Result<&Scene, SceneError> {
        self.scenes.get(&id).ok_or(SceneError::UnknownScene(id))
    }

    fn get_item(&self, id: ItemId) -> Result<&SceneItem, SceneError> {
        self.item(id)
            .map(|(_, i)| i)
            .ok_or(SceneError::UnknownItem(id))
    }

    fn filter_index(source: &Source, id: FilterId) -> Result<usize, SceneError> {
        source
            .filters
            .iter()
            .position(|f| f.id == id)
            .ok_or(SceneError::UnknownFilter(id))
    }

    fn apply(&mut self, op: Op) -> Result<Undo, SceneError> {
        let next = self.next.clone();
        self.apply_with(op, next)
    }

    /// Applies `op` to a copy, validates the copy, and commits it with `next` ids.
    /// The copy keeps the code simple and makes every edit all-or-nothing; collections
    /// hold hundreds of objects and edits are user-paced, on the control thread.
    fn apply_with(&mut self, op: Op, next: NextIds) -> Result<Undo, SceneError> {
        let mut draft = self.clone();
        draft.next = next;
        let inverse = draft.perform(op)?;
        draft.validate()?;
        *self = draft;
        Ok(Undo(inverse))
    }

    /// Checks that an id being restored was issued and is free.
    fn restorable(issued: u32, raw: u32, in_use: bool) -> Result<(), SceneError> {
        if raw >= issued || in_use {
            Err(SceneError::UndoConflict)
        } else {
            Ok(())
        }
    }

    fn take_item(&mut self, id: ItemId) -> Result<(SceneId, usize, SceneItem), SceneError> {
        for scene in self.scenes.values_mut() {
            if let Some(index) = scene.items.iter().position(|i| i.id == id) {
                return Ok((scene.id, index, scene.items.remove(index)));
            }
        }
        Err(SceneError::UnknownItem(id))
    }

    fn insert_item(
        &mut self,
        scene: SceneId,
        index: usize,
        item: SceneItem,
    ) -> Result<(), SceneError> {
        Self::restorable(self.next.item, item.id.0, self.item(item.id).is_some())?;
        let s = self
            .scenes
            .get_mut(&scene)
            .ok_or(SceneError::UnknownScene(scene))?;
        if index > s.items.len() {
            return Err(SceneError::IndexOutOfRange(index));
        }
        s.items.insert(index, item);
        Ok(())
    }

    /// Removes every item matching `content`, returning them with their positions in
    /// an order that `restore_items` puts back exactly.
    fn remove_items(&mut self, content: ItemContent) -> Vec<(SceneId, usize, SceneItem)> {
        let mut removed = Vec::new();
        for scene in self.scenes.values_mut() {
            let mut index = 0;
            let mut taken = 0;
            while index < scene.items.len() {
                if scene.items[index].content == content {
                    // Record the original position; restoring in ascending order then
                    // puts every item back exactly where it was.
                    removed.push((scene.id, index + taken, scene.items.remove(index)));
                    taken += 1;
                } else {
                    index += 1;
                }
            }
        }
        removed
    }

    fn restore_items(&mut self, items: Vec<(SceneId, usize, SceneItem)>) -> Result<(), SceneError> {
        for (scene, index, item) in items {
            self.insert_item(scene, index, item)?;
        }
        Ok(())
    }

    fn perform(&mut self, op: Op) -> Result<Op, SceneError> {
        match op {
            Op::InsertCanvas {
                canvas,
                overrides,
                globals,
            } => {
                let id = canvas.id;
                Self::restorable(self.next.canvas, id.0, self.canvases.contains_key(&id))?;
                self.canvases.insert(id, canvas);
                if let Some(scene) = globals {
                    let sid = scene.id;
                    Self::restorable(self.next.scene, sid.0, self.scenes.contains_key(&sid))?;
                    for item in &scene.items {
                        Self::restorable(self.next.item, item.id.0, self.item(item.id).is_some())?;
                    }
                    self.scenes.insert(sid, scene);
                }
                for (item, layout) in overrides {
                    let item = self.item_mut(item)?;
                    item.overrides.insert(id, layout);
                }
                Ok(Op::RemoveCanvas(id))
            }
            Op::RemoveCanvas(id) => {
                let canvas = self
                    .canvases
                    .remove(&id)
                    .ok_or(SceneError::UnknownCanvas(id))?;
                // Its global scene goes with it (ADR-0065): the user never sees it as
                // a scene of its own. Validation keeps it nested nowhere.
                let globals = canvas.globals.and_then(|g| self.scenes.remove(&g));
                if self
                    .scenes
                    .values()
                    .any(|s| s.scope == SceneScope::Canvas(id))
                {
                    return Err(SceneError::CanvasHasScenes(id));
                }
                let mut overrides = Vec::new();
                for scene in self.scenes.values_mut() {
                    for item in scene.items.iter_mut() {
                        if let Some(layout) = item.overrides.remove(&id) {
                            overrides.push((item.id, layout));
                        }
                    }
                }
                Ok(Op::InsertCanvas {
                    canvas,
                    overrides,
                    globals,
                })
            }
            Op::SetCanvasMaster {
                canvas,
                expected,
                new,
            } => {
                let slot = self
                    .canvases
                    .get_mut(&canvas)
                    .ok_or(SceneError::UnknownCanvas(canvas))?;
                if slot.master != expected {
                    return Err(SceneError::UndoConflict);
                }
                slot.master = new;
                Ok(Op::SetCanvasMaster {
                    canvas,
                    expected: new,
                    new: expected,
                })
            }
            Op::ReplaceCanvas {
                mut expected,
                mut new,
            } => {
                let slot = self
                    .canvases
                    .get_mut(&new.id)
                    .ok_or(SceneError::UnknownCanvas(new.id))?;
                // The master is a live control with a record of its own: replacing a
                // canvas keeps its current master, so a fader move never makes an
                // earlier rename conflict or come back.
                expected.master = slot.master;
                new.master = slot.master;
                if *slot != expected || expected.id != new.id {
                    return Err(SceneError::UndoConflict);
                }
                *slot = new.clone();
                Ok(Op::ReplaceCanvas {
                    expected: new,
                    new: expected,
                })
            }
            Op::InsertSource { source, placements } => {
                let id = source.id;
                Self::restorable(self.next.source, id.0, self.sources.contains_key(&id))?;
                for f in &source.filters {
                    if f.id.0 >= self.next.filter {
                        return Err(SceneError::UndoConflict);
                    }
                }
                self.sources.insert(id, source);
                self.restore_items(placements)?;
                Ok(Op::RemoveSource(id))
            }
            Op::Batch(ops) => {
                let mut inverse = Vec::with_capacity(ops.len());
                for op in ops {
                    inverse.push(self.perform(op)?);
                }
                inverse.reverse();
                Ok(Op::Batch(inverse))
            }
            Op::RemoveSource(id) => {
                let source = self
                    .sources
                    .remove(&id)
                    .ok_or(SceneError::UnknownSource(id))?;
                let placements = self.remove_items(ItemContent::Source(id));
                Ok(Op::InsertSource { source, placements })
            }
            Op::SetSourceAudio {
                source,
                expected,
                new,
            } => {
                let slot = self
                    .sources
                    .get_mut(&source)
                    .ok_or(SceneError::UnknownSource(source))?;
                if slot.audio != expected {
                    return Err(SceneError::UndoConflict);
                }
                slot.audio = new;
                Ok(Op::SetSourceAudio {
                    source,
                    expected: new,
                    new: expected,
                })
            }
            Op::ReplaceSource {
                mut expected,
                mut new,
            } => {
                let issued = self.next.filter;
                let slot = self
                    .sources
                    .get_mut(&new.id)
                    .ok_or(SceneError::UnknownSource(new.id))?;
                // Volume and mute are live controls with records of their own
                // (`SetSourceAudio`): replacing a source keeps its current sound, so a
                // fader move never makes an earlier rename conflict or come back.
                expected.audio = slot.audio;
                new.audio = slot.audio;
                if *slot != expected
                    || expected.id != new.id
                    || new.filters.iter().any(|f| f.id.0 >= issued)
                {
                    return Err(SceneError::UndoConflict);
                }
                *slot = new.clone();
                Ok(Op::ReplaceSource {
                    expected: new,
                    new: expected,
                })
            }
            Op::InsertScene {
                scene,
                placements,
                shown,
                globals,
            } => {
                let id = scene.id;
                Self::restorable(self.next.scene, id.0, self.scenes.contains_key(&id))?;
                self.scenes.insert(id, scene);
                self.restore_items(placements)?;
                for (canvas, program, preview) in shown {
                    let c = self
                        .canvases
                        .get_mut(&canvas)
                        .ok_or(SceneError::UndoConflict)?;
                    if program {
                        c.program = Some(id);
                    }
                    if preview {
                        c.preview = Some(id);
                    }
                }
                for canvas in globals {
                    let c = self
                        .canvases
                        .get_mut(&canvas)
                        .ok_or(SceneError::UndoConflict)?;
                    // Never replace a global scene set since.
                    if c.globals.is_some() {
                        return Err(SceneError::UndoConflict);
                    }
                    c.globals = Some(id);
                }
                Ok(Op::RemoveScene(id))
            }
            Op::RemoveScene(id) => {
                let scene = self
                    .scenes
                    .remove(&id)
                    .ok_or(SceneError::UnknownScene(id))?;
                let placements = self.remove_items(ItemContent::Scene(id));
                let mut shown = Vec::new();
                let mut globals = Vec::new();
                for c in self.canvases.values_mut() {
                    if c.globals == Some(id) {
                        c.globals = None;
                        globals.push(c.id);
                    }
                    let program = c.program == Some(id);
                    let preview = c.preview == Some(id);
                    if program {
                        c.program = None;
                    }
                    if preview {
                        c.preview = None;
                    }
                    if program || preview {
                        shown.push((c.id, program, preview));
                    }
                }
                Ok(Op::InsertScene {
                    scene,
                    placements,
                    shown,
                    globals,
                })
            }
            Op::ReplaceSceneHeader { id, expected, new } => {
                let s = self
                    .scenes
                    .get_mut(&id)
                    .ok_or(SceneError::UnknownScene(id))?;
                if s.name != expected.0 || s.scope != expected.1 {
                    return Err(SceneError::UndoConflict);
                }
                s.name = new.0.clone();
                s.scope = new.1;
                Ok(Op::ReplaceSceneHeader {
                    id,
                    expected: new,
                    new: expected,
                })
            }
            Op::InsertItem { scene, index, item } => {
                let id = item.id;
                self.insert_item(scene, index, item)?;
                Ok(Op::RemoveItem(id))
            }
            Op::RemoveItem(id) => {
                let (scene, index, item) = self.take_item(id)?;
                Ok(Op::InsertItem { scene, index, item })
            }
            Op::ReplaceItem { expected, new } => {
                let slot = self.item_mut(new.id)?;
                if *slot != expected || expected.id != new.id || expected.content != new.content {
                    return Err(SceneError::UndoConflict);
                }
                *slot = new.clone();
                Ok(Op::ReplaceItem {
                    expected: new,
                    new: expected,
                })
            }
            Op::ReplaceItems(pairs) => {
                let mut inverse = Vec::with_capacity(pairs.len());
                for (expected, new) in pairs {
                    let slot = self.item_mut(new.id)?;
                    if *slot != expected || expected.id != new.id || expected.content != new.content
                    {
                        return Err(SceneError::UndoConflict);
                    }
                    *slot = new.clone();
                    inverse.push((new, expected));
                }
                inverse.reverse();
                Ok(Op::ReplaceItems(inverse))
            }
            Op::MoveItem { item, from, to } => {
                let index = to;
                let (scene, at, value) = self.take_item(item)?;
                if at != from {
                    return Err(SceneError::UndoConflict);
                }
                let s = self
                    .scenes
                    .get_mut(&scene)
                    .ok_or(SceneError::UnknownScene(scene))?;
                if index > s.items.len() {
                    return Err(SceneError::IndexOutOfRange(index));
                }
                s.items.insert(index, value);
                Ok(Op::MoveItem {
                    item,
                    from: to,
                    to: from,
                })
            }
        }
    }

    fn item_mut(&mut self, id: ItemId) -> Result<&mut SceneItem, SceneError> {
        self.scenes
            .values_mut()
            .flat_map(|s| s.items.iter_mut())
            .find(|i| i.id == id)
            .ok_or(SceneError::UnknownItem(id))
    }

    /// Checks every invariant of the scene model spec.
    fn validate(&self) -> Result<(), SceneError> {
        let mut names = BTreeSet::new();
        for c in self.canvases.values() {
            check_name(&c.name)?;
            if !names.insert(c.name.as_str()) {
                return Err(SceneError::DuplicateName(c.name.clone()));
            }
            if !c.master.is_valid() {
                return Err(SceneError::InvalidVolume);
            }
            if let Some(f) = c.follow {
                let target = self
                    .canvases
                    .get(&f)
                    .ok_or(SceneError::InvalidFollow(c.id))?;
                if f == c.id || target.follow.is_some() {
                    return Err(SceneError::InvalidFollow(c.id));
                }
            }
            if let Fallback::Scene(scene) = c.fallback {
                let shown = self.get_scene(scene)?;
                if c.follow.is_none() || !allowed_on(shown.scope, c.id) {
                    return Err(SceneError::InvalidFollow(c.id));
                }
            }
            if c.size.width == 0 || c.size.height == 0 {
                return Err(SceneError::EmptyCanvas);
            }
            if c.size.width > MAX_CANVAS_SIDE || c.size.height > MAX_CANVAS_SIDE {
                return Err(SceneError::CanvasTooLarge);
            }
            let rate = c.frame_rate.per_second();
            if u64::from(rate.num()) > u64::from(MAX_FRAME_RATE) * u64::from(rate.den()) {
                return Err(SceneError::FrameRateTooHigh);
            }
            for shown in [c.program, c.preview].into_iter().flatten() {
                let scene = self.get_scene(shown)?;
                if !allowed_on(scene.scope, c.id) {
                    return Err(SceneError::ScopeViolation(shown));
                }
            }
        }
        self.check_globals()?;
        let mut names = BTreeSet::new();
        let mut filters = BTreeSet::new();
        for s in self.sources.values() {
            check_name(&s.name)?;
            if !names.insert(s.name.as_str()) {
                return Err(SceneError::DuplicateName(s.name.clone()));
            }
            check_type(&s.type_id)?;
            check_settings(&s.settings)?;
            if !s.audio.volume_is_valid() {
                return Err(SceneError::InvalidVolume);
            }
            if !s.audio.sync_offset_is_valid() {
                return Err(SceneError::InvalidSyncOffset);
            }
            for f in &s.filters {
                if !filters.insert(f.id) {
                    return Err(SceneError::DuplicateFilter(f.id));
                }
                check_type(&f.type_id)?;
                check_settings(&f.settings)?;
            }
        }
        let mut names = BTreeSet::new();
        let globals: BTreeSet<SceneId> = self.canvases.values().filter_map(|c| c.globals).collect();
        for s in self.scenes.values() {
            check_name(&s.name)?;
            // A global scene is never listed, so its name never takes one from the
            // user (ADR-0065).
            if !globals.contains(&s.id) && !names.insert(s.name.as_str()) {
                return Err(SceneError::DuplicateName(s.name.clone()));
            }
            if let SceneScope::Canvas(c) = s.scope {
                self.get_canvas(c)?;
            }
            for item in &s.items {
                for (canvas, layout) in std::iter::once((None, &item.layout))
                    .chain(item.overrides.iter().map(|(c, l)| (Some(*c), l)))
                {
                    if let Some(c) = canvas {
                        self.get_canvas(c)?;
                        // An override for a canvas that can never show the scene is
                        // dead data and would hide mistakes.
                        if !allowed_on(s.scope, c) {
                            return Err(SceneError::ScopeViolation(s.id));
                        }
                    }
                    if !layout.is_valid() {
                        return Err(SceneError::InvalidLayout);
                    }
                    if matches!(item.content, ItemContent::Scene(_)) && !layout.crop.is_none() {
                        return Err(SceneError::CropOnScene(item.id));
                    }
                    if matches!(item.content, ItemContent::Scene(_)) && !layout.style.is_plain() {
                        return Err(SceneError::StyleOnScene(item.id));
                    }
                }
                match item.content {
                    ItemContent::Source(id) => {
                        self.get_source(id)?;
                    }
                    ItemContent::Scene(child) => {
                        let nested = self.get_scene(child)?;
                        if !may_nest(s.scope, nested.scope) {
                            return Err(SceneError::ScopeViolation(child));
                        }
                    }
                }
            }
        }
        self.check_acyclic()?;
        self.check_size()
    }

    /// The global-scene invariants (ADR-0065): scoped to its own canvas, shown and
    /// nested nowhere, nesting nothing.
    fn check_globals(&self) -> Result<(), SceneError> {
        let globals: BTreeSet<SceneId> = self.canvases.values().filter_map(|c| c.globals).collect();
        if globals.is_empty() {
            return Ok(());
        }
        for c in self.canvases.values() {
            let Some(g) = c.globals else { continue };
            let scene = self.get_scene(g)?;
            if scene.scope != SceneScope::Canvas(c.id)
                || scene
                    .items
                    .iter()
                    .any(|i| matches!(i.content, ItemContent::Scene(_)))
            {
                return Err(SceneError::GlobalScene(g));
            }
        }
        for c in self.canvases.values() {
            for shown in [c.program, c.preview].into_iter().flatten() {
                if globals.contains(&shown) {
                    return Err(SceneError::GlobalScene(shown));
                }
            }
        }
        for s in self.scenes.values() {
            for item in &s.items {
                if let ItemContent::Scene(child) = item.content
                    && globals.contains(&child)
                {
                    return Err(SceneError::GlobalScene(child));
                }
            }
        }
        Ok(())
    }

    /// Nesting depth and flattened layer count per scene, memoised; recursion is cut
    /// off at the depth limit, so it is bounded.
    fn check_size(&self) -> Result<(), SceneError> {
        fn measure(
            col: &SceneCollection,
            scene: SceneId,
            level: usize,
            memo: &mut BTreeMap<SceneId, (usize, u64)>,
        ) -> Result<(usize, u64), SceneError> {
            if let Some(&known) = memo.get(&scene) {
                return Ok(known);
            }
            if level > MAX_NESTING_DEPTH {
                return Err(SceneError::NestingTooDeep(scene));
            }
            let mut depth = 1;
            let mut layers: u64 = 0;
            for item in &col.get_scene(scene)?.items {
                match item.content {
                    ItemContent::Source(_) => layers = layers.saturating_add(1),
                    ItemContent::Scene(child) => {
                        let (d, l) = measure(col, child, level + 1, memo)?;
                        depth = depth.max(d + 1);
                        layers = layers.saturating_add(l);
                    }
                }
            }
            if depth > MAX_NESTING_DEPTH {
                return Err(SceneError::NestingTooDeep(scene));
            }
            if layers > MAX_LAYERS {
                return Err(SceneError::TooManyLayers(scene));
            }
            memo.insert(scene, (depth, layers));
            Ok((depth, layers))
        }
        let mut memo = BTreeMap::new();
        for &scene in self.scenes.keys() {
            measure(self, scene, 1, &mut memo)?;
        }
        // A scene a canvas may show is drawn with that canvas's global scene
        // (ADR-0065): together they stay within the limit.
        for c in self.canvases.values() {
            let Some(g) = c.globals else { continue };
            let extra = memo.get(&g).map_or(0, |m| m.1);
            if extra == 0 {
                continue;
            }
            for s in self
                .scenes
                .values()
                .filter(|s| s.id != g && allowed_on(s.scope, c.id))
            {
                let own = memo.get(&s.id).map_or(0, |m| m.1);
                if own.saturating_add(extra) > MAX_LAYERS {
                    return Err(SceneError::TooManyLayers(s.id));
                }
            }
        }
        Ok(())
    }

    /// Nesting must form a DAG: depth-first search with colours.
    fn check_acyclic(&self) -> Result<(), SceneError> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        let mut marks: BTreeMap<SceneId, Mark> = BTreeMap::new();
        for &root in self.scenes.keys() {
            if marks.contains_key(&root) {
                continue;
            }
            // Stack of (scene, next child index to visit).
            let mut stack: Vec<(SceneId, usize)> = vec![(root, 0)];
            marks.insert(root, Mark::Open);
            while let Some((scene, child)) = stack.last().copied() {
                let items = &self.scenes[&scene].items;
                let next =
                    items[child.min(items.len())..]
                        .iter()
                        .enumerate()
                        .find_map(|(offset, i)| match i.content {
                            ItemContent::Scene(s) => Some((child + offset, s)),
                            ItemContent::Source(_) => None,
                        });
                match next {
                    None => {
                        marks.insert(scene, Mark::Done);
                        stack.pop();
                    }
                    Some((at, nested)) => {
                        if let Some(top) = stack.last_mut() {
                            top.1 = at + 1;
                        }
                        match marks.get(&nested) {
                            Some(Mark::Open) => return Err(SceneError::NestingCycle(nested)),
                            Some(Mark::Done) => {}
                            None => {
                                marks.insert(nested, Mark::Open);
                                stack.push((nested, 0));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// True if a scene with `scope` may be shown on `canvas`.
pub(crate) fn allowed_on(scope: SceneScope, canvas: CanvasId) -> bool {
    match scope {
        SceneScope::Shared => true,
        SceneScope::Canvas(c) => c == canvas,
    }
}

/// True if a scene with scope `child` may be nested in one with scope `parent`: a
/// shared scene may be shown anywhere, so it may only nest shared scenes.
fn may_nest(parent: SceneScope, child: SceneScope) -> bool {
    match (parent, child) {
        (_, SceneScope::Shared) => true,
        (SceneScope::Canvas(p), SceneScope::Canvas(c)) => p == c,
        (SceneScope::Shared, SceneScope::Canvas(_)) => false,
    }
}
