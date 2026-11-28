//! Guest sources in the studio (spec `guest-source.md`): adopting their two nodes,
//! keeping their control handles, and telling each whether a shown scene draws it.
//! The worker starts when the host connects a pairing ([`Studio::guest_control`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rbs_engine::Engine;
use rbs_scene::activity::Level;
use rbs_scene::model::SourceId;
use rbs_sources::guest::{GuestControl, GuestSource};

use super::adopt::{SoundTarget, adopt_with_sound};
use super::{Entry, Studio, Watch};
use crate::RecordError;

/// The control handles of the guest source instances.
#[derive(Default)]
pub(super) struct Guests {
    controls: BTreeMap<SourceId, Arc<GuestControl>>,
}

impl Guests {
    /// Adopts a guest's picture and sound nodes and keeps its handle. `level` is the
    /// binder's current level: shown at [`Level::Shown`] and above.
    pub(super) fn adopt(
        &mut self,
        engine: &mut Engine,
        entry: &mut Entry,
        g: GuestSource,
        level: Level,
        to: SoundTarget<'_>,
    ) -> Result<(), RecordError> {
        g.control.set_shown(level >= Level::Shown);
        self.controls.insert(entry.source, Arc::clone(&g.control));
        let halves = (Box::new(g.node) as _, Box::new(g.sound) as _);
        adopt_with_sound(engine, entry, halves, Watch::Video(g.stats), to)
    }

    /// Drops the handles of sources no longer bound; the worker stops with the last.
    pub(super) fn retain(&mut self, bound: &BTreeSet<SourceId>) {
        self.controls.retain(|source, _| bound.contains(source));
    }

    /// A level change from the binder: the source is shown or not (ADR-0139 decision
    /// 12). Nothing for a source that is not a guest.
    pub(super) fn set_level(&self, source: SourceId, level: Level) {
        if let Some(c) = self.controls.get(&source) {
            c.set_shown(level >= Level::Shown);
        }
    }
}

impl Studio {
    /// The control handle of a guest source instance, to start its worker on a
    /// connected pairing's receiver. `None` when `source` has no guest instance.
    pub fn guest_control(&self, source: SourceId) -> Option<Arc<GuestControl>> {
        self.guests.controls.get(&source).cloned()
    }
}
