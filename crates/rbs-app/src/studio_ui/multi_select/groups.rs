//! Groups in the Sources list and the Group and Ungroup commands (ADR-0182): the first
//! row of a group's block is marked as its header (arrow, name, combined Show/Hide and
//! Lock), the members sit under it while it is open. A click on the header selects the
//! whole block, a click on a member row that member alone (ADR-0194), and a press on a
//! member's picture the whole block. Runs when the rows are made again or on a click,
//! never per frame.
//! Nothing here costs anything while no scene has a group.

use rbs_api::Command as C;
use rbs_api::request as rq;
use slint::Model as _;

use super::Picked;
use crate::studio_ui::{SourceData, SourceRow, StudioUi};

impl Picked {
    /// Marks the group fields of the rows `data` (made from `rows`, in the same order);
    /// forgets open groups that are gone. The open groups are the set's second part:
    /// closed by default, in memory only (ADR-0182 decision 9).
    pub(in crate::studio_ui) fn mark_groups(
        &mut self,
        data: &mut [SourceData],
        rows: &[SourceRow],
    ) {
        let group = |r: &SourceRow| r.group.map(|g| g.get() as i32);
        self.1.retain(|g| rows.iter().any(|r| group(r) == Some(*g)));
        for i in 0..data.len().min(rows.len()) {
            let d = &mut data[i];
            let Some(g) = group(&rows[i]) else {
                d.grouped = false;
                d.group_head = false;
                continue;
            };
            let members = || rows.iter().filter(|r| group(r) == Some(g));
            d.grouped = true;
            d.group_id = g;
            d.group_head = i == 0 || group(&rows[i - 1]) != Some(g);
            d.group_open = self.1.contains(&g);
            d.group_name = rows[i].group_name.as_str().into();
            d.group_hidden = members().all(|r| !r.visible);
            d.group_locked = members().all(|r| r.locked);
        }
    }
}

impl StudioUi {
    /// The source ids of group `group`'s members, in list order.
    pub(in crate::studio_ui) fn group_members(&self, group: i32) -> Vec<i32> {
        self.rows
            .iter()
            .filter(|r| r.group.map(|g| g.get() as i32) == Some(group))
            .map(|r| r.id.get() as i32)
            .collect()
    }

    /// Whether group `group` is in the scene being edited, and whether all its members
    /// are hidden and all its pictures locked (what its row's eye and lock show).
    pub fn group_state(&self, group: i32) -> Option<(bool, bool)> {
        let row = self
            .rows
            .iter()
            .find(|r| r.group.map(|g| g.get() as i32) == Some(group))?;
        let members = || self.rows.iter().filter(|r| r.group == row.group);
        Some((members().all(|r| !r.visible), members().all(|r| r.locked)))
    }

    /// The group a source is in, if any.
    pub(in crate::studio_ui) fn group_of(&self, source: i32) -> Option<i32> {
        self.rows
            .iter()
            .find(|r| r.id.get() as i32 == source)
            .and_then(|r| r.group.map(|g| g.get() as i32))
    }

    /// Selects the whole block of `source`'s group (primary: `source`); a source in no
    /// group is a plain selection.
    pub(in crate::studio_ui) fn select_block(&mut self, source: i32) {
        self.selected = source;
        match self.group_of(source) {
            Some(g) => {
                // A selected member's group opens, so the selection is in the list.
                self.picked.1.insert(g);
                self.picked.set(self.group_members(g));
                self.remark_groups();
            }
            None => self.picked.clear(),
        }
        self.mark_picked();
    }

    /// Opens or closes the members of a group (its header's arrow).
    pub fn open_group(&mut self, group: i32, open: bool) {
        if open {
            self.picked.1.insert(group);
        } else {
            self.picked.1.remove(&group);
        }
        self.remark_groups();
    }

    /// A header was clicked: its whole group is selected.
    pub fn select_group(&mut self, group: i32) {
        if let Some(first) = self.group_members(group).first().copied() {
            self.select_block(first);
            self.refresh_marks();
        }
    }

    /// Renames a group (`group.rename`).
    pub fn rename_group(&mut self, group: i32, name: &str) {
        if let Some(scene) = self.scene_target(None) {
            self.call(C::GroupRename(rq::RenameGroup {
                scene: scene as u32,
                group: group as u32,
                name: name.to_owned(),
            }));
        }
    }

