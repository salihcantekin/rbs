//! Control-thread operations on timed-show cues.

use std::collections::{BTreeMap, BTreeSet};

use rbs_scene::model::{CanvasId, ItemContent};

use super::{
    BindReport, Duration, Engine, GraphDescription, ItemId, RecordError, SceneBinder,
    SceneCollection, ShowCues, Studio, StudioSettings, Timestamp,
};

pub(super) fn initial_bind(
    binder: &mut SceneBinder,
    graph: &mut GraphDescription,
    collection: &SceneCollection,
    settings: &StudioSettings,
    engine: &Engine,
) -> Result<(Option<HeldShowCues>, BTreeSet<ItemId>, BindReport), RecordError> {
    let mut held_cues = settings.held_cues.clone();
    let now = engine.clock().now();
    if let Some(cues) = &mut held_cues {
        cues.expire(now);
    }
    let items = held_cues
        .as_ref()
        .map(|cues| cues.active_at(now))
        .unwrap_or_default();
    let report = if items.is_empty() {
        binder.sync(collection, graph)
    } else {
        binder.sync_cued(collection, graph, &items, &BTreeSet::new())
    }
    .map_err(RecordError::Bind)?;
    Ok((held_cues, items, report))
}

pub(super) fn outgoing_cues(
    collection: &SceneCollection,
    canvas: CanvasId,
    cues: &BTreeSet<ItemId>,
) -> BTreeSet<ItemId> {
    let Some(canvas) = collection.canvas(canvas) else {
        return BTreeSet::new();
    };
    let mut pending: Vec<_> = canvas.globals.into_iter().chain(canvas.program).collect();
    let mut seen = BTreeSet::new();
    let mut captured = BTreeSet::new();
    while let Some(scene) = pending.pop() {
        if !seen.insert(scene) {
            continue;
        }
        if let Some(scene) = collection.scene(scene) {
            for item in &scene.items {
                if cues.contains(&item.id) {
                    captured.insert(item.id);
                }
                if let ItemContent::Scene(nested) = item.content {
                    pending.push(nested);
                }
            }
        }
    }
    captured
}

/// Independent timed-show deadlines captured for a held program copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldShowCues(ShowCues);

impl HeldShowCues {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drop expired held cues and report whether any ended.
    pub fn expire(&mut self, now: Timestamp) -> bool {
        self.0.expire(now)
    }

    /// The held item ids whose captured deadlines have not passed.
    pub fn active_at(&self, now: Timestamp) -> BTreeSet<ItemId> {
        self.0.active_at(now)
    }

    /// Time until the next deadline, or `None` when no cues remain.
    pub fn wait(&self, now: Timestamp) -> Option<Duration> {
        self.0.wait(now)
    }

    /// Move unexpired deadlines between the old and new engine clocks, preserving
    /// their remaining time. Both timestamps must refer to the handover instant.
    #[must_use]
    pub fn rebased(&self, old_now: Timestamp, new_now: Timestamp) -> Self {
        Self(self.0.rebased(old_now, new_now))
    }
}

impl Studio {
    /// Starts or restarts a timed show. Does nothing when `visible` is true or
    /// `show_for_ms` is absent (ADR-0105). The deadline is `now + show_for_ms` on
    /// the studio clock. Returns whether a cue is now running for `item`.
    pub fn show_cue(&mut self, item: ItemId, visible: bool, show_for_ms: Option<u32>) -> bool {
        let now = self.engine.clock().now();
        self.cues.show(item, visible, show_for_ms, now)
    }

    /// Starts or restarts the cues of hidden items at one clock instant (group Show,
    /// ADR-0193). Each pair is an item and its duration; the caller has checked them.
    /// Held and outgoing captures are not touched. Returns how many cues started.
    pub fn show_cues(&mut self, items: &[(ItemId, u32)]) -> usize {
        let now = self.engine.clock().now();
        items
            .iter()
            .filter(|(item, ms)| self.cues.show(*item, false, Some(*ms), now))
            .count()
    }

