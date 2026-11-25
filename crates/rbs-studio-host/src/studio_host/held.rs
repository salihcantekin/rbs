//! The held program (ADR-0063): a copy of the scene on air that keeps running while
//! the scene itself is edited, and the ids it takes.

use super::*;

/// Ids of the held program copy (ADR-0063): far above any id a collection issues, so
/// they never meet one, and the same for every hold that starts from a real scene, so
/// the program's compositor and layers keep their nodes from one hold to the next. A
/// copy's item has the id of the item it copies plus the copy's id.
pub(super) const HELD_IDS: u32 = 0xF000_0000;

/// The other id of a held copy: a copy taken right after another one (a transition
/// from a held program to the scene held next) gets the id the last one did not have,
/// so the studio sees the program change and runs the transition.
pub(super) const HELD_IDS_NEXT: u32 = 0xF800_0000;

/// The program as it was when its scene went into preview too (ADR-0063).
#[derive(Clone, Debug)]
pub(super) struct Held {
    /// The program scene the copy was taken from.
    pub(super) from: SceneId,
    /// The copy, with ids from [`HELD_IDS`].
    pub(super) scene: Scene,
    /// Identity of this copy, even when a replacement reuses its temporary scene id.
    pub(super) cue_identity: Arc<()>,
    /// Original item ids to the ids in this copy.
    pub(super) cue_items: BTreeMap<ItemId, ItemId>,
    /// Captured once, when this copy is taken.
    pub(super) cues: Option<Result<rbs_record::HeldShowCues, ItemId>>,
}

/// A copy of the canvas's program scene, for the program to show while the same scene
/// is edited in preview; `None` unless the canvas is in studio mode with its program
/// scene in preview.
pub(super) fn held_copy(
    collection: &SceneCollection,
    canvas: CanvasId,
    after: Option<SceneId>,
) -> Option<Held> {
    let c = collection.canvas(canvas)?;
    let program = c.program?;
    if c.preview != Some(program) {
        return None;
    }
    copy_of(collection, program, after)
}

/// A copy of `program` as it is now, to hold on air (ADR-0063), with the id that is
/// not `after`'s, the copy it follows.
pub(super) fn copy_of(
    collection: &SceneCollection,
    program: SceneId,
    after: Option<SceneId>,
) -> Option<Held> {
    let scene = collection.scene(program)?;
    let mut held = scene.clone();
    let base = if after.map(SceneId::get) == Some(HELD_IDS) {
        HELD_IDS_NEXT
    } else {
        HELD_IDS
    };
    held.id = SceneId::from_raw(base);
    // Unique in the run collection; read only if a failure names the scene.
    held.name = format!("{} (on air)", scene.name);
    while collection.scenes().any(|s| s.name == held.name) {
        held.name.push('*');
    }
    let mut cue_items = BTreeMap::new();
    for item in &mut held.items {
        let original = item.id;
        item.id = ItemId::from_raw(base.checked_add(original.get())?);
        cue_items.insert(original, item.id);
    }
    Some(Held {
        from: program,
        scene: held,
        cue_identity: Arc::new(()),
        cue_items,
        cues: None,
    })
}

/// The held copy's scene id to the scene it copies, for the studio's group Scroll
/// runs (ADR-0187 decision 2): a held group shares its original's run until an edit.
pub(super) fn scene_origins(held: Option<&Held>) -> BTreeMap<SceneId, SceneId> {
    held.map(|h| (h.scene.id, h.from)).into_iter().collect()
}

/// Whether the program the studio runs changed only between a scene and its held copy
/// (ADR-0151 follow-up): studio mode entered or left, or the scene on air previewed,
/// with the same picture on air. Each side is (run id, the user's scene). From one
/// copy to the next (a transition of a scene to itself, ADR-0063 decision 3) is a
/// real change.
pub(super) fn same_picture(
    before: Option<(SceneId, SceneId)>,
    now: Option<(SceneId, SceneId)>,
) -> bool {
    match (before, now) {
        (Some((was, was_real)), Some((id, real))) => {
            was != id && was_real == real && (was == was_real || id == real)
        }
        _ => false,
    }
}

/// What is held after a change (ADR-0063, ADR-0098): nothing outside studio mode; the
/// copy kept while its scene is on air, whatever the preview scene is, less the items
/// whose source or nested scene is gone from the collection; a new copy when the
/// program scene is also the preview scene and nothing of it is held (the hold starts:
/// studio mode entered, a transition, or an undo or redo put another scene on air);
/// nothing otherwise. Only a transition (which drops the copy) or another scene on air
/// ends a hold, so previewing another scene never puts edits on air.
pub(super) fn next_held(
    held: Option<Held>,
    collection: &SceneCollection,
    canvas: CanvasId,
    after: Option<SceneId>,
) -> Option<Held> {
    let c = collection.canvas(canvas)?;
    let program = c.program?;
    let preview = c.preview?;
    match held {
        Some(mut h) if h.from == program => {
            h.scene.items.retain(|i| match i.content {
                ItemContent::Source(id) => collection.source(id).is_some(),
                ItemContent::Scene(id) => collection.scene(id).is_some(),
                _ => true,
            });
            Some(h)
        }
        _ if preview == program => held_copy(collection, canvas, after),
        _ => None,
    }
}

