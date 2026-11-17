//! Remove from scene (ADR-0065 decision 7, ADR-0142): the picked item goes, and with
//! it the companion items its scene no longer needs, or the whole source at its last
//! placement. One undo record; the host labels it.

use std::collections::BTreeSet;

use super::{Op, SceneCollection, SceneError, Undo};
use crate::model::{ItemContent, ItemId, SceneId, SourceId};

/// What Remove from scene removed (ADR-0142 decision 6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    /// The picked item's source was deleted (its last placement).
    pub source_deleted: bool,
    /// Every source deleted: the picked one at its last placement, and companions.
    pub deleted_sources: Vec<SourceId>,
    /// Every item removed in any scene, the picked one first.
    pub removed_items: Vec<ItemId>,
}

impl SceneCollection {
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
