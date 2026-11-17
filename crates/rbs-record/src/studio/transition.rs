//! Scene transitions on a running studio (ADR-0128 decisions 5 to 12, spec
//! `docs/specs/recording.md`, "Transitions").
//!
//! A program change with a kind that draws the incoming scene whole (Fade, Swipe,
//! Slide) runs in
//! three steps. While it *waits*, the binder binds an incoming compositor for the
//! canvas (`Role::Incoming`) drawing the new program scene, and keeps the outgoing
//! scene's sources linked to the program compositor and heard; the program compositor
//! still draws the outgoing layers only. Once the incoming compositor has drawn every
//! layer (its ready counter), or after [`READY_WAIT`], it *runs*: the program compositor
//! draws the outgoing layers and, above them, the incoming canvas with a fade (Fade,
//! through an optional colour) or sliding in from an edge (Swipe; Slide also slides
//! the outgoing layers out). When the
//! duration has passed it *ends* in two steps (decision 8): the program compositor gets
//! the incoming scene's own layers, and once it took them
//! ([`ParamSender::pending`](rbs_graph::runtime::param::ParamSender::pending) reads
//! false), [`Studio::settle_transitions`] binds without the outgoing scene, which
//! removes the incoming compositor and unlinks what only the outgoing scene used.
//!
//! A Move whose outgoing scene draws sources the incoming one does not runs without
//! an incoming compositor: it starts at once, the program compositor draws the
//! incoming layers with their glide (ADR-0100) and, above them, the leaving layers
//! fading out over the glide's duration (decision 5); it ends the same two steps.
//!
//! Nothing of this exists while no transition runs (P13): no node, no layer, no check
//! per frame.

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_gpu::compositor::{LayerShape, LayerStyle};
use rbs_gpu::node::{Fade, Slide};
use rbs_scene::binder::OutgoingScene;
use rbs_scene::{CollectionTransition, GlideEasing, SwipeDirection, TransitionKind};

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

/// A program change's one-off kind and duration (ADR-0128 decision 13): absent parts
/// keep the collection's. With Move, a duration replaces the glide's and every
/// per-source override for that change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransitionOnce {
    /// The kind for this change.
    pub kind: Option<TransitionKind>,
    /// Milliseconds for this change, at most `MAX_GLIDE_MS`.
    pub duration_ms: Option<u32>,
}

impl TransitionOnce {
    /// A one-off kind and duration.
    pub fn new(kind: Option<TransitionKind>, duration_ms: Option<u32>) -> Self {
        Self { kind, duration_ms }
    }
}

/// Whether a transition started or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransitionPhase {
    /// The picture began to change.
    Started,
    /// The new scene is fully on air.
    Ended,
    /// A manual transition holds at [`TransitionEvent::progress`] (ADR-0135
    /// decision 6): the latest value, at most once per service step.
    Holding,
}

/// What a T-bar value did (ADR-0135 decisions 1 to 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManualStep {
    /// Nothing to show: `t` is 0 with no manual transition, the kind is Cut below
    /// the end, or a manual transition was cancelled.
    Idle,
    /// A manual transition waits for its picture or holds at `t`.
    Holding,
    /// `t` reached 1: the host makes the program change now, which ends the manual
    /// transition without a timed one.
    Take,
}

/// A transition's start or end, for the `transition.state` topic (ADR-0128 decision
/// 13). Reported once each, never per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct TransitionEvent {
    /// The canvas.
    pub canvas: CanvasId,
    /// Started or ended.
    pub phase: TransitionPhase,
    /// The kind that runs.
    pub kind: TransitionKind,
    /// Its length.
    pub duration: Duration,
    /// The scene that was on air.
    pub from: Option<SceneId>,
    /// The scene going on air.
    pub to: Option<SceneId>,
    /// Engine time of this state.
    pub at: Timestamp,
    /// Driven by hand (a T-bar, ADR-0135 decision 5).
    pub manual: bool,
    /// The progress a manual transition holds at (`Holding`).
    pub progress: Option<f32>,
}

/// The transition one update runs: the collection's, with a one-off kind and duration
/// over it.
pub(super) struct Effective {
    pub(super) transition: CollectionTransition,
    pub(super) glide: rbs_scene::CollectionGlide,
    /// False when a one-off duration replaces the per-source glide overrides.
    pub(super) source_glides: bool,
}

impl Effective {
    /// The kind that runs.
    pub(super) fn kind(&self) -> TransitionKind {
        self.transition.kind
    }

    /// True when the change glides (Move).
    pub(super) fn glides(&self) -> bool {
        self.kind() == TransitionKind::Move
    }
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
    /// What the blend draws (Fade, Swipe or Slide and their settings).
    blend: Blend,
    phase: Phase,
    incoming: Option<IncomingView>,
    /// The mixer's levels before the change (main canvas; decision 10).
    from_mix: BTreeMap<SourceId, SourceAudio>,
    /// The sources the new program scene is heard with (main canvas).
    to_heard: BTreeSet<SourceId>,
    /// The scenes it goes from and to, for its events.
    scenes: (Option<SceneId>, Option<SceneId>),
    /// Driven by hand (ADR-0135); `None` for a timed transition.
    manual: Option<Manual>,
    /// The stinger it plays (main canvas of a Stinger change, ADR-0140).
    pub(super) stinger: Option<stinger::StingerRun>,
}

impl CanvasTransition {
    /// True while it waits for or plays `source` as its stinger.
    pub(super) fn plays(&self, source: SourceId) -> bool {
        self.stinger.is_some_and(|r| r.source == source)
            && matches!(self.phase, Phase::Waiting(_) | Phase::Running(_))
    }

    /// Its stinger while it waits, with the time it started waiting.
    pub(super) fn stinger_waiting(&self) -> Option<(Timestamp, stinger::StingerRun)> {
        match (self.phase, self.stinger) {
            (Phase::Waiting(since), Some(run)) => Some((since, run)),
            _ => None,
        }
    }

    /// True while a Stinger change (the main canvas's or a follower's) waits.
    pub(super) fn stinger_waiting_any(&self) -> bool {
        matches!(self.blend, Blend::Stinger { .. }) && matches!(self.phase, Phase::Waiting(_))
    }

    /// True once the incoming compositor drew every layer.
    pub(super) fn incoming_ready(&self) -> bool {
        self.incoming
            .as_ref()
            .is_some_and(|v| v.ready.load(Ordering::Acquire) != 0)
    }

    /// The stinger's source while the binder keeps it.
    pub(super) fn stinger_source(&self) -> Option<SourceId> {
        self.outgoing.stinger
    }

    /// Ends a Stinger change at once without its stinger: the plain layers go out
    /// and the binder no longer keeps the source.
    pub(super) fn end_stinger(&mut self, now: Timestamp) {
        self.outgoing.stinger = None;
        if matches!(self.phase, Phase::Waiting(_) | Phase::Running(_)) {
            self.phase = Phase::Ending(now);
        }
    }
}

/// A manual transition's own state (ADR-0135 decisions 1 to 6).
pub(super) struct Manual {
    /// The latest applied progress.
    t: f32,
    /// The kind it shows (Move is drawn as a crossfade, decision 3).
    kind: TransitionKind,
    /// When it passed its ready gate (`started` was published).
    started: Option<Timestamp>,
    /// The sources the program mixer heard at its start.
    from_heard: BTreeSet<SourceId>,
    /// The levels the preview scene is heard with.
    to_mix: BTreeMap<SourceId, SourceAudio>,
    /// Cancelled (t back to 0, or ended without a program change): the program
    /// alone again until the settle.
    cancelled: bool,
    /// The program changed to its scene: it ends without a timed transition.
    taken: bool,
}

