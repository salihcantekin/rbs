//! Scene transitions on a running studio (ADR-0128 decisions 5 to 12, spec
//! `docs/specs/recording.md`, "Transitions").
//!
//! A program change with a kind that draws the incoming scene whole (Fade) runs in
//! three steps. While it *waits*, the binder binds an incoming compositor for the
//! canvas (`Role::Incoming`) drawing the new program scene, and keeps the outgoing
//! scene's sources linked to the program compositor and heard; the program compositor
//! still draws the outgoing layers only. Once the incoming compositor has drawn every
//! layer (its ready counter), or after [`READY_WAIT`], it *runs*: the program compositor
//! draws the outgoing layers and, above them, the incoming canvas with a fade. When the
//! duration has passed it *ends* in two steps (decision 8): the program compositor gets
//! the incoming scene's own layers, and once it took them
//! ([`ParamSender::pending`](rbs_graph::runtime::param::ParamSender::pending) reads
//! false), [`Studio::settle_transitions`] binds without the outgoing scene, which
//! removes the incoming compositor and unlinks what only the outgoing scene used.
//!
//! Nothing of this exists while no transition runs (P13): no node, no layer, no check
//! per frame.

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_gpu::node::Fade;
use rbs_scene::binder::OutgoingScene;
use rbs_scene::{CollectionTransition, GlideEasing, TransitionKind};

use super::*;

/// The longest a transition waits for the incoming picture before it starts anyway
/// (ADR-0128 decision 7), on the master clock. The same bound covers the wait for
/// the program compositor to take the plain layers at the end (decision 8).
pub const READY_WAIT: Duration = Duration::from_millis(250);

/// What a transition of one canvas is doing, as the host and the control API report
/// it (ADR-0128 decision 11, topic `transition.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransitionState {
    /// Waiting for the incoming picture (at most [`READY_WAIT`]).
    Waiting,
    /// Drawing the blend.
    Running,
    /// The blend is over; the outgoing scene is unbound at the next settle.
    Ending,
}

/// Where a transition of one canvas is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the incoming picture since this time.
    Waiting(Timestamp),
    /// Drawing the blend since this time.
    Running(Timestamp),
    /// The plain layers were sent at this time; waiting for the compositor to take
    /// them before the outgoing scene is unbound.
    Ending(Timestamp),
    /// Ready to be unbound by [`Studio::settle_transitions`].
    Done,
}

/// The incoming compositor of one canvas, once adopted.
pub(super) struct IncomingView {
    compositor: NodeId,
    layers: LayerSender,
    bound: BoundCompositor,
    ready: Arc<AtomicU64>,
}

/// A transition running on one canvas.
pub(super) struct CanvasTransition {
    /// The program compositor's layers when the change was accepted.
    from: Vec<BoundLayer>,
    /// What the binder keeps of the outgoing scene.
    outgoing: OutgoingScene,
    duration: Duration,
    easing: GlideEasing,
    phase: Phase,
    incoming: Option<IncomingView>,
}

/// What a program change on a canvas runs, from the collection's transition: `None`
/// for a kind that needs no incoming picture (Cut, Move) or a duration of 0.
fn blend_of(transition: CollectionTransition) -> Option<(Duration, GlideEasing)> {
    match transition.kind {
        TransitionKind::Fade if transition.fade.duration_ms > 0 => Some((
            Duration::from_millis(u64::from(transition.fade.duration_ms)),
            transition.fade.easing,
        )),
        _ => None,
    }
}

fn easing(e: GlideEasing) -> rbs_gpu::node::Easing {
    match e {
        GlideEasing::Linear => rbs_gpu::node::Easing::Linear,
        _ => rbs_gpu::node::Easing::EaseInOut,
    }
}

impl Studio {
    /// Starts a transition on each canvas whose program scene changed in this update
    /// and whose collection transition draws the incoming picture. A change while one
    /// runs ends that one at once: its incoming scene is the outgoing scene now
    /// (decision 9). A canvas whose program did not change keeps its transition.
    pub(super) fn note_transitions(
        &mut self,
        collection: &SceneCollection,
        old_main: Option<SceneId>,
        old_extra: &BTreeMap<CanvasId, Option<SceneId>>,
        old_layers: &BTreeMap<CanvasId, Vec<BoundLayer>>,
    ) {
        let mut changed: Vec<CanvasId> = Vec::new();
        let new_main = program_scene(collection, self.settings.canvas);
        if old_main.is_some() && old_main != new_main {
            changed.push(self.settings.canvas);
        }
        for extra in &self.extras {
            let previous = old_extra.get(&extra.canvas).copied().flatten();
            if previous.is_some() && previous != program_scene(collection, extra.canvas) {
                changed.push(extra.canvas);
            }
        }
        if changed.is_empty() {
            return;
        }
        let blend = blend_of(collection.transition());
        let now = self.engine.clock().now();
        let heard: BTreeSet<SourceId> = self
            .bound_mixer
            .as_ref()
            .map(|m| m.inputs.iter().map(|i| i.source).collect())
            .unwrap_or_default();
        for canvas in changed {
            // A running one ends at once; its compositor is kept for the next one.
            let incoming = self.transitions.remove(&canvas).and_then(|t| t.incoming);
            let Some((duration, easing)) = blend else {
                continue;
            };
            let from = old_layers.get(&canvas).cloned().unwrap_or_default();
            let drawn: BTreeSet<SourceId> = from.iter().map(|l| l.source).collect();
            let heard = if canvas == self.settings.canvas {
                heard.clone()
            } else {
                BTreeSet::new()
            };
            if let Some(view) = &incoming {
                // A reused compositor: the counter must move past its value now.
                view.ready.store(0, Ordering::Release);
            }
            self.transitions.insert(
                canvas,
                CanvasTransition {
                    from,
                    outgoing: OutgoingScene::new(drawn, heard),
                    duration,
                    easing,
                    phase: Phase::Waiting(now),
                    incoming,
                },
            );
        }
    }

