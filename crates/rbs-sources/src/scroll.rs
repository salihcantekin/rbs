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
    runs: BTreeMap<ScrollKey, Entry>,
}

/// One run and what the live calls set on it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    run: ScrollRun,
    /// The speed a resume eases back to: the key's speed, or the last live speed.
    speed: f64,
    /// Paused (eased to 0 by a live call).
    paused: bool,
    /// A `Once` run whose end was reported.
    finished: bool,
}

impl Entry {
    fn start(key: &ScrollKey, now: Timestamp) -> Self {
        let speed = f64::from(key.speed());
        Self {
            run: ScrollRun::start(now, speed),
            speed,
            paused: false,
            finished: false,
        }
    }
}

/// How long a live change of `key` eases.
fn ramp_of(key: &ScrollKey) -> Duration {
    Duration::from_millis(u64::from(key.ramp_ms()))
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
        self.runs.get(key).map(|e| &e.run)
    }

    /// The keys with a run.
    pub fn keys(&self) -> impl Iterator<Item = &ScrollKey> {
        self.runs.keys()
    }

    /// True when a live call paused the run of `key`.
    pub fn is_paused(&self, key: &ScrollKey) -> bool {
        self.runs.get(key).is_some_and(|e| e.paused)
    }

    /// Keeps exactly the runs of `keys`: a key without a run starts one at `now`, at
    /// its own speed, from its start; a run whose key is not listed ends (a key seen
    /// again later starts over). True when a run started or ended.
    pub fn keep(&mut self, keys: &BTreeSet<ScrollKey>, now: Timestamp) -> bool {
        self.keep_placed(keys, |_| false, now)
    }

    /// [`Self::keep`], except that a run with `on_return: continue` whose key is not
    /// listed goes on while `placed` says its source is still in a scene and no other
    /// key of its canvas and source is listed (ADR-0152 decision 6): drawn again, it
    /// is where it would be. A run of an edited scroll (a new key) thus ends once the
    /// new key is drawn.
    pub fn keep_placed(
        &mut self,
        keys: &BTreeSet<ScrollKey>,
        placed: impl Fn(model::SourceId) -> bool,
        now: Timestamp,
    ) -> bool {
        let before = self.runs.len();
        self.runs.retain(|k, _| {
            keys.contains(k)
                || (k.continues()
                    && placed(k.source)
                    && !keys
                        .iter()
                        .any(|o| o.canvas == k.canvas && o.source == k.source))
        });
        let mut changed = self.runs.len() != before;
        for key in keys {
            if !self.runs.contains_key(key) {
                self.runs.insert(*key, Entry::start(key, now));
                changed = true;
            }
        }
        changed
    }

    /// Every run moved to a new clock: `now_old` on the old clock is `now_new` on the
    /// new one (a studio restart, ADR-0150 decision 5).
    pub fn moved(&mut self, now_old: Timestamp, now_new: Timestamp) {
        for e in self.runs.values_mut() {
            e.run = e.run.moved(now_old, now_new);
        }
    }

    /// The run of `key` eased from its speed at `now` to `speed` (canvas pixels per
    /// second) over the key's ramp; a paused run keeps the speed for its resume. False
    /// when `key` has no run.
    pub fn change_speed(&mut self, key: &ScrollKey, now: Timestamp, speed: f64) -> bool {
        let ramp = ramp_of(key);
        match self.runs.get_mut(key) {
            Some(e) => {
                e.speed = speed;
                if !e.paused {
                    e.run = e.run.change_speed(now, speed, ramp);
                }
                true
            }
            None => false,
        }
    }

    /// The run of `key` eased to a stop at `now`. False when `key` has no run.
    pub fn pause(&mut self, key: &ScrollKey, now: Timestamp) -> bool {
        let ramp = ramp_of(key);
        match self.runs.get_mut(key) {
            Some(e) => {
                if !e.paused {
                    e.paused = true;
                    e.run = e.run.change_speed(now, 0.0, ramp);
                }
                true
            }
            None => false,
        }
    }

    /// The run of `key` eased back to its speed at `now`. False when `key` has no run.
    pub fn resume(&mut self, key: &ScrollKey, now: Timestamp) -> bool {
        let ramp = ramp_of(key);
        match self.runs.get_mut(key) {
            Some(e) => {
                if e.paused {
                    e.paused = false;
                    e.run = e.run.change_speed(now, e.speed, ramp);
                }
                true
            }
            None => false,
        }
    }

    /// The run of `key` started again from its start at `now`, at its own speed, not
    /// paused and not finished. False when `key` has no run.
    pub fn restart(&mut self, key: &ScrollKey, now: Timestamp) -> bool {
        match self.runs.get_mut(key) {
            Some(e) => {
                *e = Entry::start(key, now);
                true
            }
            None => false,
        }
    }

    /// Marks the run of `key` as finished; true the first time (its end is reported
    /// once per run).
    pub fn finish(&mut self, key: &ScrollKey) -> bool {
        match self.runs.get_mut(key) {
            Some(e) if !e.finished => {
                e.finished = true;
                true
            }
            _ => false,
        }
    }

    /// True when the end of the run of `key` was reported.
    pub fn is_finished(&self, key: &ScrollKey) -> bool {
        self.runs.get(key).is_some_and(|e| e.finished)
    }
}