/// How a running transition shapes the main mixer's strips (ADR-0128 decision 10):
/// while it waits, the outgoing levels; while it runs, the incoming levels reached
/// over the picture's start and duration; after it, the incoming levels. A source of
/// the outgoing scene only goes to silence, one of the incoming scene only comes up
/// from silence, a shared one moves from its outgoing to its incoming level.
pub(super) struct MixShape {
    /// A manual transition's eased progress and the preview levels (ADR-0135
    /// decision 4); `None` for a timed one.
    manual: Option<(f32, BTreeMap<SourceId, SourceAudio>)>,
    phase: Phase,
    duration: Duration,
    outgoing: BTreeSet<SourceId>,
    from_mix: BTreeMap<SourceId, SourceAudio>,
    to_heard: BTreeSet<SourceId>,
    /// A Stinger change: its source, left as it is, and its cut point, where every
    /// other strip switches with the default ramp (ADR-0140 decision 6).
    stinger: Option<(Option<SourceId>, Duration)>,
}

impl MixShape {
    /// Shapes strips that hold the incoming (current) levels.
    pub(super) fn apply(&self, strips: &mut [MixStrip]) {
        let silent = |mut a: SourceAudio| {
            a.muted = true;
            a
        };
        for s in strips {
            match self.phase {
                Phase::Waiting(_) => {
                    s.audio = match self.from_mix.get(&s.source) {
                        Some(a) if self.outgoing.contains(&s.source) => *a,
                        _ => silent(s.audio),
                    };
                    s.ramp = None;
                }
                Phase::Running(_) if self.manual.is_some() => {
                    // Each strip at the lerp of its outgoing and incoming gain, with
                    // the mixer's default ramp, as a fader (decision 4).
                    let Some((w, to_mix)) = &self.manual else {
                        continue;
                    };
                    let gain = |a: Option<&SourceAudio>, heard: bool| match a {
                        Some(a) if heard && !a.muted => a.volume,
                        _ => 0.0,
                    };
                    let out = gain(
                        self.from_mix.get(&s.source),
                        self.outgoing.contains(&s.source),
                    );
                    let into = gain(to_mix.get(&s.source), self.to_heard.contains(&s.source));
                    let volume = out + (into - out) * w;
                    s.audio.volume = volume;
                    s.audio.muted = volume <= 0.0;
                    s.ramp = None;
                }
                Phase::Running(start) => {
                    if let Some((stinger, cut)) = self.stinger {
                        if stinger == Some(s.source) {
                            s.ramp = None;
                            continue;
                        }
                        if !self.to_heard.contains(&s.source) {
                            s.audio = silent(s.audio);
                        }
                        s.ramp = Some((start + cut, rbs_audio::DEFAULT_RAMP));
                        continue;
                    }
                    if !self.to_heard.contains(&s.source) {
                        s.audio = silent(s.audio);
                    }
                    s.ramp = Some((start, self.duration));
                }
                Phase::Ending(_) | Phase::Done => {
                    if !self.to_heard.contains(&s.source) {
                        s.audio = silent(s.audio);
                    }
                    s.ramp = None;
                }
            }
        }
    }
}

/// How a running transition draws the incoming canvas over the outgoing layers
/// (ADR-0128 decisions 2 and 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blend {
    /// A fade in, through an opaque sRGB colour when there is one.
    Fade { via: Option<[u8; 3]> },
    /// The incoming canvas slides in from the edge opposite `direction`; with `push`
    /// (Slide) the outgoing layers move out by the same offset.
    Swipe {
        direction: SwipeDirection,
        push: bool,
    },
    /// The program compositor draws the incoming layers (gliding) and, above them,
    /// the outgoing-only layers fading out. No incoming compositor.
    Move,
    /// A stinger plays over the change; the incoming canvas replaces the outgoing
    /// layers at `cut` under it (ADR-0140). A `follower` canvas switches at the main
    /// canvas's cut without a stinger picture (decision 7).
    Stinger { cut: Duration, follower: bool },
}

impl Blend {
    fn kind(self) -> TransitionKind {
        match self {
            Blend::Fade { .. } => TransitionKind::Fade,
            Blend::Swipe { push: false, .. } => TransitionKind::Swipe,
            Blend::Swipe { push: true, .. } => TransitionKind::Slide,
            Blend::Move => TransitionKind::Move,
            Blend::Stinger { .. } => TransitionKind::Stinger,
        }
    }
}

/// What a program change on a canvas runs, from the collection's transition: `None`
/// for Cut, Move (see [`move_of`]) or a duration of 0.
fn blend_of(transition: CollectionTransition) -> Option<(Duration, GlideEasing, Blend)> {
    let ms = |d: u32| Duration::from_millis(u64::from(d));
    match transition.kind {
        TransitionKind::Fade if transition.fade.duration_ms > 0 => Some((
            ms(transition.fade.duration_ms),
            transition.fade.easing,
            Blend::Fade {
                via: transition.fade.via,
            },
        )),
        TransitionKind::Swipe | TransitionKind::Slide => {
            let push = transition.kind == TransitionKind::Slide;
            let s = if push {
                transition.slide
            } else {
                transition.swipe
            };
            (s.duration_ms > 0).then(|| {
                (
                    ms(s.duration_ms),
                    s.easing,
                    Blend::Swipe {
                        direction: s.direction,
                        push,
                    },
                )
            })
        }
        _ => None,
    }
}

/// A Move's fade-out of the leaving layers: the glide's duration, when it has one.
fn move_of(effective: &Effective) -> Option<(Duration, GlideEasing, Blend)> {
    (effective.glides() && effective.glide.duration_ms > 0).then(|| {
        (
            Duration::from_millis(u64::from(effective.glide.duration_ms)),
            effective.glide.easing,
            Blend::Move,
        )
    })
}

/// What a rehearsal plays (ADR-0135 decision 11): the transition a
/// `canvas.transition` into `preview` would run now, with Move as a crossfade of the
/// glide's duration and easing. `None` for Cut and a duration of 0.
pub(super) fn rehearsal_plan(
    collection: &SceneCollection,
    preview: SceneId,
) -> Option<(Duration, GlideEasing, TransitionKind)> {
    let e = Studio::transition_into(collection, Some(preview), TransitionOnce::default());
    let kind = e.kind();
    if kind == TransitionKind::Move {
        return (e.glide.duration_ms > 0).then(|| {
            (
                Duration::from_millis(u64::from(e.glide.duration_ms)),
                e.glide.easing,
                kind,
            )
        });
    }
    blend_of(e.transition).map(|(d, ease, blend)| (d, ease, blend.kind()))
}

