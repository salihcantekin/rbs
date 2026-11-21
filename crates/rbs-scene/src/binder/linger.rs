//! Linger (ADR-0152 decisions 3, 4 and 8): a source whose level drops below its
//! `run` stays Active for its `linger_s`, on the binder's injected clock.
//!
//! Evaluated on the control side in each sync, never per frame. `linger_s` 0 (every
//! type's default) adds nothing: no entry, no wake. A binder without a clock does not
//! linger.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rbs_core::time::Timestamp;

use crate::collection::SceneCollection;
use crate::model::SourceId;

use super::NodeTypes;

/// What the binder remembers between syncs for linger.
#[derive(Debug, Clone, Default)]
pub(super) struct Linger {
    /// The clock reading for the sync in progress; `None` without a clock.
    pub(super) now: Option<Timestamp>,
    /// The sources Active by level, policy or a forced cause at the last sync,
    /// lingering ones left out (so an ended linger does not start a new one).
    held: BTreeSet<SourceId>,
    /// Each lingering source and when its linger ends.
    until: BTreeMap<SourceId, Timestamp>,
}

impl Linger {
    /// Adds the lingering sources to `running` (the sources Active by level, policy
    /// or a forced cause) and returns them. A source starts lingering when it left
    /// `running` since the last sync and its policy has a linger. A linger ends when
    /// the source is Active again, its time is up, its linger is set to 0, or it is
    /// deactivated (Off) or deleted.
    pub(super) fn apply(
        &mut self,
        types: &dyn NodeTypes,
        collection: &SceneCollection,
        running: &mut BTreeSet<SourceId>,
    ) -> BTreeSet<SourceId> {
        let Some(now) = self.now else {
            self.until.clear();
            return BTreeSet::new();
        };
        let linger_of = |id: SourceId| {
            collection
                .source(id)
                .filter(|s| !s.standby())
                .map_or(0, |s| {
                    types
                        .activity(&s.type_id)
                        .effective(Some(&s.activity))
                        .linger_s
                })
        };
        for id in self.held.difference(running) {
            let secs = linger_of(*id);
            if secs > 0 && !self.until.contains_key(id) {
                self.until
                    .insert(*id, now + Duration::from_secs(u64::from(secs)));
            }
        }
        self.until
            .retain(|id, end| !running.contains(id) && *end > now && linger_of(*id) > 0);
        self.held.clone_from(running);
        let lingering: BTreeSet<SourceId> = self.until.keys().copied().collect();
        running.extend(lingering.iter().copied());
        lingering
    }

    /// When the soonest linger ends; `None` while nothing lingers.
    pub(super) fn next_end(&self) -> Option<Timestamp> {
        self.until.values().min().copied()
    }

    /// Whether `id` is lingering now.
    pub(super) fn contains(&self, id: SourceId) -> bool {
        self.until.contains_key(&id)
    }
}
