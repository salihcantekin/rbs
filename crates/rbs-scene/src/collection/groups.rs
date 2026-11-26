//! Groups of items (ADR-0182): a tag on items of one scene, no layer of its own.
//!
//! A group's members are the scene's items whose `group` is its id, at least two and
//! contiguous in layer order. The edits here build ordinary records (`SetSceneItems`
//! and `SetGroups`), so grouping, ungrouping and a group dissolving after a removal
//! undo with the edit that caused them.

use std::collections::{BTreeMap, BTreeSet};

use super::{NextIds, Op, SceneCollection, SceneError, Undo, allowed_on, check_name, issue};
use crate::model::{CanvasId, Group, GroupId, GroupScroll, ItemId, Scene, SceneId, SceneItem};

/// Copies a scene's groups under fresh ids and points the copied `items` at them
/// (ADR-0182: a copy never shares a group with the original).
pub(super) fn copy_groups(
    next: &mut NextIds,
    groups: &[Group],
    items: &mut [SceneItem],
) -> Result<Vec<Group>, SceneError> {
    let mut map = BTreeMap::new();
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let id = GroupId(issue(&mut next.group)?);
        map.insert(g.id, id);
        out.push(Group {
            id,
            name: g.name.clone(),
            scrolls: g.scrolls.clone(),
        });
    }
    for i in items {
        i.group = i.group.and_then(|g| map.get(&g).copied());
    }
    Ok(out)
}

/// The first "Group N" name that is free in `scene`.
fn free_name(scene: &Scene) -> String {
    (1u32..)
        .map(|n| format!("Group {n}"))
        .find(|name| scene.groups.iter().all(|g| &g.name != name))
        .unwrap_or_else(|| "Group".to_owned())
}

/// `index` moved up to the end of the group block it falls inside, so an item inserted
/// there lands above the block instead of splitting it (ADR-0182 decision 4).
pub(super) fn above_block(items: &[SceneItem], mut index: usize) -> usize {
    while index > 0
        && index < items.len()
        && items[index - 1].group.is_some()
        && items[index - 1].group == items[index].group
    {
        index += 1;
    }
    index
}

/// Checks the group invariants of one scene (ADR-0182 decision 3).
pub(super) fn validate_scene(scene: &Scene) -> Result<(), SceneError> {
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for g in &scene.groups {
        check_name(&g.name)?;
        if !names.insert(g.name.as_str()) {
            return Err(SceneError::DuplicateName(g.name.clone()));
        }
        if !ids.insert(g.id) {
            return Err(SceneError::DuplicateId(g.id.to_string()));
        }
    }
    for item in &scene.items {
        if let Some(g) = item.group
            && !ids.contains(&g)
        {
            return Err(SceneError::UnknownGroup(g));
        }
    }
    for g in &scene.groups {
        let at: Vec<usize> = scene
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.group == Some(g.id))
            .map(|(n, _)| n)
            .collect();
        if at.len() < 2 {
            return Err(SceneError::GroupTooSmall(g.id));
        }
        if at[at.len() - 1] - at[0] + 1 != at.len() {
            return Err(SceneError::GroupNotContiguous(g.id));
        }
    }
    Ok(())
}

impl SceneCollection {
    pub(super) fn validate_group_scrolls(&self, scene: &Scene) -> Result<(), SceneError> {
        for group in &scene.groups {
            for (&canvas, scroll) in &group.scrolls {
                self.get_canvas(canvas)?;
                if !allowed_on(scene.scope, canvas) {
                    return Err(SceneError::ScopeViolation(scene.id));
                }
                if !scroll.is_valid() {
                    return Err(SceneError::InvalidLayout);
                }
            }
        }
        Ok(())
    }

    /// The groups of `scene`, in creation order.
    pub fn groups(&self, scene: SceneId) -> Result<&[Group], SceneError> {
        Ok(&self.get_scene(scene)?.groups)
    }

    /// Sets the timed-show duration of exactly the expected group members as one edit.
    /// Membership is compared as a set; duplicate or stale ids are refused.
    pub fn set_group_show_for(
        &mut self,
        scene: SceneId,
        group: GroupId,
        expected_members: &[ItemId],
        show_for_ms: Option<u32>,
    ) -> Result<Undo, SceneError> {
        let s = self.get_scene(scene)?;
        if !s.groups.iter().any(|g| g.id == group) {
            return Err(SceneError::UnknownGroup(group));
        }
        check_members(s, group, expected_members)?;
        if !crate::model::show_for_is_valid(show_for_ms) {
            return Err(SceneError::InvalidShowFor);
        }
        let old = s.items.clone();
        let mut new = old.clone();
        for item in &mut new {
            if item.group == Some(group) {
                item.show_for_ms = show_for_ms;
            }
        }
        self.apply(Op::SetSceneItems {
            scene,
            expected: old,
            new,
        })
    }