/// The rehearsal compositor's layers (ADR-0135 decision 12): the program picture,
/// and over it the preview picture coming in as the timed transition would bring it,
/// from `start` for `duration`, in the rehearsal's own `size`. Move is a crossfade.
pub(super) fn rehearsal_layers(
    collection: &SceneCollection,
    preview: SceneId,
    (program, incoming): (NodeId, NodeId),
    size: (u32, u32),
    start: Timestamp,
) -> Vec<rbs_gpu::node::NodeLayer> {
    let e = Studio::transition_into(collection, Some(preview), TransitionOnce::default());
    let plan = blend_of(e.transition).or_else(|| {
        (e.kind() == TransitionKind::Move && e.glide.duration_ms > 0).then(|| {
            (
                Duration::from_millis(u64::from(e.glide.duration_ms)),
                e.glide.easing,
                Blend::Fade { via: None },
            )
        })
    });
    let (w, h) = (size.0 as f32, size.1 as f32);
    let mut bottom = rbs_gpu::node::NodeLayer::new(program, 0, 0.0, 0.0, w, h);
    bottom.opaque = true;
    let Some((duration, ease, blend)) = plan else {
        return vec![bottom];
    };
    let ease = easing(ease);
    let mut top = rbs_gpu::node::NodeLayer::new(incoming, 0, 0.0, 0.0, w, h);
    top.opaque = true;
    let mut layers = Vec::with_capacity(3);
    match blend {
        // A Stinger is never rehearsed or held (ADR-0140 decision 8).
        Blend::Fade { via: None } | Blend::Move | Blend::Stinger { .. } => {
            top.fade = Some(Fade {
                from: 0.0,
                start,
                duration,
                easing: ease,
            });
            layers.push(bottom);
        }
        Blend::Fade {
            via: Some([r, g, b]),
        } => {
            let half = duration / 2;
            let mut colour = rbs_gpu::node::NodeLayer::new(incoming, 0, 0.0, 0.0, w, h);
            colour.style = LayerStyle::new(LayerShape::Box, w.max(h), [r, g, b, 255], [w, h]);
            colour.fade = Some(Fade {
                from: 0.0,
                start,
                duration: half,
                easing: ease,
            });
            top.fade = Some(Fade {
                from: 0.0,
                start: start + half,
                duration: duration - half,
                easing: ease,
            });
            layers.push(bottom);
            layers.push(colour);
        }
        Blend::Swipe { direction, push } => {
            let offset = swipe_offset(direction, size);
            top.slide = Some(Slide {
                from: shifted(top.transform, offset),
                start,
                duration,
                easing: ease,
            });
            if push {
                let at = bottom.transform;
                bottom.transform = shifted(at, (-offset.0, -offset.1));
                bottom.slide = Some(Slide {
                    from: at,
                    start,
                    duration,
                    easing: ease,
                });
            }
            layers.push(bottom);
        }
    }
    layers.push(top);
    layers
}

/// The offset, in canvas pixels, the incoming canvas of a Swipe starts at: one canvas
/// width or height off the edge opposite `direction`.
fn swipe_offset(direction: SwipeDirection, (w, h): (u32, u32)) -> (f32, f32) {
    let (w, h) = (w as f32, h as f32);
    match direction {
        SwipeDirection::Left => (w, 0.0),
        SwipeDirection::Right => (-w, 0.0),
        SwipeDirection::Up => (0.0, h),
        _ => (0.0, -h),
    }
}

/// `t` moved by `(dx, dy)` canvas pixels.
fn shifted(mut t: rbs_gpu::math::Affine2, (dx, dy): (f32, f32)) -> rbs_gpu::math::Affine2 {
    t.tx += dx;
    t.ty += dy;
    t
}

/// A start, end or hold found by a step, made an event at the step's time.
struct Step {
    canvas: CanvasId,
    phase: TransitionPhase,
    kind: TransitionKind,
    duration: Duration,
    scenes: (Option<SceneId>, Option<SceneId>),
    manual: bool,
    progress: Option<f32>,
}

impl Step {
    fn new(
        canvas: CanvasId,
        phase: TransitionPhase,
        kind: TransitionKind,
        duration: Duration,
        scenes: (Option<SceneId>, Option<SceneId>),
    ) -> Self {
        Self {
            canvas,
            phase,
            kind,
            duration,
            scenes,
            manual: false,
            progress: None,
        }
    }

    fn manual(mut self, progress: Option<f32>) -> Self {
        self.manual = true;
        self.progress = progress;
        self
    }

    fn event(self, at: Timestamp) -> TransitionEvent {
        TransitionEvent {
            canvas: self.canvas,
            phase: self.phase,
            kind: self.kind,
            duration: self.duration,
            from: self.scenes.0,
            to: self.scenes.1,
            at,
            manual: self.manual,
            progress: self.progress,
        }
    }
}

/// The incoming canvas over the outgoing `layers`, frozen at progress `t` (ADR-0135
/// decision 3): what the timed transition shows at `t` times its duration. Returns the
/// layers to draw above `layers`, and moves `layers` out for a Slide.
fn frozen_blend(
    compositor: NodeId,
    (w, h): (u32, u32),
    blend: Blend,
    ease: GlideEasing,
    t: f32,
    layers: &mut [rbs_gpu::node::NodeLayer],
) -> Vec<rbs_gpu::node::NodeLayer> {
    let e = |u: f32| easing(ease).map(f64::from(u));
    let full = || rbs_gpu::node::NodeLayer::new(compositor, 0, 0.0, 0.0, w as f32, h as f32);
    let mut top = full();
    // Transparent areas are the picture over black, as after the transition.
    top.opaque = true;
    let mut out = Vec::with_capacity(2);
    match blend {
        Blend::Fade { via: None } | Blend::Move | Blend::Stinger { .. } => top.opacity = e(t),
        Blend::Fade {
            via: Some([r, g, b]),
        } => {
            let mut colour = full();
            colour.style = LayerStyle::new(
                LayerShape::Box,
                w.max(h) as f32,
                [r, g, b, 255],
                [w as f32, h as f32],
            );
            colour.opacity = e((2.0 * t).min(1.0));
            out.push(colour);
            top.opacity = e((2.0 * t - 1.0).max(0.0));
        }
        Blend::Swipe { direction, push } => {
            let (dx, dy) = swipe_offset(direction, (w, h));
            let gone = e(t);
            let k = 1.0 - gone;
            top.transform = shifted(top.transform, (dx * k, dy * k));
            if push {
                for l in layers.iter_mut() {
                    l.transform = shifted(l.transform, (-dx * gone, -dy * gone));
                }
            }
        }
    }
    out.push(top);
    out
}

/// The levels `scene` is heard with (ADR-0091): its own entries over the sources'.
fn scene_mix(collection: &SceneCollection, scene: SceneId) -> BTreeMap<SourceId, SourceAudio> {
    let levels = collection.scene(scene).and_then(|s| s.audio.as_ref());
    collection
        .sources()
        .map(|s| (s.id, levels.map_or(s.audio, |l| l.heard(s.id, s.audio))))
        .collect()
}

fn easing(e: GlideEasing) -> rbs_gpu::node::Easing {
    match e {
        GlideEasing::Linear => rbs_gpu::node::Easing::Linear,
        _ => rbs_gpu::node::Easing::EaseInOut,
    }
}

impl Studio {
    /// Sets the next program change's one-off kind and duration (ADR-0128 decision 13).
    /// The next [`Studio::update`] uses it whether or not the program changes, then
    /// forgets it.
    pub fn set_transition_once(&mut self, once: TransitionOnce) {
        self.transition_once = Some(once);
    }

    /// The starts and ends since the last call, oldest first.
    pub fn take_transition_events(&mut self) -> Vec<TransitionEvent> {
        std::mem::take(&mut self.transition_events)
    }

    /// The collection's transition with the one-off over it; takes the one-off.
    pub(super) fn effective_transition(&mut self, collection: &SceneCollection) -> Effective {
        // The incoming scene's override (ADR-0135 decision 8): the main canvas's new
        // program scene when it changes, else the first further canvas's that does.
        let changed = |canvas: CanvasId, old: Option<SceneId>| {
            program_scene(collection, canvas).filter(|new| Some(*new) != old)
        };
        let incoming = changed(
            self.settings.canvas,
            program_scene(&self.collection, self.settings.canvas),
        )
        .or_else(|| {
            self.extras
                .iter()
                .find_map(|x| changed(x.canvas, program_scene(&self.collection, x.canvas)))
        });
        let once = self.transition_once.take().unwrap_or_default();
        let mut e = Self::transition_into(collection, incoming, once);
        // A manual transition's end starts no timed one (ADR-0135 decision 5).
        if self.manual_take {
            e.transition.kind = TransitionKind::Cut;
        }
        e
    }

