//! The group topics of protocol 1.108 (ADR-0184, ADR-0185, ADR-0193 decision 5):
//! `group.changed`, `group.render_state` and `group.scroll_finished`, published by
//! [`Host::publish`] on the control thread after each change. A key's object id is
//! issued by the host while the key lives; the payload names its scene, group and
//! canvas. Without a group Scroll, a subscriber or a cached key nothing is built.

use rbs_api::request::{
    GetGroup, GroupChanged, GroupRemoved, GroupRenderFailure, GroupRenderState,
    GroupScrollFinished, GroupState,
};
use std::collections::BTreeSet;

use rbs_api::types::OutputCanvas;
use rbs_scene::model::GroupId;

use super::super::*;
use super::apply::api_failures;

/// Scene, group and whether the canvas is the vertical one.
type Key = (SceneId, GroupId, bool);

fn on(vertical: bool) -> OutputCanvas {
    if vertical {
        OutputCanvas::Vertical
    } else {
        OutputCanvas::Main
    }
}

/// What the group topics last published.
#[derive(Debug, Default)]
pub(in crate::studio_host) struct GroupEvents {
    objects: BTreeMap<Key, u32>,
    next: u32,
    /// `group.changed`: each key's state without its countdown, and its members' cue
    /// deadlines (a restart moves one).
    changed: BTreeMap<Key, (GroupState, Vec<Option<Timestamp>>)>,
    /// `group.render_state`: the keys with failing occurrences.
    failing: BTreeMap<Key, Vec<GroupRenderFailure>>,
}

impl GroupEvents {
    fn object(&mut self, key: Key) -> Option<u32> {
        if let Some(id) = self.objects.get(&key) {
            return Some(*id);
        }
        // Exhausted after 2^32 keys in one run: the key is not published.
        self.next = self.next.checked_add(1)?;
        self.objects.insert(key, self.next);
        Some(self.next)
    }

    fn retire(&mut self, key: &Key) {
        if !self.changed.contains_key(key) && !self.failing.contains_key(key) {
            self.objects.remove(key);
        }
    }
}

fn state_key(topic: Topic, object: u32) -> StateKey {
    StateKey {
        topic,
        object: Some(object),
    }
}

impl Host {
    /// Publishes what changed on the group topics since the last call.
    pub(in crate::studio_host) fn publish_groups(&mut self) {
        for f in self.studio.take_group_scroll_finished() {
            self.publish_group_finished(f);
        }
        let watch = self.events.wanted(Topic::GroupChanged);
        let scrolls = self
            .collection
            .scenes()
            .any(|s| s.groups.iter().any(|g| !g.scrolls.is_empty()));
        if !watch && !scrolls && self.group_events.changed.is_empty() {
            if !self.group_events.failing.is_empty() {
                self.publish_render_states(&[]);
            }
            return;
        }
        let mut keys = Vec::new();
        let vertical = self.verticals.vertical.is_some();
        for s in self.collection.scenes() {
            for g in &s.groups {
                keys.push((s.id, g.id, false));
                if vertical {
                    keys.push((s.id, g.id, true));
                }
            }
        }
        self.publish_render_states(&keys);
        if watch {
            self.publish_changed(&keys);
        } else {
            let gone: Vec<Key> = std::mem::take(&mut self.group_events.changed)
                .into_keys()
                .collect();
            for key in &gone {
                self.group_events.retire(key);
            }
        }
    }

    fn publish_group_finished(&mut self, f: rbs_record::GroupScrollFinished) {
        let canvas = if f.canvas == self.canvas {
            OutputCanvas::Main
        } else if self.verticals.vertical.map(|v| v.canvas) == Some(f.canvas) {
            OutputCanvas::Vertical
        } else {
            return;
        };
        // One key per run, retired with it: a fresh object id each time.
        let Some(object) = self.group_events.next.checked_add(1) else {
            return;
        };
        self.group_events.next = object;
        let key = state_key(Topic::GroupScrollFinished, object);
        self.events.changed(key, || {
            EventPayload::GroupScrollFinished(GroupScrollFinished {
                scene: f.scene.get(),
                group: f.group.get(),
                canvas,
                run_id: f.run_id,
            })
        });
        self.events.forget(key);
    }