    /// Replaces the saved Scroll for one canvas in a single undoable edit.
    pub fn set_group_scroll(
        &mut self,
        scene: SceneId,
        group: GroupId,
        expected_members: &[ItemId],
        canvas: CanvasId,
        scroll: Option<GroupScroll>,
    ) -> Result<Undo, SceneError> {
        self.get_canvas(canvas)?;
        let s = self.get_scene(scene)?;
        if !super::allowed_on(s.scope, canvas) {
            return Err(SceneError::ScopeViolation(scene));
        }
        if scroll.is_some_and(|s| !s.is_valid()) {
            return Err(SceneError::InvalidLayout);
        }
        let expected = s.groups.clone();
        let mut new = expected.clone();
        let target = new
            .iter_mut()
            .find(|g| g.id == group)
            .ok_or(SceneError::UnknownGroup(group))?;
        check_members(s, group, expected_members)?;
        if let Some(scroll) = scroll {
            target.scrolls.insert(canvas, scroll);
        } else {
            target.scrolls.remove(&canvas);
        }
        let items = s.items.clone();
        self.apply(Op::Batch(vec![
            Op::SetSceneItems {
                scene,
                expected: items.clone(),
                new: items,
            },
            Op::SetGroups {
                scene,
                expected,
                new,
            },
        ]))
    }

    /// Groups ungrouped `items` of `scene` (ADR-0186), including uniquely placed
    /// companion sound items. The members move next to the topmost
    /// one, keeping their order; other items keep theirs. `name` defaults to the first
    /// free "Group N". `UnknownItem` for an item that does not exist, `GroupScope` for
    /// one in another scene, `GroupTooSmall` for fewer than two distinct items.
    pub fn group_items(
        &mut self,
        scene: SceneId,
        items: &[ItemId],
        name: Option<&str>,
    ) -> Result<(GroupId, Undo), SceneError> {
        let s = self.get_scene(scene)?.clone();
        if let Some(name) = name {
            check_name(name.trim())?;
        }
        let mut wanted = BTreeSet::new();
        for id in items {
            let owner = self
                .item_scene
                .get(id)
                .copied()
                .ok_or(SceneError::UnknownItem(*id))?;
            if owner != scene {
                return Err(SceneError::GroupScope);
            }
            wanted.insert(*id);
        }
        if wanted.len() < 2 {
            return Err(SceneError::GroupTooSmall(GroupId(0)));
        }
        let selected = wanted.clone();
        for item in s.items.iter().filter(|i| selected.contains(&i.id)) {
            let crate::model::ItemContent::Source(picture) = item.content else {
                continue;
            };
            let companions: Vec<_> = s
                .items
                .iter()
                .filter(|candidate| {
                    let crate::model::ItemContent::Source(source) = candidate.content else {
                        return false;
                    };
                    self.source(source)
                        .is_some_and(|source| source.companion_of() == Some(picture))
                })
                .collect();
            if !companions.is_empty() {
                let pictures = s
                    .items
                    .iter()
                    .filter(|i| i.content == crate::model::ItemContent::Source(picture))
                    .count();
                if pictures != 1 || companions.len() != 1 {
                    return Err(SceneError::AmbiguousCompanion);
                }
                wanted.insert(companions[0].id);
            }
        }
        if let Some(item) = s
            .items
            .iter()
            .find(|i| wanted.contains(&i.id) && i.group.is_some())
        {
            return Err(SceneError::ItemAlreadyGrouped(item.id));
        }
        let mut next = self.next.clone();
        let id = GroupId(super::issue(&mut next.group)?);
        let name = match name {
            Some(n) => n.trim().to_owned(),
            None => free_name(&s),
        };
        // Members go where the topmost one was; the others keep their places.
        let top = s
            .items
            .iter()
            .rposition(|i| wanted.contains(&i.id))
            .unwrap_or(0);
        let mut ordered: Vec<SceneItem> = Vec::with_capacity(s.items.len());
        let members: Vec<SceneItem> = s
            .items
            .iter()
            .filter(|i| wanted.contains(&i.id))
            .cloned()
            .map(|mut i| {
                i.group = Some(id);
                i
            })
            .collect();
        for (n, item) in s.items.iter().enumerate() {
            if wanted.contains(&item.id) {
                if n == top {
                    ordered.extend(members.iter().cloned());
                }
            } else {
                ordered.push(item.clone());
            }
        }
        let mut groups = s.groups.clone();
        groups.push(Group::new(id, &name));
        let ops = vec![
            Op::SetSceneItems {
                scene,
                expected: s.items.clone(),
                new: ordered,
            },
            Op::SetGroups {
                scene,
                expected: s.groups.clone(),
                new: groups,
            },
        ];
        let undo = self.apply_with(Op::Batch(ops), next)?;
        Ok((id, undo))
    }

