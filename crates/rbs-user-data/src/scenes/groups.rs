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
use rbs_scene::model::{CanvasId, Group, GroupId, GroupScroll};
use serde::{Deserialize, Serialize};

use super::{
    DirectionName, RepeatName, ScrollDoc, ScrollReturnName, StartName, default_ramp_ms, is_default,
    is_default_ramp_ms, is_zero, later,
};
use crate::document::BodyError;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupDoc {
    id: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scrolls: Vec<GroupScrollEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupScrollEntry {
    canvas: u32,
    scroll: GroupScrollDoc,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupScrollDoc {
    #[serde(default, skip_serializing_if = "is_default")]
    direction: DirectionName,
    speed: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    gap: f64,
    #[serde(default, skip_serializing_if = "is_default")]
    repeat: RepeatName,
    #[serde(default, skip_serializing_if = "is_default")]
    start: StartName,
    #[serde(default, skip_serializing_if = "is_zero")]
    fade: f64,
    #[serde(
        default = "default_ramp_ms",
        skip_serializing_if = "is_default_ramp_ms"
    )]
    ramp_ms: u32,
}

impl GroupScrollDoc {
    fn of(scroll: GroupScroll) -> Self {
        let s = ScrollDoc::of(&scroll.as_scroll());
        Self {
            direction: s.direction,
            speed: s.speed,
            gap: s.gap,
            repeat: s.repeat,
            start: s.start,
            fade: s.fade,
            ramp_ms: s.ramp_ms,
        }
    }

    fn scroll(self) -> GroupScroll {
        GroupScroll::from_scroll(
            ScrollDoc {
                direction: self.direction,
                speed: self.speed,
                gap: self.gap,
                repeat: self.repeat,
                start: self.start,
                fade: self.fade,
                ramp_ms: self.ramp_ms,
                on_return: ScrollReturnName::Restart,
            }
            .scroll(),
        )
    }
}

/// A scene's groups for the document.
pub(super) fn group_docs(groups: &[Group]) -> Vec<GroupDoc> {
    groups
        .iter()
        .map(|g| GroupDoc {
            id: g.id.get(),
            name: g.name.clone(),
            scrolls: g
                .scrolls
                .iter()
                .map(|(canvas, scroll)| GroupScrollEntry {
                    canvas: canvas.get(),
                    scroll: GroupScrollDoc::of(*scroll),
                })
                .collect(),
        })
        .collect()
}

/// A scene's groups read back; the collection validates them.
pub(super) fn read(docs: Vec<GroupDoc>) -> Result<Vec<Group>, BodyError> {
    docs.into_iter()
        .map(|g| {
            let mut group = Group::new(GroupId::from_raw(g.id), &g.name);
            for entry in g.scrolls {
                if group
                    .scrolls
                    .insert(CanvasId::from_raw(entry.canvas), entry.scroll.scroll())
                    .is_some()
                {
                    return Err(BodyError::Value("duplicate group Scroll canvas".into()));
                }
            }
            Ok(group)
        })
        .collect()
}

pub(super) fn needs_31(col: &SceneCollection) -> bool {
    col.scenes()
        .any(|s| s.groups.iter().any(|g| !g.scrolls.is_empty()))
}

pub(super) fn refuse_before_31(from: u32, body: &toml::Table) -> Result<(), BodyError> {
    let has_scrolls = body
        .get("scenes")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("groups").and_then(toml::Value::as_array))
        .flatten()
        .any(|g| g.as_table().is_some_and(|g| g.contains_key("scrolls")));
    if from < 31 && has_scrolls {
        return Err(later(from, "group `scrolls`", 31));
    }
    Ok(())
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
pub(super) fn refuse_old_keys(from: u32, body: &toml::Table) -> Result<(), BodyError> {
    refuse_before_31(from, body)?;
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
