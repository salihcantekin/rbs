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
    use std::time::Duration;

    use rbs_core::time::{Clock, TestClock, Timestamp};
    use rbs_scene::model::ItemId;

    use super::ShowCues;

    fn item(id: u32) -> ItemId {
        ItemId::from_raw(id)
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