    /// Capture held deadlines using the original-to-copy item mapping.
    /// Refuses a duplicate destination id.
    pub fn capture_held_cues(
        &self,
        items: &BTreeMap<ItemId, ItemId>,
    ) -> Result<HeldShowCues, ItemId> {
        self.cues
            .snapshot_remapped(items, self.engine.clock().now())
            .map(HeldShowCues)
    }

    /// Install a held program's captured deadlines and return the previous capture.
    /// The caller restores the previous capture if the corresponding update fails.
    pub fn replace_held_cues(&mut self, held: Option<HeldShowCues>) -> Option<HeldShowCues> {
        std::mem::replace(&mut self.held_cues, held)
    }

    /// The installed held deadlines, for a restart handover.
    pub fn held_cues(&self) -> Option<&HeldShowCues> {
        self.held_cues.as_ref()
    }

    pub(super) fn cued_items(&self) -> BTreeSet<ItemId> {
        let mut items = self.cues.items();
        if let Some(held) = &self.held_cues {
            items.extend(held.0.items());
        }
        items
    }

    /// Capture the current cue draw state for an outgoing transition.
    pub fn capture_outgoing_cues(&self) -> BTreeSet<ItemId> {
        let now = self.engine.clock().now();
        let mut items = self.cues.active_at(now);
        if let Some(held) = &self.held_cues {
            items.extend(held.active_at(now));
        }
        items
    }

    /// Drops the cue when the eye opens. The deadline must not hide the item later.
    pub fn drop_cue(&mut self, item: ItemId) -> bool {
        self.cues.drop_item(item)
    }

    /// Ends cues whose deadline has passed. False when the map is empty or every
    /// deadline is still ahead, without scanning the scene. When true, the caller
    /// must update the studio to rebind the changed cue visibility.
    pub fn expire_cues(&mut self, now: Timestamp) -> bool {
        let live = self.cues.expire(now);
        let held = self.held_cues.as_mut().is_some_and(|cues| cues.expire(now));
        live || held
    }

    /// How long the control thread may sleep before the soonest cue ends. `None`
    /// when no cue is running, so nothing is armed (P13).
    pub fn cue_wait(&self, now: Timestamp) -> Option<Duration> {
        self.cues
            .wait(now)
            .into_iter()
            .chain(self.held_cues.as_ref().and_then(|held| held.wait(now)))
            .min()
    }

    /// When `item`'s timed-show cue ends on the studio clock. `None` when it has none.
    pub fn cue_deadline(&self, item: ItemId) -> Option<Timestamp> {
        self.cues.deadline(item)
    }

    /// Milliseconds left on a timed-show cue for `item`. `None` when it has none.
    pub fn cue_remaining_ms(&self, item: ItemId, now: Timestamp) -> Option<u32> {
        self.cues.remaining_ms(item, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebasing_preserves_fractional_deadlines_and_drops_expired_cues() {
        let a = ItemId::from_raw(1);
        let b = ItemId::from_raw(2);
        let mut original = ShowCues::new();
        assert!(original.show(a, false, Some(1), Timestamp::from_nanos(10)));
        assert!(original.show(b, false, Some(2), Timestamp::from_nanos(10)));
        let original = HeldShowCues(original);
        let moved = original.rebased(Timestamp::from_nanos(1_000_010), Timestamp::from_nanos(50));
        assert_eq!(
            moved.active_at(Timestamp::from_nanos(50)),
            BTreeSet::from([b])
        );
        assert_eq!(
            moved.wait(Timestamp::from_nanos(51)),
            Some(Duration::from_nanos(999_999))
        );
        assert!(moved.active_at(Timestamp::from_nanos(1_000_050)).is_empty());
        assert_eq!(
            original.active_at(Timestamp::from_nanos(10)),
            BTreeSet::from([a, b])
        );
    }
}
