//! The scene collection: validated edits, each returning its own undo.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;

use crate::model::{
    Canvas, CanvasId, Filter, FilterId, ItemContent, ItemId, Layout, Scene, SceneId, SceneItem,
    SceneScope, SettingValue, Settings, Source, SourceAudio, SourceId,
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
    },
    RemoveCanvas(CanvasId),
    /// Replaces a canvas only if it currently equals `expected`, so an undo used out
    /// of order is refused instead of reverting later changes.
    ReplaceCanvas {
        expected: Canvas,
        new: Canvas,
    },
    InsertSource {
        source: Source,
        placements: Vec<(SceneId, usize, SceneItem)>,
    },
    RemoveSource(SourceId),
    /// Replaces everything of a source but its `audio`, which stays as it is.
    ReplaceSource {
        expected: Source,
        new: Source,
    },
    /// Replaces only a source's volume and mute.
    SetSourceAudio {
        source: SourceId,
        expected: SourceAudio,
        new: SourceAudio,
    },
    InsertScene {
        scene: Scene,
        placements: Vec<(SceneId, usize, SceneItem)>,
        shown: Vec<(CanvasId, bool, bool)>,
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
        };
        let undo = self.apply_with(
            Op::InsertCanvas {
                canvas,
                overrides: Vec::new(),
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
    pub fn delete_source(&mut self, id: SourceId) -> Result<Undo, SceneError> {
        self.apply(Op::RemoveSource(id))
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

    /// Sets a source's volume, a linear factor in `0.0..=1.0` (ADR-0059; `InvalidVolume`
    /// otherwise). The binder ignores it: the source keeps its node.
    pub fn set_source_volume(&mut self, id: SourceId, volume: f32) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio::new(volume, expected.muted);
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
    }

    /// Mutes or unmutes a source's sound (ADR-0059); the volume is kept.
    pub fn set_source_muted(&mut self, id: SourceId, muted: bool) -> Result<Undo, SceneError> {
        let expected = self.get_source(id)?.audio;
        let new = SourceAudio::new(expected.volume, muted);
        self.apply(Op::SetSourceAudio {
            source: id,
            expected,
            new,
        })
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
            Op::InsertCanvas { canvas, overrides } => {
                let id = canvas.id;
                Self::restorable(self.next.canvas, id.0, self.canvases.contains_key(&id))?;
                self.canvases.insert(id, canvas);
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
                Ok(Op::InsertCanvas { canvas, overrides })
            }
            Op::ReplaceCanvas { expected, new } => {
                let slot = self
                    .canvases
                    .get_mut(&new.id)
                    .ok_or(SceneError::UnknownCanvas(new.id))?;
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
                Ok(Op::RemoveScene(id))
            }
            Op::RemoveScene(id) => {
                let scene = self
                    .scenes
                    .remove(&id)
                    .ok_or(SceneError::UnknownScene(id))?;
                let placements = self.remove_items(ItemContent::Scene(id));
                let mut shown = Vec::new();
                for c in self.canvases.values_mut() {
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
        let mut names = BTreeSet::new();
        let mut filters = BTreeSet::new();
        for s in self.sources.values() {
            check_name(&s.name)?;
            if !names.insert(s.name.as_str()) {
                return Err(SceneError::DuplicateName(s.name.clone()));
            }
            check_type(&s.type_id)?;
            check_settings(&s.settings)?;
            if !s.audio.is_valid() {
                return Err(SceneError::InvalidVolume);
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
        for s in self.scenes.values() {
            check_name(&s.name)?;
            if !names.insert(s.name.as_str()) {
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
