//! Independent copies of sources (ADR-0133 decisions 3 and 4): Duplicate source and
//! Duplicate scene with new sources. The host decides names, standby and mute (it knows
//! source classes and the filter catalogue); this module issues ids, places items,
//! re-keys scene own levels, remaps key links inside the copied set and applies
//! everything as one undo record.

use std::collections::BTreeMap;

use rbs_graph::node::NodeTypeId;

use super::{Op, SceneCollection, SceneError, Undo, issue};
use crate::model::{
    COMPANION_OF_HINT, FilterId, ItemContent, ItemId, STANDBY_HINT, Scene, SceneId, SettingValue,
    Source, SourceId,
};

/// How one source is copied (ADR-0133 decision 3). The host fills it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceCopy {
    /// The source copied.
    pub from: SourceId,
    /// The copy's name; must be free in the collection.
    pub name: String,
    /// Whether the copy starts in standby (the `standby` hint).
    pub standby: bool,
    /// `Some` sets the copy's mute (a companion copy starts muted); `None` keeps the
    /// original's.
    pub muted: Option<bool>,
}

impl SourceCopy {
    /// A copy of `from` named `name`, in the original's standby state and mute.
    pub fn new(from: SourceId, name: &str, standby: bool) -> Self {
        Self {
            from,
            name: name.to_owned(),
            standby,
            muted: None,
        }
    }

    /// The same copy with this mute.
    pub fn with_muted(self, muted: bool) -> Self {
        Self {
            muted: Some(muted),
            ..self
        }
    }
}

/// What a duplicate made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Duplicated {
    /// Each copied source and its copy, in the order of the request.
    pub sources: Vec<(SourceId, SourceId)>,
    /// Each new item of a copy, `(copy, item)`, in the order placed.
    pub items: Vec<(SourceId, ItemId)>,
}

impl Duplicated {
    /// The copy of `from`, if it was copied.
    pub fn copy_of(&self, from: SourceId) -> Option<SourceId> {
        self.sources
            .iter()
            .find(|(f, _)| *f == from)
            .map(|(_, t)| *t)
    }
}

/// The filter setting keys that name a source (`FieldKind::Source`), by filter type.
/// The host answers from its filter catalogue.
pub type SourceFields<'a> = &'a dyn Fn(&NodeTypeId) -> Vec<String>;

impl SceneCollection {
    /// Duplicate source (ADR-0133 decision 3): copies `copies` and places them in the
    /// scene of `item`. `copies[0]` must be the item's source and is placed directly
    /// above `item` with the same layout, overrides, visibility, lock and timed show.
    /// Every further entry must be a companion of `copies[0].from`; it is placed in the
    /// same scene directly above its original's first item there, else directly above
    /// the first copy. A copied companion of a copied picture points at the picture
    /// copy; a companion copied alone loses `companion_of`. A scene in Own levels gives
    /// each copy its original's entry. Key links inside the set are remapped. One undo
    /// record.
    pub fn duplicate_source(
        &mut self,
        item: ItemId,
        copies: &[SourceCopy],
        source_fields: SourceFields<'_>,
    ) -> Result<(Duplicated, Undo), SceneError> {
        let (scene, original) = self.item(item).ok_or(SceneError::UnknownItem(item))?;
        let original = original.clone();
        let first = copies.first().ok_or(SceneError::UnknownItem(item))?;
        if original.content != ItemContent::Source(first.from) {
            return Err(SceneError::UnknownItem(item));
        }
        for c in &copies[1..] {
            if self.get_source(c.from)?.companion_of() != Some(first.from) {
                return Err(SceneError::UnknownSource(c.from));
            }
        }
        let mut next = self.next.clone();
        let (made, map) = self.copied_sources(copies, &mut next, source_fields)?;
        let mut draft = self.clone();
        draft.next = next;
        let mut inverses = Vec::new();
        let mut result = Duplicated {
            sources: copies
                .iter()
                .filter_map(|c| map.get(&c.from).map(|t| (c.from, *t)))
                .collect(),
            items: Vec::new(),
        };
        let own_levels = draft
            .get_scene(scene)?
            .audio
            .as_ref()
            .map(|a| a.levels.clone());
        for source in made {
            let levels = own_levels
                .as_ref()
                .and_then(|l| {
                    let from = map
                        .iter()
                        .find(|(_, t)| **t == source.id)
                        .map(|(f, _)| *f)?;
                    l.get(&from).map(|level| vec![(scene, *level)])
                })
                .unwrap_or_default();
            inverses.push(draft.perform(Op::InsertSource {
                source,
                placements: Vec::new(),
                levels,
            })?);
        }
        for (i, c) in copies.iter().enumerate() {
            let Some(&to) = map.get(&c.from) else {
                continue;
            };
            let s = draft.get_scene(scene)?;
            let (below, template) = if i == 0 {
                (Some(item), original.clone())
            } else {
                let own = s
                    .items
                    .iter()
                    .find(|it| it.content == ItemContent::Source(c.from))
                    .cloned();
                match own {
                    Some(it) => (Some(it.id), it),
                    None => {
                        let picture_copy = result.items.first().map(|(_, id)| *id);
                        let mut t = original.clone();
                        t.content = ItemContent::Source(c.from);
                        (picture_copy, t)
                    }
                }
            };
            let index = below
                .and_then(|b| s.items.iter().position(|it| it.id == b))
                .map_or(s.items.len(), |p| p + 1);
            let mut new = template;
            new.id = ItemId(issue(&mut draft.next.item)?);
            new.content = ItemContent::Source(to);
            result.items.push((to, new.id));
            inverses.push(draft.perform(Op::InsertItem {
                scene,
                index,
                item: new,
            })?);
        }
        draft.validate()?;
        inverses.reverse();
        *self = draft;
        Ok((result, Undo(Op::Batch(inverses))))
    }

