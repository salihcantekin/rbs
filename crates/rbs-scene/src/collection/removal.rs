//! Remove from scene (ADR-0065 decision 7, ADR-0142): the picked item goes, and with
//! it the companion items its scene no longer needs, or the whole source at its last
//! placement. One undo record; the host labels it.

use std::collections::BTreeSet;

use super::{Op, SceneCollection, SceneError, Undo};
use crate::model::{GroupId, ItemContent, ItemId, SceneId, SourceId};

/// What Remove from scene removed (ADR-0142 decision 6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    /// The picked item's source was deleted (its last placement). For a group,
    /// true if any explicitly selected source was deleted.
    pub source_deleted: bool,
    /// Every source deleted: the picked one at its last placement, and companions.
    pub deleted_sources: Vec<SourceId>,
    /// Every item removed in any scene. For a single-item removal the picked one is
    /// first; a group removal returns them in deterministic id order.
    pub removed_items: Vec<ItemId>,
}

impl SceneCollection {
    /// Remove exactly the expected members of a group and their dependent companions
    /// as one validated edit. Membership is compared as a set; duplicate, empty, or
    /// stale membership is refused without mutation. The result lists every removed
    /// placement and deleted source, each once, for authorization and cleanup.
    pub fn remove_group_members(
        &mut self,
        scene: SceneId,
        group: GroupId,
        expected_members: &[ItemId],
    ) -> Result<(Removed, Undo), SceneError> {
        let current = self.get_scene(scene)?;
        if !current.groups.iter().any(|g| g.id == group) {
            return Err(SceneError::UnknownGroup(group));
        }
        let expected: BTreeSet<_> = expected_members.iter().copied().collect();
        let actual: BTreeSet<_> = current
            .items
            .iter()
            .filter(|i| i.group == Some(group))
            .map(|i| i.id)
            .collect();
        if expected.is_empty() || expected.len() != expected_members.len() || expected != actual {
            return Err(SceneError::UndoConflict);
        }
        let selected_sources: BTreeSet<_> = current
            .items
            .iter()
            .filter(|i| actual.contains(&i.id))
            .filter_map(|i| match i.content {
                ItemContent::Source(id) => Some(id),
                _ => None,
            })
            .collect();
        let mut items = actual;
        let mut deleted = BTreeSet::new();
        self.expand_group_removal(scene, &mut items, &mut deleted);
        let mut ops = Vec::new();
        if self
            .transition
            .stinger
            .source
            .is_some_and(|id| deleted.contains(&id))
        {
            let mut new = self.transition;
            new.stinger.source = None;
            ops.push(Op::SetTransition {
                expected: self.transition,
                new,
            });
        }
        for id in &items {
            let (_, item) = self.item(*id).ok_or(SceneError::UnknownItem(*id))?;
            if !matches!(item.content, ItemContent::Source(source) if deleted.contains(&source)) {
                ops.push(Op::RemoveItem(*id));
            }
        }
        ops.extend(deleted.iter().copied().map(Op::RemoveSource));
        let removed = Removed {
            source_deleted: !selected_sources.is_disjoint(&deleted),
            deleted_sources: deleted.into_iter().collect(),
            removed_items: items.into_iter().collect(),
        };
        let undo = self.apply(Op::Batch(ops))?;
        Ok((removed, undo))
    }

    fn expand_group_removal(
        &self,
        scene: SceneId,
        items: &mut BTreeSet<ItemId>,
        deleted: &mut BTreeSet<SourceId>,
    ) {
        loop {
            let before = (items.len(), deleted.len());
            let mut sources = BTreeSet::new();
            let mut removed_here = BTreeSet::new();
            for id in items.iter().copied() {
                if let Some((owner, item)) = self.item(id)
                    && let ItemContent::Source(source) = item.content
                {
                    sources.insert(source);
                    if owner == scene {
                        removed_here.insert(source);
                    }
                }
            }
            for source in sources {
                if self
                    .placements_of(source)
                    .iter()
                    .all(|(_, id)| items.contains(id))
                {
                    deleted.insert(source);
                }
            }
            for source in self.sources.values() {
                if let Some(picture) = source.companion_of() {
                    if deleted.contains(&picture) {
                        deleted.insert(source.id);
                    } else if removed_here.contains(&picture)
                        && !self.draws_without(scene, picture, items)
                    {
                        items.extend(
                            self.placements_of(source.id)
                                .into_iter()
                                .filter(|(owner, _)| *owner == scene)
                                .map(|(_, item)| item),
                        );
                    }
                }
            }
            for source in deleted.iter().copied() {
                items.extend(self.placements_of(source).into_iter().map(|(_, id)| id));
            }
            if before == (items.len(), deleted.len()) {
                return;
            }
        }
    }

