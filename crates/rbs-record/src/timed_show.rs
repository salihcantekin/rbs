//! Timed-show cues (ADR-0105): host session state, not the scene document.
//!
//! The map is `ItemId` to a deadline on the studio clock. While it is empty, nothing
//! is scheduled and nothing walks items (P13). While it is not, one wake is the
//! soonest deadline. Tests drive a [`TestClock`](rbs_core::time::TestClock); nothing
//! here sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_scene::model::ItemId;
use rbs_scene::{MAX_SHOW_FOR_MS, show_for_is_valid};

/// Item ids that a Show press is drawing until their deadline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShowCues {
    deadlines: BTreeMap<ItemId, Timestamp>,
}

impl ShowCues {
    /// No cues. No wake.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing is showing on a timer.
    pub fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }

    /// The items still inside their deadline. Empty when the map is empty.
    pub fn items(&self) -> BTreeSet<ItemId> {
        self.deadlines.keys().copied().collect()
    }

    /// Starts or restarts a cue. Returns false when Show does nothing: the item is
    /// visible, or `show_for_ms` is absent or out of range. A hidden item with a
    /// duration gets `now + show_for_ms`, replacing a cue that was already running.
    pub fn show(
        &mut self,
        item: ItemId,
        visible: bool,
        show_for_ms: Option<u32>,
        now: Timestamp,
    ) -> bool {
        if visible || !show_for_is_valid(show_for_ms) {
            return false;
        }
        let Some(ms) = show_for_ms else {
            return false;
        };
        if ms == 0 || ms > MAX_SHOW_FOR_MS {
            return false;
        }
        let deadline = now + Duration::from_millis(u64::from(ms));
        self.deadlines.insert(item, deadline);
        true
    }

    /// Captures unexpired deadlines under their held-copy item ids.
    ///
    /// Only ids in `items` are copied; later changes to the original cues do not
    /// affect the snapshot. Duplicate destination ids are rejected.
    pub fn snapshot_remapped(
        &self,
        items: &BTreeMap<ItemId, ItemId>,
        now: Timestamp,
    ) -> Result<Self, ItemId> {
        let mut destinations = BTreeSet::new();
        for &held in items.values() {
            if !destinations.insert(held) {
                return Err(held);
            }
        }
        let deadlines = items
            .iter()
            .filter_map(|(original, held)| {
                self.deadlines
                    .get(original)
                    .filter(|deadline| **deadline > now)
                    .map(|deadline| (*held, *deadline))
            })
            .collect();
        Ok(Self { deadlines })
    }

    /// Captures the currently drawn cue ids for a frozen outgoing snapshot.
    pub fn active_at(&self, now: Timestamp) -> BTreeSet<ItemId> {
        self.deadlines
            .iter()
            .filter(|(_, deadline)| **deadline > now)
            .map(|(item, _)| *item)
            .collect()
    }

    pub(crate) fn rebased(&self, old_now: Timestamp, new_now: Timestamp) -> Self {
        let deadlines = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| **deadline > old_now)
            .map(|(item, deadline)| (*item, new_now + deadline.saturating_since(old_now)))
            .collect();
        Self { deadlines }
    }

    /// Drops the cue for `item` (the eye was opened, or the item left the document).
    /// Returns whether a cue was running.
    pub fn drop_item(&mut self, item: ItemId) -> bool {
        self.deadlines.remove(&item).is_some()
    }

    /// Drops cues `keep` rejects. No work when the map is empty.
    pub fn retain(&mut self, mut keep: impl FnMut(ItemId) -> bool) {
        if self.deadlines.is_empty() {
            return;
        }
        self.deadlines.retain(|id, _| keep(*id));
    }

    /// Drops cues whose deadline is at or before `now`. Returns whether any ended.
    /// An empty map returns false without walking items.
    pub fn expire(&mut self, now: Timestamp) -> bool {
        if self.deadlines.is_empty() {
            return false;
        }
        let before = self.deadlines.len();
        self.deadlines.retain(|_, deadline| *deadline > now);
        self.deadlines.len() != before
    }

    /// Time until the soonest deadline. `None` when the map is empty (do not arm a
    /// wake). `Some(0)` when a deadline is already due.
    pub fn wait(&self, now: Timestamp) -> Option<Duration> {
        let due = self.deadlines.values().copied().min()?;
        if now >= due {
            Some(Duration::ZERO)
        } else {
            Some(due.saturating_since(now))
        }
    }

    /// When `item`'s cue ends. `None` when it has no cue.
    pub fn deadline(&self, item: ItemId) -> Option<Timestamp> {
        self.deadlines.get(&item).copied()
    }

    /// Milliseconds left before `item`'s cue ends. `None` when it has no cue.
    pub fn remaining_ms(&self, item: ItemId, now: Timestamp) -> Option<u32> {
        let deadline = *self.deadlines.get(&item)?;
        let left = if now >= deadline {
            0
        } else {
            u32::try_from(deadline.saturating_since(now).as_millis()).unwrap_or(u32::MAX)
        };
        Some(left)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use rbs_core::time::{Clock, TestClock, Timestamp};
    use rbs_scene::model::ItemId;

    use super::ShowCues;

    fn item(id: u32) -> ItemId {
        ItemId::from_raw(id)
    }

    #[test]
    fn held_snapshot_keeps_original_deadline_after_preview_cue_changes() {
        let clock = TestClock::new();
        let mut preview = ShowCues::new();
        let a = item(1);
        let b = item(2);
        let held_a = item(101);
        assert!(preview.show(a, false, Some(2_000), clock.now()));
        let mut held = preview
            .snapshot_remapped(&BTreeMap::from([(a, held_a)]), clock.now())
            .unwrap();

        clock.advance(Duration::from_millis(500));
        assert!(preview.show(a, false, Some(3_000), clock.now()));
        assert!(preview.show(b, false, Some(3_000), clock.now()));
        assert!(preview.drop_item(a));
        assert_eq!(held.remaining_ms(held_a, clock.now()), Some(1_500));
        clock.advance(Duration::from_millis(1_500));
        assert!(held.expire(clock.now()));
        assert!(held.is_empty());
        assert!(preview.active_at(clock.now()).contains(&b));
    }

    #[test]
    fn snapshots_exclude_expired_cues_and_frozen_draw_state_does_not_change() {
        let mut cues = ShowCues::new();
        let a = item(1);
        let b = item(2);
        assert!(cues.show(a, false, Some(1_000), Timestamp::ZERO));
        assert!(cues.show(b, false, Some(2_000), Timestamp::ZERO));
        let now = Timestamp::ZERO + Duration::from_secs(1);
        let frozen = cues.active_at(now);
        let held = cues
            .snapshot_remapped(&BTreeMap::from([(a, item(11)), (b, item(12))]), now)
            .unwrap();
        assert_eq!(frozen, BTreeSet::from([b]));
        assert_eq!(held.items(), BTreeSet::from([item(12)]));
        assert!(cues.drop_item(b));
        assert_eq!(frozen, BTreeSet::from([b]));
    }

    #[test]
    fn held_snapshot_refuses_duplicate_copy_ids() {
        let cues = ShowCues::new();
        let duplicate = item(11);
        assert_eq!(
            cues.snapshot_remapped(
                &BTreeMap::from([(item(1), duplicate), (item(2), duplicate)]),
                Timestamp::ZERO
            ),
            Err(duplicate)
        );
    }

    #[test]
    fn held_snapshot_retains_nested_item_ids_without_following_preview() {
        let nested = item(8);
        let mut preview = ShowCues::new();
        assert!(preview.show(nested, false, Some(1_000), Timestamp::ZERO));
        let held = preview
            .snapshot_remapped(&BTreeMap::from([(nested, nested)]), Timestamp::ZERO)
            .unwrap();
        assert!(preview.drop_item(nested));
        assert_eq!(held.active_at(Timestamp::ZERO), BTreeSet::from([nested]));
    }

    #[test]
    fn an_empty_map_schedules_nothing() {
        let cues = ShowCues::new();
        let now = Timestamp::ZERO;
        assert!(cues.is_empty());
        assert!(cues.wait(now).is_none());
        assert!(!cues.clone().expire(now));
        assert!(cues.items().is_empty());
    }

    #[test]
    fn show_on_a_visible_item_does_nothing_and_a_hidden_one_restarts() {
        let clock = TestClock::new();
        let mut cues = ShowCues::new();
        let logo = item(7);
        assert!(!cues.show(logo, true, Some(2_000), clock.now()));
        assert!(cues.is_empty());
        assert!(!cues.show(logo, false, None, clock.now()));
        assert!(!cues.show(logo, false, Some(0), clock.now()));
        assert!(cues.show(logo, false, Some(2_000), clock.now()));
        assert_eq!(cues.wait(clock.now()), Some(Duration::from_millis(2_000)));
        clock.advance(Duration::from_millis(500));
        assert!(cues.show(logo, false, Some(2_000), clock.now()));
        assert_eq!(cues.wait(clock.now()), Some(Duration::from_millis(2_000)));
        assert!(!cues.expire(clock.now()));
        clock.advance(Duration::from_millis(2_000));
        assert!(cues.expire(clock.now()));
        assert!(cues.is_empty());
        assert!(cues.wait(clock.now()).is_none());
    }

    #[test]
    fn opening_the_eye_drops_the_cue_and_the_soonest_deadline_is_the_only_wake() {
        let clock = TestClock::new();
        let mut cues = ShowCues::new();
        let a = item(1);
        let b = item(2);
        assert!(cues.show(a, false, Some(1_000), clock.now()));
        assert!(cues.show(b, false, Some(5_000), clock.now()));
        assert_eq!(cues.wait(clock.now()), Some(Duration::from_millis(1_000)));
        assert!(cues.drop_item(a));
        assert_eq!(cues.wait(clock.now()), Some(Duration::from_millis(5_000)));
        assert!(!cues.drop_item(a));
        clock.advance(Duration::from_millis(5_000));
        assert!(cues.expire(clock.now()));
        assert!(cues.wait(Timestamp::from_nanos(1)).is_none());
    }

    #[test]
    fn removing_an_item_drops_its_cue_and_a_move_keeps_it() {
        let mut cues = ShowCues::new();
        let logo = item(3);
        assert!(cues.show(logo, false, Some(3_000), Timestamp::ZERO));
        cues.retain(|id| id == logo);
        assert!(!cues.is_empty());
        cues.retain(|_| false);
        assert!(cues.is_empty());
        assert!(cues.wait(Timestamp::ZERO).is_none());
    }
}