    /// Duplicate scene with new sources (ADR-0133 decision 4): a copy of scene `id`
    /// named `name` whose items point at copies of their sources. `copies` lists every
    /// source to copy, each once (the host includes companions); items of sources not
    /// listed (global or not wanted) and nested scenes stay references. Own levels are
    /// re-keyed to the copies and key links inside the set are remapped. Refused for a
    /// canvas's global scene. One undo record.
    pub fn duplicate_scene_with_copies(
        &mut self,
        id: SceneId,
        name: &str,
        copies: &[SourceCopy],
        source_fields: SourceFields<'_>,
    ) -> Result<(SceneId, Duplicated, Undo), SceneError> {
        if self.is_global_scene(id) {
            return Err(SceneError::GlobalScene(id));
        }
        let original = self.get_scene(id)?.clone();
        let mut next = self.next.clone();
        let (made, map) = self.copied_sources(copies, &mut next, source_fields)?;
        let new_id = SceneId(issue(&mut next.scene)?);
        let mut items = Vec::with_capacity(original.items.len());
        let mut placed = Vec::new();
        for it in &original.items {
            let mut copy = it.clone();
            copy.id = ItemId(issue(&mut next.item)?);
            if let ItemContent::Source(s) = it.content
                && let Some(&to) = map.get(&s)
            {
                copy.content = ItemContent::Source(to);
                placed.push((to, copy.id));
            }
            items.push(copy);
        }
        let audio = original.audio.clone().map(|mut a| {
            a.levels = a
                .levels
                .into_iter()
                .map(|(s, l)| (map.get(&s).copied().unwrap_or(s), l))
                .collect();
            a
        });
        let mut ops: Vec<Op> = made
            .into_iter()
            .map(|source| Op::InsertSource {
                source,
                placements: Vec::new(),
                levels: Vec::new(),
            })
            .collect();
        ops.push(Op::InsertScene {
            scene: Scene {
                id: new_id,
                name: name.trim().to_owned(),
                scope: original.scope,
                items,
                audio,
            },
            placements: Vec::new(),
            shown: Vec::new(),
            globals: Vec::new(),
        });
        let undo = self.apply_with(Op::Batch(ops), next)?;
        let sources = copies
            .iter()
            .filter_map(|c| map.get(&c.from).map(|t| (c.from, *t)))
            .collect();
        Ok((
            new_id,
            Duplicated {
                sources,
                items: placed,
            },
            undo,
        ))
    }

    /// The copies, with new source and filter ids from `next`, hints and mute as asked,
    /// `companion_of` remapped or dropped, and key links remapped inside the set.
    fn copied_sources(
        &self,
        copies: &[SourceCopy],
        next: &mut super::NextIds,
        source_fields: SourceFields<'_>,
    ) -> Result<(Vec<Source>, BTreeMap<SourceId, SourceId>), SceneError> {
        let mut map = BTreeMap::new();
        for c in copies {
            self.get_source(c.from)?;
            if map.contains_key(&c.from) {
                return Err(SceneError::DuplicateId(c.from.to_string()));
            }
            map.insert(c.from, SourceId(issue(&mut next.source)?));
        }
        let mut made = Vec::with_capacity(copies.len());
        for c in copies {
            let mut s = self.get_source(c.from)?.clone();
            s.id = map[&c.from];
            s.name = c.name.trim().to_owned();
            match s.companion_of().and_then(|p| map.get(&p)) {
                Some(p) => {
                    s.hints
                        .insert(COMPANION_OF_HINT.to_owned(), p.0.to_string());
                }
                None => {
                    s.hints.remove(COMPANION_OF_HINT);
                }
            }
            if c.standby {
                s.hints.insert(STANDBY_HINT.to_owned(), "1".to_owned());
            } else {
                s.hints.remove(STANDBY_HINT);
            }
            if let Some(m) = c.muted {
                s.audio.muted = m;
            }
            for f in &mut s.filters {
                f.id = FilterId(issue(&mut next.filter)?);
                for key in source_fields(&f.type_id) {
                    let mapped = match f.settings.get(&key) {
                        Some(SettingValue::Int(v)) => u32::try_from(*v)
                            .ok()
                            .and_then(|raw| map.get(&SourceId(raw))),
                        _ => None,
                    };
                    if let Some(to) = mapped {
                        f.settings.insert(key, SettingValue::Int(i64::from(to.0)));
                    }
                }
            }
            made.push(s);
        }
        Ok((made, map))
    }
}