    /// The outgoing scenes the binder keeps while their transition has not ended.
    pub(super) fn outgoing_scenes(&self) -> BTreeMap<CanvasId, OutgoingScene> {
        self.transitions
            .iter()
            .filter(|(_, t)| t.phase != Phase::Done)
            .map(|(c, t)| (*c, t.outgoing.clone()))
            .collect()
    }

    /// Adopts each newly bound incoming compositor with a ready counter, keeps the
    /// bound layers of the others, and sends a retired one to `leaving`.
    pub(super) fn sync_incoming(
        &mut self,
        report: &BindReport,
        leaving: &mut Vec<Added>,
    ) -> Result<(), RecordError> {
        let canvases: Vec<CanvasId> = self.transitions.keys().copied().collect();
        for canvas in canvases {
            let bound = report.compositors.get(&(canvas, Role::Incoming)).cloned();
            let size = self.canvas_size(canvas).ok();
            let Some(t) = self.transitions.get_mut(&canvas) else {
                continue;
            };
            match (t.incoming.as_mut(), bound, size) {
                (Some(view), Some(bound), _) if view.compositor == bound.node => {
                    view.bound = bound;
                }
                (view, Some(bound), Some((width, height))) => {
                    if let Some(old) = view.map(|v| v.compositor) {
                        leaving.push(Added {
                            node: old,
                            partition: RENDER,
                            gpu: true,
                        });
                    }
                    let ready = Arc::new(AtomicU64::new(0));
                    let (compositor, layers) = CompositorNode::new(
                        CanvasDesc { width, height },
                        MAX_LAYERS as u32,
                        Vec::new(),
                    );
                    let compositor = compositor.with_ready(Arc::clone(&ready));
                    self.engine
                        .adopt::<GpuRuntime>(RENDER, bound.node, Box::new(compositor))?;
                    t.incoming = Some(IncomingView {
                        compositor: bound.node,
                        layers,
                        bound,
                        ready,
                    });
                }
                _ => {}
            }
        }
        // Incoming compositors the binder retired (their transition is done, or ended
        // by a change on a canvas that no longer transitions).
        for t in self.transitions.values_mut() {
            if t.incoming
                .as_ref()
                .is_some_and(|v| report.retired_compositors.contains(&v.compositor))
                && let Some(view) = t.incoming.take()
            {
                leaving.push(Added {
                    node: view.compositor,
                    partition: RENDER,
                    gpu: true,
                });
            }
        }
        self.transitions
            .retain(|_, t| !(t.phase == Phase::Done && t.incoming.is_none()));
        Ok(())
    }

    /// The program layers of `canvas` while its transition runs, or `None` to paint
    /// them as usual. Also sends the incoming compositor its layers.
    pub(super) fn transition_layers(
        &mut self,
        canvas: CanvasId,
        plan: &rbs_graph::description::SchedulePlan,
        program: &BoundCompositor,
    ) -> Option<Vec<rbs_gpu::node::NodeLayer>> {
        let size = self.canvas_size(canvas).ok()?;
        // Items of the canvas's global scene (ADR-0065): drawn once, above the blend
        // (decision 5), so an overlay does not double, dim or move.
        let globals: BTreeSet<ItemId> = self
            .collection
            .globals_of(canvas)
            .and_then(|g| self.collection.scene(g))
            .map(|s| s.items.iter().map(|i| i.id).collect())
            .unwrap_or_default();
        let is_global = |l: &BoundLayer| l.path.first().is_some_and(|i| globals.contains(i));
        let t = self.transitions.get_mut(&canvas)?;
        let running = matches!(t.phase, Phase::Running(_));
        if let Some(view) = &mut t.incoming {
            let mut incoming = view.bound.clone();
            if running {
                incoming.layers.retain(|l| !is_global(l));
            }
            let _ = view.layers.send(node_layers(&incoming, plan));
        }
        let mut outgoing = program.clone();
        outgoing.layers.clone_from(&t.from);
        match t.phase {
            Phase::Waiting(_) => Some(node_layers(&outgoing, plan)),
            Phase::Running(start) => {
                outgoing.layers.retain(|l| !is_global(l));
                let mut layers = node_layers(&outgoing, plan);
                if let Some(view) = &t.incoming {
                    let (w, h) = size;
                    let mut top = rbs_gpu::node::NodeLayer::new(
                        view.compositor,
                        0,
                        0.0,
                        0.0,
                        w as f32,
                        h as f32,
                    );
                    // Transparent areas are the picture over black, as encoded after
                    // the transition (decision 5).
                    top.opaque = true;
                    top.fade = Some(Fade {
                        from: 0.0,
                        start,
                        duration: t.duration,
                        easing: easing(t.easing),
                    });
                    layers.push(top);
                }
                // The globals as they are now, above everything.
                let mut above = program.clone();
                above.layers.retain(|l| is_global(l));
                layers.extend(node_layers(&above, plan));
                Some(layers)
            }
            Phase::Ending(_) | Phase::Done => None,
        }
    }

