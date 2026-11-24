//! Groups of items in the document (format 30, ADR-0182).
//!
//! ```toml
//! [[scenes.groups]]       # optional, in creation order
//! id = 1
//! name = "Camera set"
//! [[scenes.items]]
//! group = 1               # optional: the group the item belongs to
//! ```
//!
//! `next.group` holds the next group id and is written once a group was ever made.

use rbs_scene::SceneCollection;
use rbs_scene::model::{Group, GroupId};
use serde::{Deserialize, Serialize};

use super::later;
use crate::document::BodyError;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupDoc {
    id: u32,
    name: String,
}

/// A scene's groups for the document.
pub(super) fn group_docs(groups: &[Group]) -> Vec<GroupDoc> {
    groups
        .iter()
        .map(|g| GroupDoc {
            id: g.id.get(),
            name: g.name.clone(),
        })
        .collect()
}

/// A scene's groups read back; the collection validates them.
pub(super) fn read(docs: Vec<GroupDoc>) -> Vec<Group> {
    docs.into_iter()
        .map(|g| Group::new(GroupId::from_raw(g.id), &g.name))
        .collect()
}

/// A collection that has, or ever had, a group is written as format 30, so an older
/// build refuses it instead of dropping the groups.
pub(super) fn needs_30(col: &SceneCollection) -> bool {
    col.id_counters().group > 0
        || col
            .scenes()
            .any(|s| !s.groups.is_empty() || s.items.iter().any(|i| i.group.is_some()))
}

/// An older file that carries format 30 keys was not written by a build that knew them.
pub(super) fn refuse_before_30(from: u32, body: &toml::Table) -> Result<(), BodyError> {
    let scenes = || {
        body.get("scenes")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
    };
    let keys = scenes().any(|s| {
        s.contains_key("groups")
            || s.get("items")
                .and_then(toml::Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter_map(toml::Value::as_table)
                        .any(|i| i.contains_key("group"))
                })
    });
    let counter = body
        .get("next")
        .and_then(toml::Value::as_table)
        .is_some_and(|n| n.contains_key("group"));
    if from < 30 && (keys || counter) {
        return Err(later(
            from,
            "scene `groups`, item `group` or `next.group`",
            30,
        ));
    }
    Ok(())
}
