//! Scrolling layers on the studio side (ADR-0150 decisions 5 and 7): the scroll runs
//! every list shares, and each compositor's channels.

use std::collections::BTreeSet;
use std::time::Duration;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rbs_gpu::node::{CompositorNode, CompositorStats, LayerList, LayerSender, MAX_SPANS};
use rbs_gpu::scroll::{is_fill, picture_length};
use rbs_gpu::{GpuContext, ScrollBinding, ScrollSender};
use rbs_scene::ScrollKey;
use rbs_scene::activity::Level;
use rbs_scene::binder::BoundLayer;
use rbs_scene::model::{CanvasId, ItemId, ScrollDirection, ScrollRepeat};
use rbs_sources::scroll::{ScrollRuns, once_distance};

use std::collections::BTreeMap;

use rbs_scene::model::SourceId;

use super::{GroupFailureReason, GroupRuns, Studio, StudioSettings};
use rbs_core::time::Timestamp;

/// A live call on an item's scroll run (ADR-0150 decision 11). Not saved, not undone;
/// it acts on the run, so every item that shares it moves too.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollControl {
    /// Ease to a stop.
    Pause,
    /// Ease back to the run's speed.
    Resume,
    /// Start again from the start, at the saved speed.
    Restart,
    /// Ease to this speed, in the units of the saved speed (layout units per second,
    /// 1 to 4000); lasts as long as the run.
    Speed(f64),
}

/// Why a live call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRefused {
    /// The item has no scroll on that canvas.
    NoScroll,
    /// No list on that canvas draws the item, so it has no run.
    NoRun,
}

/// A `Once` run whose picture has left its box (ADR-0150 decision 11), published once
/// per run with every item that used it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollFinished {
    /// The canvas.
    pub canvas: CanvasId,
    /// The items, in id order.
    pub items: Vec<ItemId>,
}

/// A compositor's layer channel and scroll channel. The scroll binding (the wrap
/// pipelines of the device and a record buffer) is made and sent before the first list
/// with a scrolling layer, so a compositor that never scrolls has none (P13).
pub(crate) struct CompositorPort {
    layers: LayerSender,
    scroll: ScrollSender,
    bound: bool,
    stats: Arc<CompositorStats>,
    /// The tag of each span slot of the last list sent.
    sent: [Option<u64>; MAX_SPANS],
}

impl CompositorPort {
    /// A new compositor node (as [`CompositorNode::new`] returns it) with its scroll
    /// channel, and the port that feeds it.
    pub(crate) fn of_new((node, layers): (CompositorNode, LayerSender)) -> (CompositorNode, Self) {
        let stats = node.stats();
        let (node, scroll) = node.with_scroll();
        (
            node,
            Self {
                layers,
                scroll,
                bound: false,
                stats,
                sent: [None; MAX_SPANS],
            },
        )
    }

    /// Sends `list`, first binding scrolling on `gpu` when a layer or group scrolls
    /// and this compositor has no binding yet. Builds GPU objects then: control thread
    /// only.
    pub(crate) fn send_list(&mut self, list: LayerList, gpu: &GpuContext) {
        let scrolls = !list.spans.is_empty() || list.layers.iter().any(|l| l.scroll.is_some());
        if !self.bound && scrolls {
            let _ = self.scroll.send(Some(ScrollBinding::new(gpu)));
            self.bound = true;
        }
        self.sent = [None; MAX_SPANS];
        for (slot, span) in self.sent.iter_mut().zip(&list.spans) {
            *slot = Some(span.tag);
        }
        let _ = self.layers.send_list(list);
    }

    /// Why a span tagged `tag` in the last list sent fails now, if it does (ADR-0193
    /// decision 5): the node's state at its last draw. Unknown until it took that list.
    /// Relaxed loads: each word is a standalone status, read as a snapshot.
    pub(crate) fn span_failure(&self, tag: u64) -> Option<GroupFailureReason> {
        if self.pending() {
            return None;
        }
        let acquire = self.stats.span_acquire_failing.load(Ordering::Relaxed);
        let pass = self.stats.span_pass_failing.load(Ordering::Relaxed);
        failure_of(&self.sent, (acquire, pass), tag)
    }

    /// The last list sent has not been taken by the node yet.
    pub(crate) fn pending(&self) -> bool {
        self.layers.pending()
    }

    /// The node is gone.
    pub(crate) fn is_closed(&self) -> bool {
        self.layers.is_closed()
    }
}

/// Why the spans tagged `tag` among the slots `sent` fail, from the node's failing
/// bits (acquire, pass): a failed acquire first.
fn failure_of(
    sent: &[Option<u64>; MAX_SPANS],
    (acquire, pass): (u32, u32),
    tag: u64,
) -> Option<GroupFailureReason> {
    let slots = (0..MAX_SPANS).filter(|&s| sent[s] == Some(tag));
    let bits = slots.fold(0u32, |b, s| b | 1 << s);
    if acquire & bits != 0 {
        Some(GroupFailureReason::Allocation)
    } else if pass & bits != 0 {
        Some(GroupFailureReason::Render)
    } else {
        None
    }
}

