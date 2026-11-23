//! Scrolling layers on the studio side (ADR-0150 decisions 5 and 7): the scroll runs
//! every list shares, and each compositor's channels.

use std::collections::BTreeSet;
use std::time::Duration;

use rbs_gpu::node::{CompositorNode, LayerSender, NodeLayer};
use rbs_gpu::scroll::{is_fill, picture_length};
use rbs_gpu::{GpuContext, ScrollBinding, ScrollSender};
use rbs_scene::ScrollKey;
use rbs_scene::activity::Level;
use rbs_scene::binder::BoundLayer;
use rbs_scene::model::{CanvasId, ItemId, ScrollDirection, ScrollRepeat};
use rbs_sources::scroll::{ScrollRuns, once_distance};

use super::{ScrollHandover, Studio};

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
}

impl CompositorPort {
    /// A new compositor node (as [`CompositorNode::new`] returns it) with its scroll
    /// channel, and the port that feeds it.
    pub(crate) fn of_new((node, layers): (CompositorNode, LayerSender)) -> (CompositorNode, Self) {
        let (node, scroll) = node.with_scroll();
        (
            node,
            Self {
                layers,
                scroll,
                bound: false,
            },
        )
    }

    /// Sends `layers`, first binding scrolling on `gpu` when one scrolls and this
    /// compositor has no binding yet. Builds GPU objects then: control thread only.
    pub(crate) fn send(&mut self, layers: Vec<NodeLayer>, gpu: &GpuContext) {
        if !self.bound && layers.iter().any(|l| l.scroll.is_some()) {
            let _ = self.scroll.send(Some(ScrollBinding::new(gpu)));
            self.bound = true;
        }
        let _ = self.layers.send(layers);
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
        }
    }

    /// Starts the runs of keys new to the lists and ends those no list draws any more
    /// (keeping a continuing one while its source is placed, ADR-0152 decision 6), at
    /// the engine's time now.
    pub(super) fn keep_scroll_runs(&mut self) {
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
    fn all_layers(&self) -> Vec<&BoundLayer> {
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
        out.extend(self.transition_layers_all());
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
