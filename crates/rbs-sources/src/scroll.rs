//! Scroll runs on the control side (ADR-0150 decisions 3 to 5; spec
//! `docs/specs/scene-sources.md`, "Scrolling layers").
//!
//! The host keeps one [`ScrollRun`] per [`ScrollKey`]: a run starts when the first
//! layer list with its key is built and ends when no list contains it. Every
//! compositor drawing a key evaluates the same run, so program, preview, transitions,
//! holds and replay show the same position at the same tick.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::scroll::{LayerScroll, ScrollDirection, ScrollRepeat, ScrollRun, ScrollStart};
use rbs_scene::ScrollKey;
use rbs_scene::model::{self, Scroll};

/// The runs of every scrolling layer the host draws, by key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScrollRuns {
    runs: BTreeMap<ScrollKey, ScrollRun>,
}

impl ScrollRuns {
    /// No runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing scrolls.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// How many runs there are.
    pub fn len(&self) -> usize {
        self.runs.len()
    }

    /// The run of `key`.
    pub fn get(&self, key: &ScrollKey) -> Option<&ScrollRun> {
        self.runs.get(key)
    }

    /// The keys with a run.
    pub fn keys(&self) -> impl Iterator<Item = &ScrollKey> {
        self.runs.keys()
    }

    /// Keeps exactly the runs of `keys`: a key without a run starts one at `now`, at
    /// its own speed, from its start; a run whose key is not listed ends (a key seen
    /// again later starts over). True when a run started or ended.
    pub fn keep(&mut self, keys: &BTreeSet<ScrollKey>, now: Timestamp) -> bool {
        let before = self.runs.len();
        self.runs.retain(|k, _| keys.contains(k));
        let mut changed = self.runs.len() != before;
        for key in keys {
            if !self.runs.contains_key(key) {
                self.runs
                    .insert(*key, ScrollRun::start(now, f64::from(key.speed())));
                changed = true;
            }
        }
        changed
    }

    /// Every run moved to a new clock: `now_old` on the old clock is `now_new` on the
    /// new one (a studio restart, ADR-0150 decision 5).
    pub fn moved(&mut self, now_old: Timestamp, now_new: Timestamp) {
        for run in self.runs.values_mut() {
            *run = run.moved(now_old, now_new);
        }
    }

    /// The run of `key` eased from its speed at `now` to `speed` over `ramp`
    /// (pause is 0). False when `key` has no run.
    pub fn change_speed(
        &mut self,
        key: &ScrollKey,
        now: Timestamp,
        speed: f64,
        ramp: Duration,
    ) -> bool {
        match self.runs.get_mut(key) {
            Some(run) => {
                *run = run.change_speed(now, speed, ramp);
                true
            }
            None => false,
        }
    }

    /// The run of `key` started again from its start at `now`, at its own speed.
    /// False when `key` has no run.
    pub fn restart(&mut self, key: &ScrollKey, now: Timestamp) -> bool {
        match self.runs.get_mut(key) {
            Some(run) => {
                *run = ScrollRun::start(now, f64::from(key.speed()));
                true
            }
            None => false,
        }
    }
}

/// The node's scroll for a layer with `scroll` (in canvas pixels) moving by `run`.
pub fn layer_scroll(scroll: &Scroll, run: ScrollRun) -> LayerScroll {
    LayerScroll {
        direction: match scroll.direction {
            model::ScrollDirection::Right => ScrollDirection::Right,
            model::ScrollDirection::Up => ScrollDirection::Up,
            model::ScrollDirection::Down => ScrollDirection::Down,
            _ => ScrollDirection::Left,
        },
        repeat: match scroll.repeat {
            model::ScrollRepeat::Once => ScrollRepeat::Once,
            _ => ScrollRepeat::Loop,
        },
        start: match scroll.start {
            model::ScrollStart::Outside => ScrollStart::Outside,
            _ => ScrollStart::Inside,
        },
        gap: scroll.gap,
        fade: scroll.fade,
        run,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use rbs_core::time::Timestamp;
    use rbs_scene::ScrollKey;
    use rbs_scene::model::{CanvasId, Scroll, ScrollDirection, SourceId};

    use super::ScrollRuns;

    fn key(source: u32, speed: f32) -> ScrollKey {
        ScrollKey::new(
            CanvasId::from_raw(0),
            SourceId::from_raw(source),
            &Scroll::new(ScrollDirection::Left, speed),
            100.0,
        )
    }

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn runs_start_with_their_key_and_end_without_it() {
        let (a, b) = (key(1, 100.0), key(2, 50.0));
        let mut runs = ScrollRuns::new();
        assert!(runs.keep(&BTreeSet::from([a]), at(1000)));
        let first = *runs.get(&a).unwrap();
        assert_eq!(
            (first.anchor, first.to, first.fresh),
            (at(1000), 100.0, true)
        );
        // A later list with both keys keeps `a` as it was and starts `b`.
        assert!(runs.keep(&BTreeSet::from([a, b]), at(2000)));
        assert_eq!(runs.get(&a), Some(&first));
        assert_eq!(runs.get(&b).unwrap().anchor, at(2000));
        assert!(!runs.keep(&BTreeSet::from([a, b]), at(3000)));
        // `a` ends; seen again, it starts over.
        assert!(runs.keep(&BTreeSet::from([b]), at(4000)));
        assert_eq!(runs.len(), 1);
        assert!(runs.keep(&BTreeSet::from([a, b]), at(5000)));
        assert_eq!(runs.get(&a).unwrap().anchor, at(5000));
        assert!(runs.keep(&BTreeSet::new(), at(6000)));
        assert!(runs.is_empty());
    }

    #[test]
    fn a_pause_is_continuous_and_a_restart_starts_over() {
        let a = key(1, 100.0);
        let mut runs = ScrollRuns::new();
        runs.keep(&BTreeSet::from([a]), at(0));
        let before = runs.get(&a).unwrap().position_at(at(2000));
        assert!(runs.change_speed(&a, at(2000), 0.0, Duration::from_millis(300)));
        let run = *runs.get(&a).unwrap();
        assert_eq!(run.position_at(at(2000)), before);
        assert_eq!(run.speed_at(at(2000)), 100.0);
        assert_eq!(run.speed_at(at(2300)), 0.0);
        assert_eq!(run.position_at(at(9000)), run.position_at(at(2300)));
        assert!(runs.restart(&a, at(9000)));
        assert_eq!(runs.get(&a).unwrap().position_at(at(9000)), 0.0);
        assert!(!runs.change_speed(&key(9, 1.0), at(0), 1.0, Duration::ZERO));
        assert!(!runs.restart(&key(9, 1.0), at(0)));
    }

    #[test]
    fn runs_move_to_a_new_clock_either_way() {
        let a = key(1, 100.0);
        let mut runs = ScrollRuns::new();
        runs.keep(&BTreeSet::from([a]), at(10_000));
        let old = *runs.get(&a).unwrap();
        // The new clock is behind the old one: 12 s on the old clock is 2 s on the new.
        runs.moved(at(12_000), at(2000));
        let new = *runs.get(&a).unwrap();
        assert_eq!(new.position_at(at(2000)), old.position_at(at(12_000)));
        runs.moved(at(2000), at(50_000));
        let ahead = *runs.get(&a).unwrap();
        assert_eq!(ahead.position_at(at(50_000)), old.position_at(at(12_000)));
    }
}