    /// Whether a scene still draws a picture after the complete removal set.
    fn draws_without(&self, scene: SceneId, source: SourceId, removed: &BTreeSet<ItemId>) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![scene];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(scene) = self.scene(id) else {
                continue;
            };
            for item in scene.items.iter().filter(|i| !removed.contains(&i.id)) {
                match item.content {
                    ItemContent::Source(id) if id == source => return true,
                    ItemContent::Scene(id) => stack.push(id),
                    _ => {}
                }
            }
        }
        false
    }

    /// Remove from scene (ADR-0142). The picked item goes. When it was its source's last
    /// placement in any scene, the source is deleted with its companions
    /// ([`SceneCollection::delete_source`]). Otherwise, when its scene no longer draws
    /// the source (no other item of it there, directly or in a nested scene), the
    /// companions' items placed directly in that scene go too, and a companion left
    /// placed nowhere is deleted. Other scenes never change. A nested scene's item is
    /// removed alone. One undo record.
    pub fn remove_from_scene(&mut self, item: ItemId) -> Result<(Removed, Undo), SceneError> {
        let (scene, it) = self.item(item).ok_or(SceneError::UnknownItem(item))?;
        let ItemContent::Source(source) = it.content else {
            let undo = self.apply(Op::RemoveItem(item))?;
            return Ok((
                Removed {
                    removed_items: vec![item],
                    ..Removed::default()
                },
                undo,
            ));
        };
        let companions: Vec<SourceId> = self
            .sources
            .values()
            .filter(|s| s.companion_of() == Some(source))
            .map(|s| s.id)
            .collect();
        let mut removed = Removed {
            removed_items: vec![item],
            ..Removed::default()
        };
        let mut ops = Vec::new();
        if self.placements_of(source).len() <= 1 {
            // The last placement: Delete source, companions first (as `delete_source`).
            for c in companions.iter().copied().chain([source]) {
                removed.removed_items.extend(
                    self.placements_of(c)
                        .into_iter()
                        .map(|(_, i)| i)
                        .filter(|i| *i != item),
                );
                removed.deleted_sources.push(c);
                ops.push(Op::RemoveSource(c));
            }
            removed.source_deleted = true;
        } else {
            ops.push(Op::RemoveItem(item));
            if !companions.is_empty() && !self.draws(scene, source, item) {
                for c in companions {
                    let placed = self.placements_of(c);
                    let here: Vec<ItemId> = placed
                        .iter()
                        .filter(|(s, _)| *s == scene)
                        .map(|(_, i)| *i)
                        .collect();
                    if here.is_empty() {
                        continue;
                    }
                    removed.removed_items.extend(&here);
                    if here.len() == placed.len() {
                        removed.deleted_sources.push(c);
                        ops.push(Op::RemoveSource(c));
                    } else {
                        ops.extend(here.into_iter().map(Op::RemoveItem));
                    }
                }
            }
        }
        let undo = self.apply(Op::Batch(ops))?;
        Ok((removed, undo))
    }

    /// Whether `scene` draws `source` without the item `except` (ADR-0142 "draws"): an
    /// item of it in the scene or in a scene nested in it at any depth, whatever the
    /// items' visibility.
    fn draws(&self, scene: SceneId, source: SourceId, except: ItemId) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![scene];
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            let Some(sc) = self.scene(s) else {
                continue;
            };
            for it in &sc.items {
                match it.content {
                    ItemContent::Source(id) if id == source && it.id != except => return true,
                    ItemContent::Scene(n) => stack.push(n),
                    ItemContent::Source(_) => {}
                }
            }
        }
        false
    }
}
