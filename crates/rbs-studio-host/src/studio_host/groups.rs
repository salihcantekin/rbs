//! Group edits of the studio (ADR-0182): host steps over the scene collection's group
//! operations, each one undo step on the history.

use rbs_scene::model::{GroupId, ItemId, SceneId};

use super::{Host, Refusal};
use crate::edit_history::EditKind;

impl Host {
    pub(super) fn group_scene(&self, scene: SceneId) -> Result<(), Refusal> {
        if self.listed(scene) {
            Ok(())
        } else {
            Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                scene.get()
            )))
        }
    }

    pub(super) fn group_name(&self, scene: SceneId, group: GroupId) -> Result<String, Refusal> {
        self.collection
            .groups(scene)
            .map_err(|e| Refusal::of(&e))?
            .iter()
            .find(|g| g.id == group)
            .map(|g| g.name.clone())
            .ok_or_else(|| Refusal::NotFound(format!("There is no group {}.", group.get())))
    }

    /// Groups `items` of `scene` (one undo step); returns the group and its name.
    pub(super) fn group_items(
        &mut self,
        scene: SceneId,
        items: &[ItemId],
        name: Option<&str>,
    ) -> Result<(GroupId, String), Refusal> {
        self.group_scene(scene)?;
        if let Some(n) = name
            && n.trim().is_empty()
        {
            return Err(Refusal::Invalid("A group needs a name.".to_owned()));
        }
        let mut draft = self.collection.clone();
        let (id, undo) = draft
            .group_items(scene, items, name)
            .map_err(|e| Refusal::of(&e))?;
        let name = draft
            .groups(scene)
            .map_err(|e| Refusal::of(&e))?
            .iter()
            .find(|group| group.id == id)
            .map(|group| group.name.clone())
            .ok_or_else(|| Refusal::Failed("The new group is unavailable.".to_owned()))?;
        self.collection = draft;
        self.edits
            .history
            .record(EditKind::Group, &name, 0, vec![undo]);
        Ok((id, name))
    }

    /// Ungroups (one undo step): the members stay where they are.
    pub(super) fn ungroup(&mut self, scene: SceneId, group: GroupId) -> Result<(), Refusal> {
        self.group_scene(scene)?;
        let name = self.group_name(scene, group)?;
        let mut draft = self.collection.clone();
        let undo = draft.ungroup(scene, group).map_err(|e| Refusal::of(&e))?;
        self.collection = draft;
        self.edits
            .history
            .record(EditKind::Ungroup, &name, 0, vec![undo]);
        Ok(())
    }

    /// Renames a group (one undo step); `Ok(false)` when it has that name.
    pub(super) fn rename_group(
        &mut self,
        scene: SceneId,
        group: GroupId,
        name: &str,
    ) -> Result<bool, Refusal> {
        self.group_scene(scene)?;
        let old = self.group_name(scene, group)?;
        if old == name.trim() {
            return Ok(false);
        }
        let undo = self
            .collection
            .rename_group(scene, group, name)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::RenameGroup, &old, 0, vec![undo]);
        Ok(true)
    }
}