    /// Moves each transition on: starts the ones whose incoming picture is ready (or
    /// waited [`READY_WAIT`]), sends the plain layers of the ones whose duration has
    /// passed, and marks done the ones whose plain layers the compositor took. Returns
    /// true when one is done, so the host calls [`Studio::settle_transitions`].
    pub(super) fn step_transitions(&mut self) -> bool {
        if self.transitions.is_empty() {
            return false;
        }
        let now = self.engine.clock().now();
        let mut resend = false;
        let mut done = false;
        let layers_pending = self.layers.pending() && !self.layers.is_closed();
        for t in self.transitions.values_mut() {
            match t.phase {
                Phase::Waiting(since) => {
                    let ready = t
                        .incoming
                        .as_ref()
                        .is_some_and(|v| v.ready.load(Ordering::Acquire) != 0);
                    if ready || now.saturating_since(since) >= READY_WAIT {
                        t.phase = Phase::Running(now);
                        resend = true;
                    }
                }
                Phase::Running(start) => {
                    if now.saturating_since(start) >= t.duration {
                        t.phase = Phase::Ending(now);
                        resend = true;
                    }
                }
                Phase::Ending(sent) => {
                    if !layers_pending || now.saturating_since(sent) >= READY_WAIT {
                        t.phase = Phase::Done;
                        done = true;
                    }
                }
                Phase::Done => done = true,
            }
        }
        if resend {
            let plan = self.graph.plan();
            self.send_program_layers(&plan);
        }
        done
    }

    /// How long the control thread may sleep before a transition needs a step. `None`
    /// when none runs, so nothing is armed (P13).
    pub fn transition_wait(&self, now: Timestamp) -> Option<Duration> {
        self.transitions
            .values()
            .map(|t| match t.phase {
                Phase::Waiting(since) => {
                    // The ready counter is polled; look again within a frame or so.
                    let left = READY_WAIT.saturating_sub(now.saturating_since(since));
                    left.min(Duration::from_millis(5))
                }
                Phase::Running(start) => t.duration.saturating_sub(now.saturating_since(start)),
                Phase::Ending(_) => Duration::from_millis(5),
                Phase::Done => Duration::ZERO,
            })
            .min()
    }

    /// True while a transition runs on `canvas`.
    pub fn transition_running(&self, canvas: CanvasId) -> bool {
        self.transitions.contains_key(&canvas)
    }

    /// What the transition of `canvas` is doing, `None` when none runs.
    pub fn transition_state(&self, canvas: CanvasId) -> Option<TransitionState> {
        self.transitions.get(&canvas).map(|t| match t.phase {
            Phase::Waiting(_) => TransitionState::Waiting,
            Phase::Running(_) => TransitionState::Running,
            Phase::Ending(_) | Phase::Done => TransitionState::Ending,
        })
    }

    /// Unbinds the outgoing scene of every transition that is done (decision 8 step
    /// 2): one update without it removes the incoming compositor and unlinks what only
    /// the outgoing scene used. Does nothing while none is done.
    pub fn settle_transitions(&mut self, sources: &SourceTypes) {
        if self.transitions.values().any(|t| t.phase == Phase::Done) {
            let collection = self.collection.clone();
            let _ = self.update(&collection, sources);
        }
    }

    /// True once since the last call when a transition is done: call
    /// [`Studio::settle_transitions`].
    pub fn take_transition_done(&mut self) -> bool {
        std::mem::take(&mut self.transition_done)
    }

    /// Ends every transition at once (leaving studio mode, a collection switch,
    /// stopping): the next update binds without them.
    pub fn end_transitions(&mut self) {
        for t in self.transitions.values_mut() {
            t.phase = Phase::Done;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fade_with_a_duration_blends() {
        let mut t = CollectionTransition::default();
        assert_eq!(blend_of(t), None);
        t.kind = TransitionKind::Cut;
        assert_eq!(blend_of(t), None);
        t.kind = TransitionKind::Fade;
        assert_eq!(
            blend_of(t),
            Some((Duration::from_millis(300), GlideEasing::Linear))
        );
        t.fade.duration_ms = 0;
        assert_eq!(blend_of(t), None);
    }
}