/// How far a `Once` scroll moves before its picture has left a `window` long box,
/// for a `picture` long picture (both canvas pixels; ADR-0150, as the compositor ends
/// it): forward from inside, the picture; from outside, the window and the picture;
/// backward from inside, the window.
pub fn once_distance(scroll: &Scroll, window: f64, picture: f64) -> f64 {
    let forward = matches!(
        scroll.direction,
        model::ScrollDirection::Left | model::ScrollDirection::Up
    );
    match (scroll.start, forward) {
        (model::ScrollStart::Outside, _) => window + picture,
        (_, true) => picture,
        (_, false) => window,
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
    fn a_continuing_run_goes_on_while_its_source_is_placed() {
        // ADR-0152 decision 6, phase 5.
        use rbs_scene::activity::OnReturn;
        let going_on = |speed: f32| {
            ScrollKey::new(
                CanvasId::from_raw(0),
                SourceId::from_raw(1),
                &Scroll::new(ScrollDirection::Left, speed).with_on_return(OnReturn::Continue),
                100.0,
            )
        };
        let a = going_on(100.0);
        assert!(a.continues() && !key(1, 100.0).continues());
        let mut runs = ScrollRuns::new();
        let none = BTreeSet::new();
        runs.keep_placed(&BTreeSet::from([a]), |_| true, at(0));
        let run = *runs.get(&a).unwrap();
        // Off screen and placed: the run is kept, so drawn again it is where it would be.
        assert!(!runs.keep_placed(&none, |_| true, at(5000)));
        runs.keep_placed(&BTreeSet::from([a]), |_| true, at(9000));
        assert_eq!(runs.get(&a), Some(&run));
        // Another key of its source drawn (an edited scroll): the old run ends.
        let b = going_on(50.0);
        runs.keep_placed(&none, |_| true, at(9500));
        runs.keep_placed(&BTreeSet::from([b]), |_| true, at(10_000));
        assert!(runs.get(&a).is_none());
        // Not placed any more: it ends; a `restart` key ends as before.
        assert!(runs.keep_placed(&none, |_| false, at(11_000)));
        assert!(runs.is_empty());
        runs.keep_placed(&BTreeSet::from([key(1, 100.0)]), |_| true, at(12_000));
        assert!(runs.keep_placed(&none, |_| true, at(13_000)));
    }

    #[test]
    fn a_pause_is_continuous_and_a_restart_starts_over() {
        let a = key(1, 100.0);
        let mut runs = ScrollRuns::new();
        runs.keep(&BTreeSet::from([a]), at(0));
        let before = runs.get(&a).unwrap().position_at(at(2000));
        assert!(runs.pause(&a, at(2000)));
        assert!(runs.is_paused(&a));
        let run = *runs.get(&a).unwrap();
        assert_eq!(run.position_at(at(2000)), before);
        assert_eq!(run.speed_at(at(2000)), 100.0);
        assert_eq!(run.speed_at(at(2300)), 0.0);
        assert_eq!(run.position_at(at(9000)), run.position_at(at(2300)));
        assert!(runs.restart(&a, at(9000)));
        assert_eq!(runs.get(&a).unwrap().position_at(at(9000)), 0.0);
        assert!(!runs.change_speed(&key(9, 1.0), at(0), 1.0));
        assert!(!runs.restart(&key(9, 1.0), at(0)));
    }

    #[test]
    fn a_live_speed_set_while_paused_waits_for_the_resume() {
        let a = key(1, 100.0);
        let mut runs = ScrollRuns::new();
        runs.keep(&BTreeSet::from([a]), at(0));
        runs.pause(&a, at(1000));
        assert!(runs.change_speed(&a, at(2000), 40.0));
        assert_eq!(runs.get(&a).unwrap().speed_at(at(3000)), 0.0);
        assert!(runs.resume(&a, at(3000)));
        assert!(!runs.is_paused(&a));
        assert_eq!(runs.get(&a).unwrap().speed_at(at(3300)), 40.0);
        // A second resume changes nothing.
        let run = *runs.get(&a).unwrap();
        runs.resume(&a, at(4000));
        assert_eq!(runs.get(&a), Some(&run));
        // The end is reported once per run; a restart is a new run.
        assert!(runs.finish(&a));
        assert!(!runs.finish(&a));
        assert!(runs.is_finished(&a));
        runs.restart(&a, at(5000));
        assert!(!runs.is_finished(&a));
        assert_eq!(runs.get(&a).unwrap().to, 100.0);
    }

    #[test]
    fn a_once_scroll_ends_when_its_picture_has_left() {
        use super::once_distance;
        use rbs_scene::model::ScrollStart;
        let left = Scroll::new(ScrollDirection::Left, 10.0);
        let right = Scroll::new(ScrollDirection::Right, 10.0);
        assert_eq!(once_distance(&left, 100.0, 300.0), 300.0);
        assert_eq!(once_distance(&right, 100.0, 300.0), 100.0);
        let outside = left.with_start(ScrollStart::Outside);
        assert_eq!(once_distance(&outside, 100.0, 300.0), 400.0);
        let outside = right.with_start(ScrollStart::Outside);
        assert_eq!(once_distance(&outside, 100.0, 300.0), 400.0);
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