/// The collection the studio runs while the program is held: `collection` with the
/// held copy added and on air on `canvas`. `None` when the copy does not fit.
pub(super) fn run_copy(
    collection: &SceneCollection,
    canvas: CanvasId,
    held: &Held,
) -> Option<SceneCollection> {
    let canvases = collection
        .canvases()
        .map(|c| {
            let mut c = c.clone();
            if c.id == canvas {
                c.program = Some(held.scene.id);
            }
            c
        })
        .collect();
    let mut scenes: Vec<Scene> = collection.scenes().cloned().collect();
    // Scene levels are not held (ADR-0091 decision 3): the copy on air carries the real
    // scene's levels as they are now, like the sources' own volume and mute.
    let mut copy = held.scene.clone();
    copy.audio = collection.scene(held.from).and_then(|s| s.audio.clone());
    scenes.push(copy);
    // The collection's glide and transition go with it: without them a held program
    // ran every change as the default Move.
    SceneCollection::from_parts_with_transition(
        canvases,
        collection.sources().cloned().collect(),
        scenes,
        collection.id_counters(),
        collection.glide(),
        collection.transition(),
    )
    .ok()
}

/// The program and its changes: the held copy, the transition and the T-bar.
pub(super) struct ProgramState {
    /// In studio mode with the program scene also in preview: the copy of the program
    /// scene the program shows until the next transition, so edits stay off air
    /// (ADR-0063). Never saved, never listed.
    pub(super) held: Option<Held>,
    /// The copy bound by the last successful studio update.
    pub(super) applied_held: Option<Held>,
    /// The held copy whose cues were installed in the running studio.
    pub(super) applied_cues: Option<Arc<()>>,
    /// Old collection cues cannot be captured for a newly loaded collection.
    pub(super) suppress_cue_capture: bool,
    /// The scene the last held copy was taken from, so a transition away from the
    /// copy names the real scene on `transition.state`.
    pub(super) held_was: Option<SceneId>,
    /// The id of the last held copy, so the next copy taken after it gets the other
    /// id and a transition from one copy to the next is seen as one.
    pub(super) held_id_was: Option<SceneId>,
    /// A program switch held back until its entering texts are uploaded, at most
    /// `TEXT_WAIT_TICKS` render ticks (ADR-0129 decision 7).
    pub(super) held_switch: Option<SceneId>,
    /// The program scene the studio ran at the last update, as (its id in the run
    /// collection, the user's scene it shows): a held copy (ADR-0063) is the scene it
    /// was taken from. `None` before the first update.
    pub(super) ran_program: Option<(SceneId, SceneId)>,
    /// Scene sources an API call places into, instead of the edited scene.
    pub(super) into: Option<SceneId>,
    /// A T-bar's end is making the program change: it does not cancel itself.
    pub(super) taking: bool,
    /// A manual transition held on the main canvas at the end of the last round.
    pub(super) tbar_manual: bool,
    /// The T-bar went to 0 this round: the rail is there already.
    pub(super) tbar_zeroed: bool,
    /// The latest T-bar value not yet applied (ADR-0135 decision 1).
    pub(super) pending_progress: Option<f32>,
}

impl ProgramState {
    pub(super) fn new(held: Option<Held>) -> Self {
        Self {
            into: None,
            applied_held: held.clone(),
            held,
            applied_cues: None,
            suppress_cue_capture: false,
            held_was: None,
            held_id_was: None,
            held_switch: None,
            ran_program: None,
            pending_progress: None,
            tbar_manual: false,
            tbar_zeroed: false,
            taking: false,
        }
    }
}

pub(super) struct CueSwap {
    previous: Option<rbs_record::HeldShowCues>,
    next: Option<Arc<()>>,
}

impl Host {
    /// Auxiliary picture and canvas calls may bind the collection but can return an
    /// error after doing so. Only the main update may install a new held identity.
    pub(super) fn auxiliary_run_collection(
        &mut self,
    ) -> Result<Option<SceneCollection>, RecordError> {
        let held = self.program.held.clone();
        let held_was = self.program.held_was;
        let held_id_was = self.program.held_id_was;
        let run = self.run_collection();
        let same = match (&self.program.held, &self.program.applied_cues) {
            (None, None) => true,
            (Some(held), Some(applied)) => Arc::ptr_eq(&held.cue_identity, applied),
            _ => false,
        };
        if same {
            Ok(run)
        } else {
            self.program.held = held;
            self.program.held_was = held_was;
            self.program.held_id_was = held_id_was;
            Err(RecordError::NotBound("pending held Program update"))
        }
    }

    /// Stage the new copy's cues before its graph is bound. The previous capture
    /// remains available until that graph update succeeds.
    pub(super) fn begin_held_cues(&mut self) -> Result<Option<CueSwap>, Option<ItemId>> {
        let next = self
            .program
            .held
            .as_ref()
            .map(|h| Arc::clone(&h.cue_identity));
        let same = match (&self.program.applied_cues, &next) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same {
            return Ok(None);
        }
        let cues = match &self.program.held {
            Some(held) => Some(held.cues.as_ref().ok_or(None)?.clone().map_err(Some)?),
            None => None,
        };
        Ok(Some(CueSwap {
            previous: self.studio.replace_held_cues(cues),
            next,
        }))
    }

    pub(super) fn finish_held_cues(&mut self, swap: Option<CueSwap>, committed: bool) {
        if let Some(swap) = swap {
            if committed {
                self.program.applied_cues = swap.next;
            } else {
                self.studio.replace_held_cues(swap.previous);
            }
        }
        if committed {
            self.program.applied_held = self.program.held.clone();
        } else {
            self.program.held = self.program.applied_held.clone();
            self.studio
                .set_scene_origins(scene_origins(self.program.applied_held.as_ref()));
            if let Some(held) = &self.program.held {
                self.program.held_id_was = Some(held.scene.id);
            }
        }
    }
}