/// Scroll runs handed from a studio to the one that replaces it (ADR-0150 decision 5).
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollHandover {
    /// The runs, on the old studio's clock.
    pub runs: ScrollRuns,
    /// The old clock's time when they were taken; the new studio's start time is the
    /// same instant on its clock, so the runs move by the difference (either sign).
    pub at: Timestamp,
    /// When each lingering source's linger ends, on the old clock; moved the same way
    /// (ADR-0152 decision 8).
    pub lingers: BTreeMap<SourceId, Timestamp>,
    /// The group Scroll runs, on the old clock, moved the same way (ADR-0185).
    pub groups: GroupRuns,
}

/// The runs a studio started with `settings` starts with: those of its handover, moved
/// to `now` on its clock (ADR-0150 decision 5, ADR-0185), or none; group runs with the
/// settings' allocators and held-copy mapping.
pub(super) fn handed_over(settings: &StudioSettings, now: Timestamp) -> (ScrollRuns, GroupRuns) {
    let (runs, groups) = match &settings.scroll_runs {
        None => (ScrollRuns::new(), GroupRuns::default()),
        Some(h) => {
            let mut runs = h.runs.clone();
            runs.moved(h.at, now);
            (runs, h.groups.moved(h.at, now))
        }
    };
    (
        runs,
        groups.starting(&settings.group_ids, &settings.scene_origins),
    )
}

/// The run keys of `layers`.
pub(crate) fn keys_of<'a>(
    layers: impl IntoIterator<Item = &'a BoundLayer>,
    out: &mut BTreeSet<ScrollKey>,
) {
    out.extend(layers.into_iter().filter_map(|l| l.scroll_key));
}

impl Studio {
    /// The scroll run of every key a list draws (ADR-0150 decision 5).
    pub fn scroll_runs(&self) -> &ScrollRuns {
        &self.scroll_runs
    }

    /// The scroll runs to hand to the studio that replaces this one
    /// ([`super::StudioSettings::scroll_runs`]), taken at the engine's time now.
    pub fn scroll_handover(&self) -> ScrollHandover {
        ScrollHandover {
            runs: self.scroll_runs.clone(),
            at: self.engine.clock().now(),
            lingers: self.binder.lingers(),
            groups: self.group_runs().clone(),
        }
    }

    /// Starts the runs of keys new to the lists and ends those no list draws any more
    /// (keeping a continuing one while its source is placed, ADR-0152 decision 6), at
    /// the engine's time now.
    pub(super) fn keep_scroll_runs(&mut self) {
        self.keep_group_runs();
        let keys = self.scroll_keys();
        if keys.is_empty() && self.scroll_runs.is_empty() {
            return;
        }
        let now = self.engine.clock().now();
        let binder = &self.binder;
        let placed = |s| binder.level_of(s).0 != Level::Unplaced;
        self.scroll_runs.keep_placed(&keys, placed, now);
    }

    /// Every layer of every list the studio sends (program and further canvases,
    /// preview, hold scenes, transitions).
    pub(super) fn all_layers(&self) -> Vec<&BoundLayer> {
        let mut out = self.live_layers();
        out.extend(self.transitions.values().flat_map(|t| &t.from));
        out
    }

    /// [`Self::all_layers`] without the transitions' outgoing snapshots.
    pub(super) fn live_layers(&self) -> Vec<&BoundLayer> {
        let mut out: Vec<&BoundLayer> = self.bound.layers.iter().collect();
        for extra in &self.extras {
            out.extend(&extra.view.bound.layers);
        }
        if let Some(sp) = &self.scene_preview {
            out.extend(&sp.bound.layers);
        }
        for run in self.hold_scenes.values() {
            out.extend(run.bound_layers());
        }
        out.extend(self.transition_incoming_layers());
        out
    }

    /// Whether a live pause holds the run of `item` on `canvas` (Properties shows
    /// Resume then). False when it has no run.
    pub fn scroll_paused(&self, item: ItemId, canvas: CanvasId) -> bool {
        self.all_layers()
            .into_iter()
            .filter(|l| l.path.last() == Some(&item))
            .filter_map(|l| l.scroll_key)
            .any(|k| k.canvas == canvas && self.scroll_runs.is_paused(&k))
    }