    /// Shows or hides every member of a group: one undo step.
    pub fn set_group_visible(&mut self, group: i32, visible: bool) {
        let gesture = group_gesture();
        for id in self.group_members(group) {
            self.set_source_visible_in(id, visible, gesture);
        }
    }

    /// Locks or unlocks every picture member of a group: one undo step.
    pub fn set_group_locked(&mut self, group: i32, locked: bool) {
        let gesture = group_gesture();
        for id in self.group_members(group) {
            if self
                .rows
                .iter()
                .any(|r| r.id.get() as i32 == id && r.layout.is_some())
            {
                self.set_source_locked_in(id, locked, gesture);
            }
        }
    }

    /// Removes every member of a group from the scene (Delete on its row): one undo
    /// step. A "Capture audio too" companion goes with its picture, so it is not asked
    /// for twice.
    pub fn remove_group(&mut self, group: i32) {
        let members = self.group_members(group);
        let gesture = group_gesture();
        self.deleted.clear();
        let items: Vec<u32> = self
            .rows
            .iter()
            .filter(|r| members.contains(&(r.id.get() as i32)))
            .filter(|r| {
                !r.companion_of
                    .is_some_and(|p| members.contains(&(p.get() as i32)))
            })
            .map(|r| r.item.get())
            .collect();
        for item in items {
            self.call(C::ItemRemove(rq::ItemRef { item, gesture }));
        }
    }

    /// The items Group would group: the selected sources (two or more) of the scene
    /// being edited, unless they already are exactly one group.
    pub fn group_items(&self) -> Result<Vec<u32>, &'static str> {
        let ids = self.picked.ids(self.selected);
        if ids.len() < 2 {
            return Err("Select two or more sources first (Ctrl or Cmd+click).");
        }
        let rows: Vec<&SourceRow> = ids
            .iter()
            .filter_map(|id| self.rows.iter().find(|r| r.id.get() as i32 == *id))
            .collect();
        if rows.iter().any(|r| r.global) {
            return Err("Global sources cannot be grouped.");
        }
        let first = rows.first().and_then(|r| r.group);
        if first.is_some() && rows.iter().all(|r| r.group == first) {
            let size = self.rows.iter().filter(|r| r.group == first).count();
            if size == rows.len() {
                return Err("These sources are a group already.");
            }
        }
        Ok(rows.iter().map(|r| r.item.get()).collect())
    }

    /// The group Ungroup would dissolve: the selected source's.
    pub fn ungroup_target(&self) -> Result<i32, &'static str> {
        self.group_of(self.selected)
            .ok_or("The selected source is not in a group.")
    }

    /// Group (`true`) or ungroup (`false`) the selected sources.
    pub fn group_command(&mut self, group: bool) {
        if group {
            self.group_selected();
        } else {
            self.ungroup_selected();
        }
    }

    /// Group (`group.create`).
    fn group_selected(&mut self) {
        if let (Ok(items), Some(scene)) = (self.group_items(), self.scene_target(None)) {
            self.call(C::GroupCreate(rq::CreateGroup {
                scene: scene as u32,
                items,
                name: None,
            }));
        }
    }

    /// Ungroup (`group.remove`).
    fn ungroup_selected(&mut self) {
        if let (Ok(group), Some(scene)) = (self.ungroup_target(), self.scene_target(None)) {
            self.call(C::GroupRemove(rq::GroupRef {
                scene: scene as u32,
                group: group as u32,
            }));
        }
    }

    fn remark_groups(&mut self) {
        let mut data: Vec<SourceData> = self.sources.iter().collect();
        self.picked.mark_groups(&mut data, &self.rows);
        crate::studio_ui::set_rows(&self.sources, data);
    }
}

/// A new window gesture for one group action, so its member edits are one undo step.
/// The numbers start above the ones the panels' sliders count up from.
fn group_gesture() -> u64 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static NEXT: AtomicI32 = AtomicI32::new(1 << 24);
    super::super::ui_gesture(NEXT.fetch_add(1, Ordering::Relaxed))
}