    /// The transition a change into `incoming` uses: `once`, then the scene's
    /// override, then the collection (ADR-0135 decision 8).
    fn transition_into(
        collection: &SceneCollection,
        incoming: Option<SceneId>,
        once: TransitionOnce,
    ) -> Effective {
        let mut e = Effective {
            transition: collection.transition(),
            glide: collection.glide(),
            source_glides: true,
        };
        let scene_override = incoming
            .and_then(|id| collection.scene(id))
            .and_then(|s| s.transition_in)
            .unwrap_or_default();
        // Precedence: the one-off, then the scene's override, then the collection. A
        // one-off kind without a duration uses that kind's collection duration.
        if let Some(kind) = once.kind.or(scene_override.kind) {
            e.transition.kind = kind;
        }
        let duration = once.duration_ms.or(if once.kind.is_some() {
            None
        } else {
            scene_override.duration_ms
        });
        if let Some(ms) = duration {
            match e.transition.kind {
                TransitionKind::Fade => e.transition.fade.duration_ms = ms,
                TransitionKind::Swipe => e.transition.swipe.duration_ms = ms,
                TransitionKind::Slide => e.transition.slide.duration_ms = ms,
                TransitionKind::Move => {
                    e.glide.duration_ms = ms;
                    e.source_glides = false;
                }
                _ => {}
            }
        }
        e
    }

