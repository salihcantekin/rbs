//! The source group requests (protocol 1.104, ADR-0182 decision 10): `group.create`,
//! `group.remove`, `group.rename` and `group.list`.

use serde::{Deserialize, Serialize};

/// `group.create` (protocol 1.104): groups items of one scene. Any group among them
/// merges into the new one with all its members; the members move next to the topmost
/// one (ADR-0182 decision 5). One undo step; answers [`GroupCreated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateGroup {
    /// The scene (`scene.list`).
    pub scene: u32,
    /// The items to group, at least two distinct ones of that scene, else
    /// `invalid_params`.
    pub items: Vec<u32>,
    /// The group's name, unique in the scene; absent: the first free "Group N".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `group.remove` (protocol 1.104): ungroups. The members stay where they are, without
/// the group. One undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRef {
    /// The scene.
    pub scene: u32,
    /// The group's id (`group.list`).
    pub group: u32,
}

/// `group.rename` (protocol 1.104). One undo step; `ok` when the name is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameGroup {
    /// The scene.
    pub scene: u32,
    /// The group's id.
    pub group: u32,
    /// The new name: not blank, unique in the scene.
    pub name: String,
}

/// `group.create`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCreated {
    /// The new group's id.
    pub group: u32,
    /// Its name.
    pub name: String,
}

/// One group in `group.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    /// The group's id.
    pub id: u32,
    /// Its name.
    pub name: String,
    /// Its members' item ids, top first (the order of `scene.sources`).
    pub items: Vec<u32>,
}

/// `group.list`'s answer (protocol 1.104).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupList {
    /// The scene.
    pub scene: u32,
    /// Its groups, in creation order; none for a scene without groups.
    pub groups: Vec<GroupInfo>,
}