    /// A live call on the run of `item` on `canvas` (ADR-0150 decision 11); every list
    /// is sent again. `Ok` with the number of runs it acted on (an item placed twice in
    /// one picture may have two).
    pub fn scroll_control(
        &mut self,
        item: ItemId,
        canvas: CanvasId,
        control: ScrollControl,
    ) -> Result<usize, ScrollRefused> {
        let saved = self
            .collection
            .item(item)
            .and_then(|(_, i)| i.layout_on(canvas).scroll)
            .ok_or(ScrollRefused::NoScroll)?;
        let keys: BTreeSet<ScrollKey> = self
            .all_layers()
            .into_iter()
            .filter(|l| l.path.last() == Some(&item))
            .filter_map(|l| l.scroll_key)
            .filter(|k| k.canvas == canvas)
            .collect();
        let now = self.engine.clock().now();
        let mut acted = 0;
        for key in &keys {
            let done = match control {
                ScrollControl::Pause => self.scroll_runs.pause(key, now),
                ScrollControl::Resume => self.scroll_runs.resume(key, now),
                ScrollControl::Restart => self.scroll_runs.restart(key, now),
                ScrollControl::Speed(speed) => {
                    // The key's speed is the saved one scaled to canvas pixels.
                    let scale = f64::from(key.speed()) / f64::from(saved.speed);
                    self.scroll_runs.change_speed(key, now, speed * scale)
                }
            };
            acted += usize::from(done);
        }
        if acted == 0 {
            return Err(ScrollRefused::NoRun);
        }
        let plan = self.graph.plan();
        self.send_program_layers(&plan);
        Ok(acted)
    }

    /// Ends of `Once` runs since the last call, oldest first.
    pub fn take_scroll_finished(&mut self) -> Vec<ScrollFinished> {
        std::mem::take(&mut self.scroll_finished)
    }

    /// Item and group `Once` runs (ADR-0150 decision 11, ADR-0185): how long until the
    /// next may end. Nothing runs without a run.
    pub(super) fn service_scrolls(&mut self) -> Option<Duration> {
        let items = if self.scroll_runs.is_empty() {
            None
        } else {
            self.service_scroll()
        };
        match (items, self.service_group_scroll()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Reports `Once` runs whose picture has left, and returns how long until the next
    /// one may. Only called while runs exist.
    pub(super) fn service_scroll(&mut self) -> Option<Duration> {
        let now = self.engine.clock().now();
        let mut due: Option<Duration> = None;
        let mut ended: Vec<ScrollKey> = Vec::new();
        let layers = self.all_layers();
        for key in self.scroll_runs.keys() {
            if self.scroll_runs.is_finished(key) {
                continue;
            }
            let Some(layer) = layers.iter().find(|l| l.scroll_key.as_ref() == Some(key)) else {
                continue;
            };
            let Some(scroll) = layer.scroll.filter(|s| s.repeat == ScrollRepeat::Once) else {
                continue;
            };
            let Some(size) = self.picture_size(layer.source) else {
                continue;
            };
            let c = layer.crop;
            let pw = f64::from(size.0.saturating_sub(c.left + c.right));
            let ph = f64::from(size.1.saturating_sub(c.top + c.bottom));
            let t = layer.transform;
            let x_len = f64::from(t.a.hypot(t.b));
            let y_len = f64::from(t.c.hypot(t.d));
            let vertical = matches!(
                scroll.direction,
                ScrollDirection::Up | ScrollDirection::Down
            );
            let (window, cross, along, across) = if vertical {
                (y_len, x_len, ph, pw)
            } else {
                (x_len, y_len, pw, ph)
            };
            if across <= 0.0 {
                continue;
            }
            // As the compositor measures it, a fill as long as the box (ADR-0177).
            let picture = picture_length(window, cross, along, across, is_fill(size.0, size.1));
            let distance = once_distance(&scroll, window, picture);
            let Some(run) = self.scroll_runs.get(key) else {
                continue;
            };
            let at = run.position_at(now);
            if at >= distance {
                ended.push(*key);
                continue;
            }
            let wait = if now < run.ramp_end() {
                Some(run.ramp_end().saturating_since(now))
            } else if run.to > 0.0 {
                Some(Duration::from_secs_f64(
                    ((distance - at) / run.to).max(0.001),
                ))
            } else {
                None
            };
            if let Some(w) = wait {
                due = Some(due.map_or(w, |d| d.min(w)));
            }
        }
        let found: Vec<(ScrollKey, Vec<ItemId>)> = ended
            .into_iter()
            .map(|key| {
                let items: BTreeSet<ItemId> = layers
                    .iter()
                    .filter(|l| l.scroll_key == Some(key))
                    .filter_map(|l| l.path.last().copied())
                    .collect();
                (key, items.into_iter().collect())
            })
            .collect();
        for (key, items) in found {
            if self.scroll_runs.finish(&key) {
                self.scroll_finished.push(ScrollFinished {
                    canvas: key.canvas,
                    items,
                });
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_slot_names_its_own_span_and_reason() {
        // Slots 0 and 2 draw run 7 (a split span), slot 1 run 9.
        let sent = [Some(7), Some(9), Some(7), None];
        let r = |bits, tag| failure_of(&sent, bits, tag);
        assert_eq!(r((0b10, 0), 9), Some(GroupFailureReason::Allocation));
        assert_eq!(r((0b10, 0), 7), None, "another span's failure");
        assert_eq!(r((0, 0b100), 7), Some(GroupFailureReason::Render));
        assert_eq!(r((0b1, 0b100), 7), Some(GroupFailureReason::Allocation));
        assert_eq!(r((0, 0), 7), None, "recovered");
        assert_eq!(r((0b1000, 0b1000), 3), None, "an unsent slot");
    }
}