    /// Starts a transition on each canvas whose program scene changed in this update
    /// and whose collection transition draws the incoming picture. A change while one
    /// runs ends that one at once: its incoming scene is the outgoing scene now
    /// (decision 9). A canvas whose program did not change keeps its transition.
    pub(super) fn note_transitions(
        &mut self,
        collection: &SceneCollection,
        effective: &Effective,
        old_main: Option<SceneId>,
        old_extra: &BTreeMap<CanvasId, Option<SceneId>>,
        old_layers: &BTreeMap<CanvasId, Vec<BoundLayer>>,
    ) {
        let mut changed: Vec<(CanvasId, Option<SceneId>, Option<SceneId>)> = Vec::new();
        let new_main = program_scene(collection, self.settings.canvas);
        if old_main.is_some() && old_main != new_main {
            changed.push((self.settings.canvas, old_main, new_main));
        }
        for extra in &self.extras {
            let previous = old_extra.get(&extra.canvas).copied().flatten();
            let next = program_scene(collection, extra.canvas);
            if previous.is_some() && previous != next {
                changed.push((extra.canvas, previous, next));
            }
        }
        if changed.is_empty() {
            return;
        }
        let manual = self.manual_take;
        let blend = blend_of(effective.transition).or_else(|| move_of(effective));
        // A Stinger plays when the main canvas changes; further canvases follow its
        // cut (ADR-0140 decision 7). Otherwise a Stinger change is a Cut.
        let main_changes = changed.iter().any(|(c, _, _)| *c == self.settings.canvas);
        let stinger = if manual || !main_changes {
            None
        } else {
            self.stinger_plan(collection, effective)
        };
        let now = self.engine.clock().now();
        let cued = self.cues.items();
        let globals_of = |canvas: CanvasId| -> BTreeSet<ItemId> {
            collection
                .globals_of(canvas)
                .and_then(|g| collection.scene(g))
                .map(|s| s.items.iter().map(|i| i.id).collect())
                .unwrap_or_default()
        };
        let heard: BTreeSet<SourceId> = self
            .bound_mixer
            .as_ref()
            .map(|m| m.inputs.iter().map(|i| i.source).collect())
            .unwrap_or_default();
        for (canvas, from_scene, to_scene) in changed {
            // A manual transition's end (ADR-0135 decision 5): the same entry keeps
            // its incoming node, now drawing the program, with the outgoing scene's
            // sources until the settle; the plain layers go out now.
            if let Some(t) = self
                .transitions
                .get_mut(&canvas)
                .filter(|t| t.manual.as_ref().is_some_and(|m| !m.cancelled))
            {
                let drawn: BTreeSet<SourceId> = old_layers
                    .get(&canvas)
                    .map(|l| l.iter().map(|l| l.source).collect())
                    .unwrap_or_default();
                let heard = if canvas == self.settings.canvas {
                    heard.clone()
                } else {
                    BTreeSet::new()
                };
                t.outgoing = t.outgoing.clone().with_program(drawn, heard);
                t.phase = Phase::Ending(now);
                if let Some(m) = &mut t.manual {
                    m.taken = true;
                    let duration = m
                        .started
                        .map_or(Duration::ZERO, |s| now.saturating_since(s));
                    self.transition_events.push(TransitionEvent {
                        canvas,
                        phase: TransitionPhase::Ended,
                        kind: m.kind,
                        duration,
                        from: from_scene,
                        to: to_scene,
                        at: now,
                        manual: true,
                        progress: None,
                    });
                }
                continue;
            }
            // A running one ends at once; its compositor is kept for the next one.
            let incoming = self.transitions.remove(&canvas).and_then(|t| t.incoming);
            let from = old_layers.get(&canvas).cloned().unwrap_or_default();
            let to_drawn: BTreeSet<SourceId> = to_scene
                .and_then(|scene| collection.visible_sources_cued(canvas, scene, &cued).ok())
                .unwrap_or_default();
            // A Move runs as a transition only when something leaves (decision 5).
            let leaving: BTreeSet<SourceId> = {
                let globals = globals_of(canvas);
                from.iter()
                    .filter(|l| !l.path.first().is_some_and(|i| globals.contains(i)))
                    .map(|l| l.source)
                    .filter(|s| !to_drawn.contains(s))
                    .collect()
            };
            let run = blend.filter(|(_, _, b)| *b != Blend::Move || !leaving.is_empty());
            let run = match &stinger {
                Some(plan) => Some((
                    plan.length,
                    GlideEasing::Linear,
                    Blend::Stinger {
                        cut: plan.cut,
                        follower: canvas != self.settings.canvas,
                    },
                )),
                None => run,
            };
            let Some((duration, easing, kind_blend)) = run else {
                // No incoming picture: it starts now, and a Move ends after its glide.
                let kind = effective.kind();
                let duration = if kind == TransitionKind::Move {
                    Duration::from_millis(u64::from(effective.glide.duration_ms))
                } else {
                    Duration::ZERO
                };
                let event = TransitionEvent {
                    canvas,
                    phase: TransitionPhase::Started,
                    kind,
                    duration,
                    from: from_scene,
                    to: to_scene,
                    at: now,
                    manual,
                    progress: None,
                };
                self.transition_events.push(event);
                // A change on this canvas replaces its pending end.
                self.transition_ends.retain(|e| e.canvas != canvas);
                let end = TransitionEvent {
                    phase: TransitionPhase::Ended,
                    at: now + duration,
                    ..event
                };
                if duration.is_zero() {
                    self.transition_events.push(end);
                } else {
                    self.transition_ends.push(end);
                }
                continue;
            };
            self.transition_ends.retain(|e| e.canvas != canvas);
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
            let (from_mix, to_heard) = if canvas == self.settings.canvas {
                (self.mix.clone(), to_drawn)
            } else {
                (BTreeMap::new(), BTreeSet::new())
            };
            // A Move has no incoming picture to wait for: it starts now, with the
            // glide (decision 5).
            let (outgoing, phase) = if kind_blend == Blend::Move {
                self.transition_events.push(TransitionEvent {
                    canvas,
                    phase: TransitionPhase::Started,
                    kind: TransitionKind::Move,
                    duration,
                    from: from_scene,
                    to: to_scene,
                    at: now,
                    manual: false,
                    progress: None,
                });
                (
                    OutgoingScene::without_incoming(leaving, heard),
                    Phase::Running(now),
                )
            } else {
                (OutgoingScene::new(drawn, heard), Phase::Waiting(now))
            };
            // The main canvas's Stinger change keeps the stinger bound and arms it
            // before the apply that makes it Active (ADR-0140 decision 4).
            let run_stinger = match (&stinger, kind_blend) {
                (
                    Some(plan),
                    Blend::Stinger {
                        follower: false, ..
                    },
                ) => {
                    self.stinger_arm = Some(plan.source);
                    Some(stinger::StingerRun {
                        source: plan.source,
                        generation: None,
                        failed: false,
                        size: plan.size,
                    })
                }
                _ => None,
            };
            let outgoing = outgoing.with_stinger(run_stinger.map(|r| r.source));
            self.transitions.insert(
                canvas,
                CanvasTransition {
                    from,
                    outgoing,
                    duration,
                    easing,
                    blend: kind_blend,
                    phase,
                    incoming,
                    from_mix,
                    to_heard,
                    scenes: (from_scene, to_scene),
                    manual: None,
                    stinger: run_stinger,
                },
            );
        }
        self.disarm_stale_stinger();
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
        let now = self.engine.clock().now();
        let stinger_layer = self.transitions.get(&canvas).and_then(|t| {
            let (Phase::Running(at), Some(run), Some(source)) =
                (t.phase, t.stinger, t.outgoing.stinger)
            else {
                return None;
            };
            self.stinger_layer(source, run.size, size, at)
        });
        let t = self.transitions.get_mut(&canvas)?;
        if t.blend == Blend::Move {
            let Phase::Running(start) = t.phase else {
                return None;
            };
            // The incoming layers with their glide, then the leaving ones fading out
            // above them so the fade is seen, then the globals above everything.
            let mut current = program.clone();
            current.layers.retain(|l| !is_global(l));
            let drawn: BTreeSet<SourceId> = current.layers.iter().map(|l| l.source).collect();
            let mut layers = paint_layers(&current, plan, self.glide.as_ref(), now, canvas);
            let mut leaving = program.clone();
            leaving.layers = t
                .from
                .iter()
                .filter(|l| !is_global(l) && !drawn.contains(&l.source))
                .cloned()
                .collect();
            for mut l in node_layers(&leaving, plan) {
                l.fade = Some(Fade {
                    from: l.opacity,
                    start,
                    duration: t.duration,
                    // As the glide's fade-ins (ADR-0100).
                    easing: rbs_gpu::node::Easing::Linear,
                });
                l.opacity = 0.0;
                layers.push(l);
            }
            let mut above = program.clone();
            above.layers.retain(|l| is_global(l));
            layers.extend(node_layers(&above, plan));
            return Some(layers);
        }
        let running = matches!(t.phase, Phase::Running(_));
        if let Some(view) = &mut t.incoming {
            let mut incoming = view.bound.clone();
            if running {
                incoming.layers.retain(|l| !is_global(l));
            }
            let _ = view.layers.send(node_layers(&incoming, plan));
        }
        let mut outgoing = program.clone();
        // A manual transition keeps the program as it is, edits included.
        if t.manual.is_none() {
            outgoing.layers.clone_from(&t.from);
        }
        if let (Some(m), Phase::Running(_)) = (&t.manual, t.phase) {
            outgoing.layers.retain(|l| !is_global(l));
            let mut layers = node_layers(&outgoing, plan);
            if let Some(view) = &t.incoming {
                let top = frozen_blend(view.compositor, size, t.blend, t.easing, m.t, &mut layers);
                layers.extend(top);
            }
            let mut above = program.clone();
            above.layers.retain(|l| is_global(l));
            layers.extend(node_layers(&above, plan));
            return Some(layers);
        }
        match t.phase {
            Phase::Waiting(_) => Some(node_layers(&outgoing, plan)),
            Phase::Running(start) => {
                outgoing.layers.retain(|l| !is_global(l));
                let mut layers = node_layers(&outgoing, plan);
                if let Some(view) = &t.incoming {
                    let (w, h) = size;
                    let full = || {
                        rbs_gpu::node::NodeLayer::new(
                            view.compositor,
                            0,
                            0.0,
                            0.0,
                            w as f32,
                            h as f32,
                        )
                    };
                    let mut top = full();
                    // Transparent areas are the picture over black, as encoded after
                    // the transition (decision 5).
                    top.opaque = true;
                    let ease = easing(t.easing);
                    match t.blend {
                        Blend::Fade { via: None } => {
                            top.fade = Some(Fade {
                                from: 0.0,
                                start,
                                duration: t.duration,
                                easing: ease,
                            });
                        }
                        Blend::Fade {
                            via: Some([r, g, b]),
                        } => {
                            // The colour covers the outgoing scene in the first half,
                            // the incoming scene comes in over it in the second. The
                            // colour is a layer of the incoming canvas whose border
                            // fills its whole box: no node of its own.
                            let half = t.duration / 2;
                            let mut colour = full();
                            colour.style = LayerStyle::new(
                                LayerShape::Box,
                                w.max(h) as f32,
                                [r, g, b, 255],
                                [w as f32, h as f32],
                            );
                            colour.fade = Some(Fade {
                                from: 0.0,
                                start,
                                duration: half,
                                easing: ease,
                            });
                            layers.push(colour);
                            top.fade = Some(Fade {
                                from: 0.0,
                                start: start + half,
                                duration: t.duration - half,
                                easing: ease,
                            });
                        }
                        // Drawn above, without an incoming compositor.
                        Blend::Move => {}
                        // The incoming canvas replaces the outgoing layers at the cut,
                        // in one step, under the stinger (ADR-0140 decision 4).
                        Blend::Stinger { cut, .. } => {
                            top.fade = Some(Fade {
                                from: 0.0,
                                start: start + cut,
                                duration: Duration::ZERO,
                                easing: rbs_gpu::node::Easing::Linear,
                            });
                        }
                        Blend::Swipe { direction, push } => {
                            let offset = swipe_offset(direction, size);
                            top.slide = Some(Slide {
                                from: shifted(top.transform, offset),
                                start,
                                duration: t.duration,
                                easing: ease,
                            });
                            if push {
                                let out = (-offset.0, -offset.1);
                                for l in &mut layers {
                                    let at = l.transform;
                                    l.transform = shifted(at, out);
                                    l.slide = Some(Slide {
                                        from: at,
                                        start,
                                        duration: t.duration,
                                        easing: ease,
                                    });
                                }
                            }
                        }
                    }
                    layers.push(top);
                }
                layers.extend(stinger_layer);
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
        if !self.transition_ends.is_empty() {
            let now = self.engine.clock().now();
            let (due, later): (Vec<_>, Vec<_>) =
                self.transition_ends.drain(..).partition(|e| e.at <= now);
            self.transition_ends = later;
            self.transition_events.extend(due);
        }
        if self.transitions.is_empty() {
            return false;
        }
        let now = self.engine.clock().now();
        let mut resend = false;
        let mut done = false;
        let layers_pending = self.layers.pending() && !self.layers.is_closed();
        let mut events = Vec::new();
        // The Stinger gate of the main canvas and its followers (ADR-0140).
        let gate = self.stinger_gate(now);
        let stopped: Option<SourceId> = self
            .transitions
            .values()
            .filter(|t| matches!(t.phase, Phase::Running(_)))
            .find_map(|t| t.outgoing.stinger)
            .filter(|s| self.stinger_stopped(*s));
        for (&canvas, t) in &mut self.transitions {
            let kind = t.manual.as_ref().map_or(t.blend.kind(), |m| m.kind);
            if let (Phase::Waiting(_), Blend::Stinger { .. }, Some(gate)) =
                (t.phase, t.blend, gate.as_ref())
            {
                match gate {
                    stinger::Gate::Wait => {}
                    stinger::Gate::Play(at) => {
                        t.phase = Phase::Running(*at);
                        resend = true;
                        events.push(Step::new(
                            canvas,
                            TransitionPhase::Started,
                            kind,
                            t.duration,
                            t.scenes,
                        ));
                    }
                    stinger::Gate::Cut => {
                        t.end_stinger(now);
                        resend = true;
                        for phase in [TransitionPhase::Started, TransitionPhase::Ended] {
                            events.push(Step::new(canvas, phase, kind, Duration::ZERO, t.scenes));
                        }
                    }
                }
                continue;
            }
            // A stinger that faulted or reached its end hold ends its change at once
            // (ADR-0140 decision 5).
            if let (Phase::Running(_), Some(source)) = (t.phase, t.outgoing.stinger)
                && stopped == Some(source)
            {
                t.end_stinger(now);
                resend = true;
                events.push(Step::new(
                    canvas,
                    TransitionPhase::Ended,
                    kind,
                    t.duration,
                    t.scenes,
                ));
                continue;
            }
            match t.phase {
                Phase::Waiting(since) => {
                    let ready = t
                        .incoming
                        .as_ref()
                        .is_some_and(|v| v.ready.load(Ordering::Acquire) != 0);
                    if ready || now.saturating_since(since) >= READY_WAIT {
                        t.phase = Phase::Running(now);
                        resend = true;
                        if let Some(m) = &mut t.manual {
                            // Past its ready gate: `started`, then where it holds.
                            m.started = Some(now);
                            events.push(
                                Step::new(
                                    canvas,
                                    TransitionPhase::Started,
                                    kind,
                                    Duration::ZERO,
                                    t.scenes,
                                )
                                .manual(None),
                            );
                            events.push(
                                Step::new(
                                    canvas,
                                    TransitionPhase::Holding,
                                    kind,
                                    Duration::ZERO,
                                    t.scenes,
                                )
                                .manual(Some(m.t)),
                            );
                        } else {
                            events.push(Step::new(
                                canvas,
                                TransitionPhase::Started,
                                kind,
                                t.duration,
                                t.scenes,
                            ));
                        }
                    }
                }
                // A manual transition holds until its value changes.
                Phase::Running(_) if t.manual.is_some() => {}
                Phase::Running(start) => {
                    if now.saturating_since(start) >= t.duration {
                        t.phase = Phase::Ending(now);
                        resend = true;
                        events.push(Step::new(
                            canvas,
                            TransitionPhase::Ended,
                            kind,
                            t.duration,
                            t.scenes,
                        ));
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
        self.transition_events
            .extend(events.into_iter().map(|s| s.event(now)));
        self.disarm_stale_stinger();
        if resend {
            let plan = self.graph.plan();
            self.send_program_layers(&plan);
            if self.transitions.contains_key(&self.settings.canvas) {
                self.send_mix();
            }
        }
        done
    }

    /// How the main canvas's transition shapes its mixer strips, `None` while none
    /// runs there.
    pub(super) fn transition_mix(&self) -> Option<MixShape> {
        let t = self.transitions.get(&self.settings.canvas)?;
        if let Some(m) = &t.manual {
            let ease = easing(t.easing);
            return Some(MixShape {
                manual: (!m.cancelled && !m.taken)
                    .then(|| (ease.map(f64::from(m.t)), m.to_mix.clone())),
                phase: t.phase,
                duration: t.duration,
                outgoing: m.from_heard.clone(),
                from_mix: t.from_mix.clone(),
                // Cancelled: the program's own sources only (decision 6).
                to_heard: if m.cancelled {
                    m.from_heard.clone()
                } else {
                    t.to_heard.clone()
                },
                stinger: None,
            });
        }
        let stinger = match t.blend {
            Blend::Stinger { cut, .. } => Some((t.outgoing.stinger, cut)),
            _ => None,
        };
        Some(MixShape {
            manual: None,
            phase: t.phase,
            duration: t.duration,
            outgoing: t.outgoing.heard.clone(),
            from_mix: t.from_mix.clone(),
            to_heard: t.to_heard.clone(),
            stinger,
        })
    }

    /// How long the control thread may sleep before a transition needs a step. `None`
    /// when none runs, so nothing is armed (P13).
    pub fn transition_wait(&self, now: Timestamp) -> Option<Duration> {
        let ends = self
            .transition_ends
            .iter()
            .map(|e| e.at.saturating_since(now))
            .min();
        let running = self
            .transitions
            .values()
            .map(|t| match t.phase {
                Phase::Waiting(since) => {
                    // The ready counter is polled; look again within a frame or so.
                    let left = READY_WAIT.saturating_sub(now.saturating_since(since));
                    left.min(Duration::from_millis(5))
                }
                // A held T-bar needs no step until its value changes.
                Phase::Running(_) if t.manual.is_some() => Duration::MAX,
                Phase::Running(start) => t.duration.saturating_sub(now.saturating_since(start)),
                Phase::Ending(_) => Duration::from_millis(5),
                Phase::Done => Duration::ZERO,
            })
            .min();
        let running = running.filter(|d| *d != Duration::MAX);
        match (ends, running) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Applies a T-bar value `t` on `canvas` (ADR-0135 decisions 1 to 7); call it once
    /// per service step with the latest value. The first `t > 0` starts a manual
    /// transition from the program scene to the preview scene (a timed one running
    /// there ends at once); it waits for the incoming picture, then holds the blend
    /// at `t`. `t = 0` cancels it. At `t = 1` it answers [`ManualStep::Take`]: the host
    /// makes the program change now, which ends it without a timed transition. Does
    /// nothing on a canvas without a preview scene.
    pub fn transition_progress(
        &mut self,
        canvas: CanvasId,
        t: f32,
        sources: &SourceTypes,
    ) -> Result<ManualStep, RecordError> {
        let t = if t.is_finite() {
            t.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let now = self.engine.clock().now();
        let live = |tr: &&mut CanvasTransition| {
            tr.manual.as_ref().is_some_and(|m| !m.cancelled && !m.taken)
        };
        if let Some(tr) = self.transitions.get_mut(&canvas).filter(live) {
            if t >= 1.0 {
                self.manual_take = true;
                return Ok(ManualStep::Take);
            }
            if t <= 0.0 {
                self.cancel_manual(canvas);
                return Ok(ManualStep::Idle);
            }
            let running = matches!(tr.phase, Phase::Running(_));
            let Some(m) = tr.manual.as_mut() else {
                return Ok(ManualStep::Idle);
            };
            if m.t == t {
                return Ok(ManualStep::Holding);
            }
            m.t = t;
            if running {
                let held = Step::new(
                    canvas,
                    TransitionPhase::Holding,
                    m.kind,
                    Duration::ZERO,
                    tr.scenes,
                )
                .manual(Some(t))
                .event(now);
                // The latest value only, at most once per service step.
                self.transition_events
                    .retain(|e| !(e.canvas == canvas && e.phase == TransitionPhase::Holding));
                self.transition_events.push(held);
                self.resend_transition(canvas);
            }
            return Ok(ManualStep::Holding);
        }
        if t <= 0.0 {
            return Ok(ManualStep::Idle);
        }
        // A T-bar move ends a rehearsal at once (ADR-0135 decision 13).
        self.drop_rehearsal();
        let collection = self.collection.clone();
        let Some(preview) = collection.canvas(canvas).and_then(|c| c.preview) else {
            return Ok(ManualStep::Idle);
        };
        let e = Self::transition_into(&collection, Some(preview), TransitionOnce::default());
        let kind = e.kind();
        // Move is drawn as a crossfade with its easing (decision 3).
        let blend = blend_of(e.transition).or_else(|| {
            (kind == TransitionKind::Move).then_some((
                Duration::ZERO,
                e.glide.easing,
                Blend::Fade { via: None },
            ))
        });
        let Some((_, easing, blend)) = blend else {
            // Cut (or a duration of 0): nothing to hold; it cuts at the end.
            if t >= 1.0 {
                self.manual_take = true;
                return Ok(ManualStep::Take);
            }
            return Ok(ManualStep::Idle);
        };
        if t >= 1.0 {
            self.manual_take = true;
            return Ok(ManualStep::Take);
        }
        // A timed transition on this canvas ends at once (decision 7); its
        // compositor is kept.
        let incoming = match self.transitions.remove(&canvas) {
            Some(old) => {
                if matches!(old.phase, Phase::Waiting(_) | Phase::Running(_)) {
                    let kind = old.blend.kind();
                    self.transition_events.push(
                        Step::new(
                            canvas,
                            TransitionPhase::Ended,
                            kind,
                            old.duration,
                            old.scenes,
                        )
                        .event(now),
                    );
                }
                old.incoming
            }
            None => None,
        };
        if let Some(view) = &incoming {
            view.ready.store(0, Ordering::Release);
        }
        let main = canvas == self.settings.canvas;
        let from = self.layer_snapshot().remove(&canvas).unwrap_or_default();
        let cued = self.cues.items();
        let (from_heard, from_mix, to_heard, to_mix) = if main {
            (
                self.bound_mixer
                    .as_ref()
                    .map(|m| m.inputs.iter().map(|i| i.source).collect())
                    .unwrap_or_default(),
                self.mix.clone(),
                collection
                    .visible_sources_cued(canvas, preview, &cued)
                    .map(|v| v.into_iter().collect())
                    .unwrap_or_default(),
                scene_mix(&collection, preview),
            )
        } else {
            Default::default()
        };
        self.transitions.insert(
            canvas,
            CanvasTransition {
                from,
                outgoing: OutgoingScene::towards(preview),
                duration: Duration::ZERO,
                easing,
                blend,
                phase: Phase::Waiting(now),
                incoming,
                from_mix,
                to_heard,
                scenes: (program_scene(&collection, canvas), Some(preview)),
                manual: Some(Manual {
                    t,
                    kind,
                    started: None,
                    from_heard,
                    to_mix,
                    cancelled: false,
                    taken: false,
                }),
                stinger: None,
            },
        );
        self.update(&collection, sources)?;
        Ok(ManualStep::Holding)
    }

    /// Cancels the manual transition of `canvas` (ADR-0135 decision 6): the program
    /// alone again and the strips back to their levels; it is unbound at the settle
    /// that follows. `ended` goes out with `to` equal to `from` once it had started.
    /// False when none holds there.
    pub fn cancel_manual(&mut self, canvas: CanvasId) -> bool {
        let now = self.engine.clock().now();
        let Some(tr) = self.transitions.get_mut(&canvas) else {
            return false;
        };
        let Some(m) = tr.manual.as_mut().filter(|m| !m.cancelled && !m.taken) else {
            return false;
        };
        m.cancelled = true;
        m.t = 0.0;
        if let Some(started) = m.started {
            let from = tr.scenes.0;
            self.transition_events.push(TransitionEvent {
                canvas,
                phase: TransitionPhase::Ended,
                kind: m.kind,
                duration: now.saturating_since(started),
                from,
                to: from,
                at: now,
                manual: true,
                progress: None,
            });
        }
        tr.phase = Phase::Ending(now);
        self.transition_events
            .retain(|e| !(e.canvas == canvas && e.phase == TransitionPhase::Holding));
        self.resend_transition(canvas);
        true
    }

    /// True while a manual transition holds on `canvas` (not cancelled or taken).
    pub fn manual_transition(&self, canvas: CanvasId) -> bool {
        self.transitions
            .get(&canvas)
            .and_then(|t| t.manual.as_ref())
            .is_some_and(|m| !m.cancelled && !m.taken)
    }

    /// Sends the program layers and, on the main canvas, the strips again.
    fn resend_transition(&mut self, canvas: CanvasId) {
        let plan = self.graph.plan();
        self.send_program_layers(&plan);
        if canvas == self.settings.canvas {
            self.send_mix();
        }
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

    /// Ends every transition and a rehearsal at once (leaving studio mode, a
    /// collection switch, stopping): the next update binds without them.
    pub fn end_transitions(&mut self) {
        self.drop_rehearsal();
        for t in self.transitions.values_mut() {
            t.phase = Phase::Done;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn a_rehearsal_blends_the_two_pictures_in_its_own_size() {
        use rbs_scene::model::SceneScope;
        let mut col = SceneCollection::new();
        let (scene, _) = col.add_scene("Next", SceneScope::Shared).unwrap();
        let (program, incoming) = (any_node(), any_node());
        let at = Timestamp::from_nanos(7);
        // Cut and a duration of 0: nothing to rehearse, only the program picture.
        let mut t = CollectionTransition {
            kind: TransitionKind::Cut,
            ..CollectionTransition::default()
        };
        col.set_transition(t).unwrap();
        assert_eq!(rehearsal_plan(&col, scene), None);
        let only = rehearsal_layers(&col, scene, (program, incoming), (64, 36), at);
        assert_eq!(only.len(), 1);
        // Fade: the preview picture fades in over the program picture, both filling
        // the rehearsal's size.
        t.kind = TransitionKind::Fade;
        t.fade.duration_ms = 300;
        col.set_transition(t).unwrap();
        assert_eq!(
            rehearsal_plan(&col, scene).map(|(d, _, k)| (d, k)),
            Some((Duration::from_millis(300), TransitionKind::Fade))
        );
        let l = rehearsal_layers(&col, scene, (program, incoming), (64, 36), at);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].producer, l[1].producer), (program, incoming));
        assert!(l[0].fade.is_none());
        let fade = l[1].fade.unwrap();
        assert_eq!(
            (fade.from, fade.start, fade.duration),
            (0.0, at, Duration::from_millis(300))
        );
        // Move rehearses as a crossfade of the glide's duration.
        t.kind = TransitionKind::Move;
        col.set_transition(t).unwrap();
        let glide = col.glide().duration_ms;
        assert_eq!(
            rehearsal_plan(&col, scene).map(|(d, _, k)| (d, k)),
            Some((
                Duration::from_millis(u64::from(glide)),
                TransitionKind::Move
            ))
        );
        let l = rehearsal_layers(&col, scene, (program, incoming), (64, 36), at);
        assert!(l[1].fade.is_some());
        // Slide: the preview picture comes in from one rehearsal width away and the
        // program picture leaves by the same offset.
        t.kind = TransitionKind::Slide;
        t.slide.duration_ms = 500;
        t.slide.direction = SwipeDirection::Left;
        col.set_transition(t).unwrap();
        let l = rehearsal_layers(&col, scene, (program, incoming), (64, 36), at);
        let (top, bottom) = (l[1], l[0]);
        assert_eq!(top.slide.unwrap().from, shifted(top.transform, (64.0, 0.0)));
        assert_eq!(
            bottom.transform,
            shifted(bottom.slide.unwrap().from, (-64.0, 0.0))
        );
        // The scene's own transition wins over the collection's.
        col.set_scene_transition(
            scene,
            Some(rbs_scene::SceneTransition {
                kind: Some(TransitionKind::Cut),
                duration_ms: None,
            }),
        )
        .unwrap();
        assert_eq!(rehearsal_plan(&col, scene), None);
    }

    #[test]
    fn fade_swipe_and_slide_with_a_duration_blend() {
        let mut t = CollectionTransition::default();
        assert_eq!(blend_of(t), None);
        t.kind = TransitionKind::Cut;
        assert_eq!(blend_of(t), None);
        t.kind = TransitionKind::Fade;
        t.fade.via = Some([1, 2, 3]);
        assert_eq!(
            blend_of(t),
            Some((
                Duration::from_millis(300),
                GlideEasing::Linear,
                Blend::Fade {
                    via: Some([1, 2, 3])
                }
            ))
        );
        t.fade.duration_ms = 0;
        assert_eq!(blend_of(t), None);
        t.kind = TransitionKind::Swipe;
        t.swipe.direction = SwipeDirection::Up;
        assert_eq!(
            blend_of(t),
            Some((
                Duration::from_millis(500),
                GlideEasing::EaseInOut,
                Blend::Swipe {
                    direction: SwipeDirection::Up,
                    push: false
                }
            ))
        );
        t.kind = TransitionKind::Slide;
        t.slide.duration_ms = 0;
        assert_eq!(blend_of(t), None);
    }

    /// A node id for layers that are never drawn.
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn any_node() -> NodeId {
        let mut g = rbs_graph::description::GraphDescription::new();
        let d = rbs_graph::node::NodeDescriptor::new(
            rbs_graph::node::NodeTypeId::new_static("test.node"),
            rbs_graph::node::NodeKind::Source,
            vec![],
            vec![rbs_graph::node::Pin::one(
                "video",
                rbs_graph::node::LinkKind::Video,
            )],
        );
        g.add_node(Arc::new(d)).unwrap()
    }

    #[test]
    fn a_frozen_blend_shows_what_the_timed_one_shows_at_t() {
        let node = any_node();
        let size = (1920, 1080);
        let lin = GlideEasing::Linear;
        // A crossfade: the incoming opacity is e(t).
        let top = frozen_blend(node, size, Blend::Fade { via: None }, lin, 0.3, &mut []);
        assert_eq!(top.len(), 1);
        assert!((top[0].opacity - 0.3).abs() < 1e-6);
        assert!(
            top[0].fade.is_none() && top[0].slide.is_none(),
            "static layers"
        );
        // Through a colour: the colour in the first half, the scene in the second.
        let via = Blend::Fade {
            via: Some([0, 0, 0]),
        };
        let first = frozen_blend(node, size, via, lin, 0.25, &mut []);
        assert!((first[0].opacity - 0.5).abs() < 1e-6);
        assert_eq!(first[1].opacity, 0.0);
        let second = frozen_blend(node, size, via, lin, 0.75, &mut []);
        assert_eq!(second[0].opacity, 1.0);
        assert!((second[1].opacity - 0.5).abs() < 1e-6);
        // A Slide at half way: the incoming canvas half in, the outgoing half out.
        let mut out = [rbs_gpu::node::NodeLayer::new(
            node, 0, 0.0, 0.0, 1920.0, 1080.0,
        )];
        let before = out[0].transform.tx;
        let slide = Blend::Swipe {
            direction: SwipeDirection::Left,
            push: true,
        };
        let top = frozen_blend(node, size, slide, lin, 0.5, &mut out);
        assert!((top[0].transform.tx - 960.0).abs() < 1e-3);
        assert!((out[0].transform.tx - (before - 960.0)).abs() < 1e-3);
    }

    #[test]
    fn a_held_t_bar_lerps_each_strip_between_its_two_gains() {
        let (a, b, c) = (
            SourceId::from_raw(1),
            SourceId::from_raw(2),
            SourceId::from_raw(3),
        );
        let level = SourceAudio::new;
        let strip = |source| MixStrip {
            producer: any_node(),
            source,
            meter: None,
            audio: level(1.0, false),
            ramp: None,
        };
        let shape = MixShape {
            manual: Some((
                0.25,
                BTreeMap::from([(a, level(0.4, false)), (c, level(1.0, false))]),
            )),
            phase: Phase::Running(Timestamp::ZERO),
            duration: Duration::ZERO,
            outgoing: BTreeSet::from([a, b]),
            from_mix: BTreeMap::from([(a, level(0.8, false)), (b, level(1.0, true))]),
            to_heard: BTreeSet::from([a, c]),
            stinger: None,
        };
        let mut strips = [strip(a), strip(b), strip(c)];
        shape.apply(&mut strips);
        // a: 0.8 -> 0.4; b: muted out, not in -> 0; c: only in, 0 -> 1.
        assert!((strips[0].audio.volume - 0.7).abs() < 1e-6);
        assert!(strips[1].audio.muted);
        assert!((strips[2].audio.volume - 0.25).abs() < 1e-6);
        assert!(strips.iter().all(|s| s.ramp.is_none()), "the default ramp");
    }

    #[test]
    fn a_stinger_switches_the_scene_sound_at_its_cut_and_leaves_its_own() {
        let (a, b, sting) = (
            SourceId::from_raw(1),
            SourceId::from_raw(2),
            SourceId::from_raw(9),
        );
        let level = SourceAudio::new;
        let strip = |source| MixStrip {
            producer: any_node(),
            source,
            meter: None,
            audio: level(1.0, false),
            ramp: None,
        };
        let anchor = Timestamp::from_nanos(1_000_000_000);
        let cut = Duration::from_millis(400);
        let shape = MixShape {
            manual: None,
            phase: Phase::Running(anchor),
            duration: Duration::from_secs(1),
            outgoing: BTreeSet::from([a]),
            from_mix: BTreeMap::from([(a, level(1.0, false))]),
            to_heard: BTreeSet::from([b]),
            stinger: Some((Some(sting), cut)),
        };
        let mut strips = [strip(a), strip(b), strip(sting)];
        shape.apply(&mut strips);
        let at = Some((anchor + cut, rbs_audio::DEFAULT_RAMP));
        // a leaves at the cut, b comes in at the cut, the stinger plays as it is.
        assert!(strips[0].audio.muted);
        assert_eq!(strips[0].ramp, at);
        assert!(!strips[1].audio.muted);
        assert_eq!(strips[1].ramp, at);
        assert!(!strips[2].audio.muted);
        assert_eq!(strips[2].ramp, None);
    }

    #[test]
    fn a_swipe_starts_one_canvas_off_the_edge_opposite_its_direction() {
        // A 9:16 canvas swipes its own size (decision 11).
        let size = (1080, 1920);
        assert_eq!(swipe_offset(SwipeDirection::Left, size), (1080.0, 0.0));
        assert_eq!(swipe_offset(SwipeDirection::Right, size), (-1080.0, 0.0));
        assert_eq!(swipe_offset(SwipeDirection::Up, size), (0.0, 1920.0));
        assert_eq!(swipe_offset(SwipeDirection::Down, size), (0.0, -1920.0));
    }
}
