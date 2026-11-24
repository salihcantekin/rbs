//! The group row's context menu (ADR-0182): Visible, Locked, Rename group and Remove from
//! scene act on every member of the group the row stands for (`group` target).

use rbs_ui::command::{CommandId, Disabled};

use super::{Run, Shown};

/// What a group row's menu entry does to its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOp {
    /// Show (`true`) or hide every member.
    Visible(bool),
    /// Lock (`true`) or unlock every picture member.
    Locked(bool),
    /// Start the name field of the group's row.
    Rename,
    /// Remove every member from the scene, after the row's confirm.
    Remove,
}

/// Whether `base` is a command of the group row's menu.
pub(super) fn is_group(base: &str) -> bool {
    matches!(
        base,
        "ui.group.visible.toggle"
            | "ui.group.locked.toggle"
            | "ui.group.rename"
            | "ui.group.remove"
    )
}

fn group_of(id: &CommandId) -> Option<i32> {
    id.param("group")?.parse().ok()
}

impl Shown<'_> {
    /// Whether a group row command can run: the group is in the scene being edited.
    pub(super) fn group_enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        self.running()?;
        match group_of(id).and_then(|g| self.studio.group_state(g)) {
            Some(_) => Ok(()),
            None => Err("The group is not in this scene.".into()),
        }
    }

    /// Whether the group's members are shown (Visible) or locked (Locked) all together.
    pub(super) fn group_checked(&self, id: &CommandId) -> bool {
        let Some((hidden, locked)) = group_of(id).and_then(|g| self.studio.group_state(g)) else {
            return false;
        };
        match id.base() {
            "ui.group.visible.toggle" => !hidden,
            "ui.group.locked.toggle" => locked,
            _ => false,
        }
    }
}

/// What a group row command runs; `on` is its checked state now.
pub(super) fn run(id: &CommandId, on: bool) -> Option<Run> {
    let group = group_of(id)?;
    Some(Run::Group(
        group,
        match id.base() {
            "ui.group.visible.toggle" => GroupOp::Visible(!on),
            "ui.group.locked.toggle" => GroupOp::Locked(!on),
            "ui.group.rename" => GroupOp::Rename,
            "ui.group.remove" => GroupOp::Remove,
            _ => return None,
        },
    ))
}