    /// Ungroups: the members stay where they are without the tag (ADR-0182 decision 5).
    pub fn ungroup(&mut self, scene: SceneId, group: GroupId) -> Result<Undo, SceneError> {
        let s = self.get_scene(scene)?.clone();
        if !s.groups.iter().any(|g| g.id == group) {
            return Err(SceneError::UnknownGroup(group));
        }
        let new_items: Vec<SceneItem> = s
            .items
            .iter()
            .cloned()
            .map(|mut i| {
                if i.group == Some(group) {
                    i.group = None;
                }
                i
            })
            .collect();
        let new_groups: Vec<Group> = s.groups.iter().filter(|g| g.id != group).cloned().collect();
        self.apply(Op::Batch(vec![
            Op::SetSceneItems {
                scene,
                expected: s.items,
                new: new_items,
            },
            Op::SetGroups {
                scene,
                expected: s.groups,
                new: new_groups,
            },
        ]))
    }

    /// Renames a group (non-blank, unique in the scene).
    pub fn rename_group(
        &mut self,
        scene: SceneId,
        group: GroupId,
        name: &str,
    ) -> Result<Undo, SceneError> {
        let s = self.get_scene(scene)?;
        let expected = s.groups.clone();
        let mut new = expected.clone();
        new.iter_mut()
            .find(|g| g.id == group)
            .ok_or(SceneError::UnknownGroup(group))?
            .name = name.trim().to_owned();
        self.apply(Op::SetGroups {
            scene,
            expected,
            new,
        })
    }

    /// Dissolves groups left with fewer than two members after an edit (ADR-0182
    /// decision 4) and returns the records that undo that, to run before the edit's own
    /// inverse. Does nothing, and costs one scan of the scenes' group lists, when no
    /// scene has a group.
    pub(super) fn settle_groups(&mut self) -> Vec<Op> {
        let mut undo = Vec::new();
        for s in self.scenes.values_mut() {
            if s.groups.is_empty() {
                continue;
            }
            let small: BTreeSet<GroupId> = s
                .groups
                .iter()
                .filter(|g| s.items.iter().filter(|i| i.group == Some(g.id)).count() < 2)
                .map(|g| g.id)
                .collect();
            if small.is_empty() {
                continue;
            }
            let (old_items, old_groups) = (s.items.clone(), s.groups.clone());
            for i in &mut s.items {
                if i.group.is_some_and(|g| small.contains(&g)) {
                    i.group = None;
                }
            }
            s.groups.retain(|g| !small.contains(&g.id));
            undo.push(Op::SetSceneItems {
                scene: s.id,
                expected: s.items.clone(),
                new: old_items,
            });
            undo.push(Op::SetGroups {
                scene: s.id,
                expected: s.groups.clone(),
                new: old_groups,
            });
        }
        undo
    }

    pub(super) fn set_groups(
        &mut self,
        scene: SceneId,
        expected: Vec<Group>,
        new: Vec<Group>,
    ) -> Result<Op, SceneError> {
        let slot = self
            .scenes
            .get_mut(&scene)
            .ok_or(SceneError::UnknownScene(scene))?;
        if slot.groups != expected {
            return Err(SceneError::UndoConflict);
        }
        slot.groups = new.clone();
        Ok(Op::SetGroups {
            scene,
            expected: new,
            new: expected,
        })
    }
}

/// `UndoConflict` unless `expected` is, as a set without duplicates, exactly the members
/// of `group` in `scene`.
fn check_members(scene: &Scene, group: GroupId, expected: &[ItemId]) -> Result<(), SceneError> {
    let wanted: BTreeSet<_> = expected.iter().copied().collect();
    let actual: BTreeSet<_> = scene
        .items
        .iter()
        .filter(|i| i.group == Some(group))
        .map(|i| i.id)
        .collect();
    if wanted.len() != expected.len() || wanted != actual {
        return Err(SceneError::UndoConflict);
    }
    Ok(())
}