    /// `group.render_state` for the keys with a group Scroll on their canvas; a key
    /// that stops failing, or is gone, reports an empty array once and is retired.
    fn publish_render_states(&mut self, keys: &[Key]) {
        let mut now: BTreeMap<Key, Vec<GroupRenderFailure>> = BTreeMap::new();
        for &(scene, group, vertical) in keys {
            let canvas = if vertical {
                self.verticals.vertical.map(|v| v.canvas)
            } else {
                Some(self.canvas)
            };
            let Some(canvas) = canvas else { continue };
            let scrolls = self
                .collection
                .scene(scene)
                .and_then(|s| s.groups.iter().find(|g| g.id == group))
                .is_some_and(|g| g.scrolls.contains_key(&canvas));
            if !scrolls {
                continue;
            }
            let failures = api_failures(self.studio.group_render_failures(scene, group, canvas));
            if !failures.is_empty() {
                now.insert((scene, group, vertical), failures);
            }
        }
        let old = std::mem::take(&mut self.group_events.failing);
        let keys: BTreeSet<Key> = old.keys().chain(now.keys()).copied().collect();
        self.group_events.failing = now;
        for key in keys {
            let failures = self
                .group_events
                .failing
                .get(&key)
                .cloned()
                .unwrap_or_default();
            if old.get(&key).map_or(&[][..], Vec::as_slice) == failures.as_slice() {
                continue;
            }
            let (scene, group, canvas) = (key.0, key.1, on(key.2));
            if failures.is_empty() {
                rbs_core::warn!(
                    rbs_core::log::Area::Studio,
                    "group {} of scene {} draws again on the {canvas:?} canvas",
                    group.get(),
                    scene.get()
                );
            } else {
                rbs_core::warn!(
                    rbs_core::log::Area::Studio,
                    "group {} of scene {} is drawn transparent on the {canvas:?} canvas: {:?}",
                    group.get(),
                    scene.get(),
                    failures.iter().map(|f| f.reason).collect::<Vec<_>>()
                );
            }
            let Some(object) = self.group_events.object(key) else {
                continue;
            };
            let topic = state_key(Topic::GroupRenderState, object);
            self.events.changed(topic, || {
                EventPayload::GroupRenderState(GroupRenderState {
                    scene: scene.get(),
                    group: group.get(),
                    canvas,
                    failures,
                })
            });
            if !self.group_events.failing.contains_key(&key) {
                self.events.forget(topic);
                self.group_events.retire(&key);
            }
        }
    }

    /// `group.changed` for every key whose `group.get` answer changed, other than by
    /// its countdown; a key whose group, scene or canvas is gone reports `removed`.
    fn publish_changed(&mut self, keys: &[Key]) {
        let mut seen = BTreeSet::new();
        for &key in keys {
            let (scene, group, canvas) = (key.0, key.1, on(key.2));
            let get = GetGroup {
                scene: scene.get(),
                group: group.get(),
                canvas,
            };
            let state = match self.group_get(&get) {
                Ok(state) => state,
                // The vertical canvas shows another scene for now: unchanged.
                Err(Refusal::Conflict(_)) => {
                    seen.insert(key);
                    continue;
                }
                Err(_) => continue,
            };
            seen.insert(key);
            let deadlines: Vec<Option<Timestamp>> = state
                .members
                .iter()
                .map(|m| {
                    m.cued
                        .then(|| self.studio.cue_deadline(ItemId::from_raw(m.item)))
                        .flatten()
                })
                .collect();
            let mut quiet = state.clone();
            for m in &mut quiet.members {
                m.remaining_ms = None;
            }
            let entry = (quiet, deadlines);
            if self.group_events.changed.get(&key) == Some(&entry) {
                continue;
            }
            self.group_events.changed.insert(key, entry);
            let Some(object) = self.group_events.object(key) else {
                continue;
            };
            self.events
                .changed(state_key(Topic::GroupChanged, object), || {
                    EventPayload::GroupChanged(GroupChanged::State(state))
                });
        }
        let gone: Vec<Key> = self
            .group_events
            .changed
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .collect();
        for key in gone {
            self.group_events.changed.remove(&key);
            let (scene, group, canvas) = (key.0, key.1, on(key.2));
            if let Some(&object) = self.group_events.objects.get(&key) {
                let topic = state_key(Topic::GroupChanged, object);
                self.events.changed(topic, || {
                    EventPayload::GroupChanged(GroupChanged::Removed(GroupRemoved {
                        scene: scene.get(),
                        group: group.get(),
                        canvas,
                        removed: true,
                    }))
                });
                self.events.forget(topic);
            }
            self.group_events.retire(&key);
        }
    }
}
