//! Group Scroll on the studio side (ADR-0185, ADR-0187, ADR-0193): the runs every list
//! shares, the group spans of each list sent, live controls and failures.
//!
//! A run is keyed by canvas, originating scene, group and settings generation. The
//! generation is issued for each effective settings change the studio sees, from the
//! host's [`GroupIds`]; a held copy (its own scene id, mapped to its original with
//! [`super::StudioSettings::scene_origins`] or [`Studio::set_scene_origins`]) takes the
//! generation of its original when it first appears with equal settings and keeps it
//! after the original is edited.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::node::{GroupSpan, LayerList, NodeLayer};
use rbs_gpu::scroll::{LayerScroll, ScrollRun};
use rbs_graph::description::SchedulePlan;
use rbs_scene::binder::{BoundCompositor, BoundLayer, GroupMark, SpanLimits, SpanRefusal, resolve};
use rbs_scene::model::{CanvasId, GroupId, GroupScroll, SceneId, ScrollDirection, ScrollRepeat};
use rbs_sources::node_layers;
use rbs_sources::scroll::{ScrollRuns, layer_scroll, once_distance};

use super::Studio;
use super::scroll::{CompositorPort, ScrollControl, ScrollRefused};

/// The run a group occurrence's Scroll belongs to (ADR-0187 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupRunKey {
    /// The canvas.
    pub canvas: CanvasId,
    /// The originating scene (a held copy's original).
    pub scene: SceneId,
    /// The group.
    pub group: GroupId,
    /// The settings generation.
    pub generation: u64,
}

/// A group's live run, as `group.get` reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupRunState {
    /// This run incarnation: new for a new run and after a restart, never reused.
    pub id: u64,
    /// Paused by a live call.
    pub paused: bool,
    /// The speed it runs at or resumes to, in layout units per second.
    pub speed: f64,
}

/// A group `Once` run whose picture has left every window that draws it (ADR-0185):
/// reported once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupScrollFinished {
    /// The canvas.
    pub canvas: CanvasId,
    /// The originating scene (a held copy's original).
    pub scene: SceneId,
    /// The group.
    pub group: GroupId,
    /// The run's public id.
    pub run_id: u64,
}

/// Which occurrence of a group failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupOccurrence {
    /// A program compositor.
    Program,
    /// The preview scene.
    Preview,
    /// The incoming scene of a transition.
    Incoming,
    /// The outgoing scene of a transition.
    Outgoing,
    /// A hold scene.
    Hold,
}

/// Why a group occurrence is drawn transparent (ADR-0193 decisions 4 and 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupFailureReason {
    /// Its window is not finite or has no area.
    InvalidBounds,
    /// Its window is over a per-intermediate limit.
    TooLarge,
    /// Its compositor's span cap was taken by groups below it.
    Capped,
    /// Its compositor could not get its intermediate.
    Allocation,
    /// Its compositor failed its pass.
    Render,
}

/// A group occurrence drawn transparent: runtime health of the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupFailure {
    /// Which occurrence.
    pub occurrence: GroupOccurrence,
    /// Why.
    pub reason: GroupFailureReason,
}

/// An outgoing occurrence's group settings and run, captured with its transition's
/// snapshot (spec "Snapshots"): later edits do not re-key it.
pub(super) type Pin = (GroupMark, GroupRunKey);

/// The pin of `m` (same canvas, scene and group).
fn pin_of<'a>(m: &GroupMark, pins: &'a [Pin]) -> Option<&'a Pin> {
    pins.iter()
        .find(|(p, _)| (p.canvas, p.scene, p.group) == (m.canvas, m.scene, m.group))
}

/// Puts the pinned settings on the marks of `layers` (a manual transition's live
/// outgoing layers); windows and refusals stay live.
pub(super) fn pin_marks(layers: &mut [BoundLayer], pins: &[Pin]) {
    for m in layers.iter_mut().filter_map(|l| l.group.as_mut()) {
        if let Some((p, _)) = pin_of(m, pins) {
            m.settings = p.settings;
            m.scroll = p.scroll;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    run: ScrollRun,
    /// Canvas pixels per layout unit along the direction.
    scale: f64,
    /// The speed a resume eases back to, canvas pixels per second.
    speed: f64,
    /// The saved speed in canvas pixels per second, where a restart starts.
    saved: f64,
    paused: bool,
    ramp: Duration,
    id: u64,
    /// The tag of its spans (ADR-0193 decision 5): its first id, kept when the public
    /// id changes, so the failure slots of lists already sent still name it.
    tag: u64,
    /// Its `Once` end was reported.
    finished: bool,
}

/// The allocators of group Scroll settings generations and public run ids (ADR-0187
/// decisions 1 and 3). The studio host owns one for its lifetime and gives it to every
/// studio it starts ([`super::StudioSettings::group_ids`]), so no value is issued twice
/// across studio restarts and collection switches. Clones share the counters.
#[derive(Debug, Clone, Default)]
pub struct GroupIds(Arc<[AtomicU64; 2]>);

impl PartialEq for GroupIds {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl GroupIds {
    /// The next value of counter `i`, from 1; `None` once exhausted (never wraps).
    fn take(&self, i: usize) -> Option<u64> {
        let old = self.0[i]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        old.checked_add(1)
    }

    fn generation(&self) -> Option<u64> {
        self.take(0)
    }

    fn run(&self) -> Option<u64> {
        self.take(1)
    }
}

/// The group Scroll runs of every list, their settings generations and the scenes
/// held copies stand for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupRuns {
    runs: BTreeMap<GroupRunKey, Run>,
    /// By canvas, bound scene and group: the settings last seen and their generation.
    versions: BTreeMap<(CanvasId, SceneId, GroupId), (GroupScroll, u64)>,
    origins: BTreeMap<SceneId, SceneId>,
    ids: GroupIds,
    /// `Once` ends not taken yet ([`Studio::take_group_scroll_finished`]).
    finished: Vec<GroupScrollFinished>,
}

impl GroupRuns {
    /// The keys with a run.
    pub fn keys(&self) -> impl Iterator<Item = &GroupRunKey> {
        self.runs.keys()
    }

    /// The run of `key`.
    pub fn get(&self, key: &GroupRunKey) -> Option<&ScrollRun> {
        self.runs.get(key).map(|r| &r.run)
    }

    fn origin(&self, scene: SceneId) -> SceneId {
        self.origins.get(&scene).copied().unwrap_or(scene)
    }

    /// The run key of a mark, once [`Self::keep`] has seen it.
    pub fn key(&self, mark: &GroupMark) -> Option<GroupRunKey> {
        let (_, generation) = self.versions.get(&(mark.canvas, mark.scene, mark.group))?;
        Some(GroupRunKey {
            canvas: mark.canvas,
            scene: self.origin(mark.scene),
            group: mark.group,
            generation: *generation,
        })
    }

    /// The run key of a mark: its pin's, else [`Self::key`].
    fn pinned_key(&self, mark: &GroupMark, pins: &[Pin]) -> Option<GroupRunKey> {
        pin_of(mark, pins).map(|p| p.1).or_else(|| self.key(mark))
    }

    /// The pins of a snapshot's `layers`, taken now.
    pub(super) fn pins(&self, layers: &[BoundLayer]) -> Vec<Pin> {
        let mut out: Vec<Pin> = Vec::new();
        for m in layers.iter().filter_map(|l| l.group) {
            if pin_of(&m, &out).is_none()
                && let Some(key) = self.key(&m)
            {
                out.push((m, key));
            }
        }
        out
    }

    /// The span's Scroll and tag of a mark with a span and a run.
    fn scroll(&self, mark: &GroupMark, pins: &[Pin]) -> Option<(LayerScroll, u64)> {
        let key = self
            .pinned_key(mark, pins)
            .filter(|_| mark.refused.is_none())?;
        let run = self.runs.get(&key)?;
        Some((layer_scroll(&mark.scroll, run.run), run.tag))
    }

    /// Issues generations for the groups `marks` draw and keeps exactly their runs and
    /// those of `pins`: a new key starts a run at `now`, an unlisted one ends. Pinned
    /// keys are kept as they are, never re-derived. Held copies resolve before the
    /// scenes they stand for, so a copy taken in the same update as an edit keeps the
    /// settings' old generation.
    fn keep<'a>(
        &mut self,
        marks: impl Iterator<Item = &'a GroupMark>,
        pins: &'a [Pin],
        now: Timestamp,
    ) {
        let mut seen: BTreeMap<(bool, CanvasId, SceneId, GroupId), &GroupMark> = BTreeMap::new();
        for m in marks {
            let held = self.origins.contains_key(&m.scene);
            seen.entry((!held, m.canvas, m.scene, m.group)).or_insert(m);
        }
        if seen.is_empty() && pins.is_empty() && self.runs.is_empty() && self.versions.is_empty() {
            return;
        }
        let mut versions = BTreeMap::new();
        for (&(_, canvas, scene, group), m) in &seen {
            let origin = self.origin(scene);
            let copied = (origin != scene)
                .then(|| self.versions.get(&(canvas, origin, group)))
                .flatten();
            let generation = match self
                .versions
                .get(&(canvas, scene, group))
                .or(copied)
                .copied()
            {
                Some((settings, g)) if settings == m.settings => g,
                // Exhausted: the group has no run (drawn transparent).
                _ => match self.ids.generation() {
                    Some(g) => g,
                    None => continue,
                },
            };
            versions.insert((canvas, scene, group), (m.settings, generation));
        }
        self.versions = versions;
        let mut wanted: BTreeMap<GroupRunKey, &GroupMark> = seen
            .values()
            .filter(|m| m.refused.is_none())
            .filter_map(|m| Some((self.key(m)?, *m)))
            .collect();
        for (m, key) in pins {
            wanted.entry(*key).or_insert(m);
        }
        self.runs.retain(|k, _| wanted.contains_key(k));
        for (key, m) in wanted {
            if !self.runs.contains_key(&key)
                && let Some(id) = self.ids.run()
            {
                self.runs.insert(key, Run::start(m, now, id));
            }
        }
    }

    /// These runs with the host's allocators and held-copy mapping of a starting
    /// studio, before its first bind.
    pub(super) fn starting(self, ids: &GroupIds, origins: &BTreeMap<SceneId, SceneId>) -> Self {
        Self {
            ids: ids.clone(),
            origins: origins.clone(),
            ..self
        }
    }

    /// A fresh public id for every run; a run is dropped once ids are exhausted. Span
    /// tags are kept, so lists already sent still attribute failures.
    fn renew_ids(&mut self) {
        let ids = self.ids.clone();
        self.runs.retain(|_, r| match ids.run() {
            Some(id) => {
                r.id = id;
                true
            }
            None => false,
        });
    }

    /// Every run moved to a new clock (a studio restart, ADR-0185).
    pub(super) fn moved(&self, now_old: Timestamp, now_new: Timestamp) -> Self {
        let mut out = self.clone();
        for r in out.runs.values_mut() {
            r.run = r.run.moved(now_old, now_new);
        }
        out
    }
}

impl Run {
    fn start(m: &GroupMark, now: Timestamp, id: u64) -> Self {
        let speed = f64::from(m.scroll.speed);
        Self {
            run: ScrollRun::start(now, speed),
            scale: speed / f64::from(m.settings.speed),
            speed,
            saved: speed,
            paused: false,
            ramp: Duration::from_millis(u64::from(m.settings.ramp_ms)),
            id,
            tag: id,
            finished: false,
        }
    }

    /// A live call (ADR-0150 decision 11); `Restart` is a new incarnation `id`.
    fn control(&mut self, control: ScrollControl, now: Timestamp, id: u64) {
        let target = match control {
            ScrollControl::Restart => {
                *self = Self {
                    run: ScrollRun::start(now, self.saved),
                    paused: false,
                    speed: self.saved,
                    id,
                    finished: false,
                    ..*self
                };
                return;
            }
            ScrollControl::Pause => {
                self.paused = true;
                0.0
            }
            ScrollControl::Resume => {
                self.paused = false;
                self.speed
            }
            ScrollControl::Speed(s) => {
                self.speed = s * self.scale;
                if self.paused {
                    return;
                }
                self.speed
            }
        };
        self.run = self.run.change_speed(now, target, self.ramp);
    }
}

/// The node's list for `bound`: `layers` (from [`node_layers`], maybe glided, so in
/// the order of [`resolve`]) with a span for each group run of members that has a
/// span and a run. Members of a group without one are left out: drawn transparent,
/// never separately (ADR-0193 decision 5). Without group marks the list has no span.
pub(super) fn with_spans(
    bound: &BoundCompositor,
    plan: &SchedulePlan,
    layers: Vec<NodeLayer>,
    groups: &GroupRuns,
    pins: &[Pin],
) -> LayerList {
    if bound.layers.iter().all(|l| l.group.is_none()) {
        return LayerList {
            layers,
            spans: Vec::new(),
        };
    }
    let marks: Vec<Option<&GroupMark>> = resolve(bound, plan)
        .into_iter()
        .map(|(_, l)| l.group.as_ref())
        .collect();
    let mut out = LayerList {
        layers: Vec::with_capacity(layers.len()),
        spans: Vec::new(),
    };
    // A layer without a run is left out by `node_layers`; then no member is drawn.
    let aligned = marks.len() == layers.len();
    let mut last: Option<(SceneId, GroupId)> = None;
    for (i, layer) in layers.into_iter().enumerate() {
        let mark = marks.get(i).copied().flatten();
        let Some(m) = mark else {
            out.layers.push(layer);
            last = None;
            continue;
        };
        let Some((scroll, tag)) = groups.scroll(m, pins).filter(|_| aligned) else {
            continue;
        };
        let at = out.layers.len() as u32;
        match out.spans.last_mut() {
            Some(s) if last == Some((m.scene, m.group)) && s.first + s.count == at => s.count += 1,
            _ => out.spans.push(GroupSpan {
                first: at,
                count: 1,
                window: m.window,
                scroll,
                offset: [0.0; 2],
                slide: None,
                tag,
            }),
        }
        last = Some((m.scene, m.group));
        out.layers.push(layer);
    }
    out
}

/// [`with_spans`] of [`node_layers`].
pub(super) fn node_list(
    bound: &BoundCompositor,
    plan: &SchedulePlan,
    runs: &ScrollRuns,
    groups: &GroupRuns,
) -> LayerList {
    pinned_list(bound, plan, runs, groups, &[])
}

/// [`node_list`] of an outgoing snapshot, its groups keyed by `pins`.
pub(super) fn pinned_list(
    bound: &BoundCompositor,
    plan: &SchedulePlan,
    runs: &ScrollRuns,
    groups: &GroupRuns,
    pins: &[Pin],
) -> LayerList {
    with_spans(bound, plan, node_layers(bound, plan, runs), groups, pins)
}

/// `list` followed by `more`.
pub(super) fn joined(mut list: LayerList, more: LayerList) -> LayerList {
    let at = list.layers.len() as u32;
    list.spans.extend(more.spans.into_iter().map(|s| GroupSpan {
        first: s.first + at,
        ..s
    }));
    list.layers.extend(more.layers);
    list
}

/// Inserts `layer` at `at`, moving spans above it up; a span it lands inside is split
/// in two that scroll alike.
pub(super) fn insert(list: &mut LayerList, at: usize, layer: NodeLayer) {
    let at32 = at as u32;
    let mut split = None;
    for s in &mut list.spans {
        if s.first >= at32 {
            s.first += 1;
        } else if s.first + s.count > at32 {
            let above = s.first + s.count - at32;
            s.count -= above;
            split = Some(GroupSpan {
                first: at32 + 1,
                count: above,
                ..*s
            });
        }
    }
    list.spans.extend(split);
    list.spans.sort_by_key(|s| s.first);
    list.layers.insert(at, layer);
}

/// The span limits of `gpu` (ADR-0193 decision 4).
pub(super) fn span_limits(gpu: &rbs_gpu::GpuContext) -> SpanLimits {
    let side = match gpu
        .capabilities()
        .get(&rbs_gpu::context::CAP_MAX_TEXTURE_DIMENSION)
    {
        Some(rbs_core::capability::CapabilityValue::Int(v)) => u32::try_from(*v).unwrap_or(0),
        _ => 0,
    };
    SpanLimits {
        max_side: side,
        max_pixels: rbs_gpu::node::MAX_SPAN_PIXELS,
        max_spans: rbs_gpu::node::MAX_SPANS,
        max_total_pixels: rbs_gpu::node::MAX_SPANS_PIXELS,
    }
}

impl Studio {
    /// Maps each held copy's scene id to the scene it copies (ADR-0185 decision 4,
    /// ADR-0063): its groups then share runs with the original's. Call before the
    /// update that adds the copy; an id without a scene in the collection is ignored.
    pub fn set_scene_origins(&mut self, origins: BTreeMap<SceneId, SceneId>) {
        self.group_runs.origins = origins;
    }

    /// Retires every group run, generation and held-copy mapping (a collection switch,
    /// ADR-0187 decision 3) and returns them, for [`Self::restore_group_runs`] when the
    /// switch fails. The allocators go on: later runs get fresh generations and ids.
    pub fn retire_group_runs(&mut self) -> GroupRuns {
        let empty = GroupRuns {
            ids: self.group_runs.ids.clone(),
            ..GroupRuns::default()
        };
        std::mem::replace(&mut self.group_runs, empty)
    }

    /// Puts back the runs [`Self::retire_group_runs`] returned.
    pub fn restore_group_runs(&mut self, runs: GroupRuns) {
        self.group_runs = runs;
    }

    /// Gives every live group run a fresh public id: a new API server lifetime
    /// (ADR-0187 decision 3). Phase, pause and speed are unchanged.
    pub fn renew_group_run_ids(&mut self) {
        self.group_runs.renew_ids();
    }

    /// The group Scroll run of every group span a list draws; the
    /// [`super::ScrollHandover`] hands them to the studio that replaces this one.
    pub fn group_runs(&self) -> &GroupRuns {
        &self.group_runs
    }

    /// Issues generations and keeps the runs of every group span the lists draw:
    /// outgoing snapshots by their pins.
    pub(super) fn keep_group_runs(&mut self) {
        let now = self.engine.clock().now();
        let marks: Vec<GroupMark> = self.live_layers().iter().filter_map(|l| l.group).collect();
        let pins = self.transition_pins();
        self.group_runs.keep(marks.iter(), &pins, now);
    }

    /// Every group mark drawn with its run key: live lists and outgoing pins.
    fn keyed_marks(&self) -> Vec<(GroupMark, GroupRunKey)> {
        let live = self.live_layers().into_iter().filter_map(|l| l.group);
        let mut out: Vec<_> = live
            .filter_map(|m| Some((m, self.group_runs.key(&m)?)))
            .collect();
        out.extend(self.transition_pins());
        out
    }

    /// Ends of group `Once` runs since the last call, oldest first.
    pub fn take_group_scroll_finished(&mut self) -> Vec<GroupScrollFinished> {
        std::mem::take(&mut self.group_runs.finished)
    }

    /// Reports group `Once` runs whose picture has left every window drawing them, and
    /// returns how long until the next one may. Nothing to do without a group run.
    pub(super) fn service_group_scroll(&mut self) -> Option<Duration> {
        if self.group_runs.runs.is_empty() {
            return None;
        }
        let now = self.engine.clock().now();
        let marks: Vec<(GroupMark, GroupRunKey)> = self
            .keyed_marks()
            .into_iter()
            .filter(|(m, _)| m.refused.is_none() && m.scroll.repeat == ScrollRepeat::Once)
            .collect();
        let mut due: Option<Duration> = None;
        let mut ended = Vec::new();
        for (key, r) in self.group_runs.runs.iter().filter(|(_, r)| !r.finished) {
            // The span's picture is its window, so the distance is the window's.
            let Some(distance) = marks
                .iter()
                .filter(|(_, k)| k == key)
                .map(|(m, _)| {
                    let up = matches!(
                        m.scroll.direction,
                        ScrollDirection::Up | ScrollDirection::Down
                    );
                    let len = f64::from(if up { m.window[3] } else { m.window[2] });
                    once_distance(&m.scroll, len, len)
                })
                .reduce(f64::max)
            else {
                continue;
            };
            let at = r.run.position_at(now);
            if at >= distance {
                ended.push(*key);
                continue;
            }
            let wait = if now < r.run.ramp_end() {
                Some(r.run.ramp_end().saturating_since(now))
            } else if r.run.to > 0.0 {
                Some(Duration::from_secs_f64(
                    ((distance - at) / r.run.to).max(0.001),
                ))
            } else {
                None
            };
            if let Some(w) = wait {
                due = Some(due.map_or(w, |d| d.min(w)));
            }
        }
        for key in ended {
            if let Some(r) = self.group_runs.runs.get_mut(&key) {
                r.finished = true;
                self.group_runs.finished.push(GroupScrollFinished {
                    canvas: key.canvas,
                    scene: key.scene,
                    group: key.group,
                    run_id: r.id,
                });
            }
        }
        due
    }

    /// The edited scene's current run of `group` on `canvas`.
    fn edited_run(&self, scene: SceneId, group: GroupId, canvas: CanvasId) -> Option<GroupRunKey> {
        let (_, generation) = self.group_runs.versions.get(&(canvas, scene, group))?;
        let key = GroupRunKey {
            canvas,
            scene,
            group,
            generation: *generation,
        };
        self.group_runs.runs.contains_key(&key).then_some(key)
    }

    /// The live run of the edited scene's `group` on `canvas`, if one is drawn.
    pub fn group_run(
        &self,
        scene: SceneId,
        group: GroupId,
        canvas: CanvasId,
    ) -> Option<GroupRunState> {
        let r = self
            .group_runs
            .runs
            .get(&self.edited_run(scene, group, canvas)?)?;
        Some(GroupRunState {
            id: r.id,
            paused: r.paused,
            speed: r.speed / r.scale,
        })
    }

    /// A live call on the edited scene's group run (ADR-0185, ADR-0150 decision 11):
    /// not saved, not undone, and no member's run changes. `Speed` is in layout units
    /// per second. Every list is sent again.
    pub fn group_scroll_control(
        &mut self,
        scene: SceneId,
        group: GroupId,
        canvas: CanvasId,
        control: ScrollControl,
    ) -> Result<(), ScrollRefused> {
        self.collection
            .scene(scene)
            .and_then(|s| s.groups.iter().find(|g| g.id == group))
            .and_then(|g| g.scrolls.get(&canvas))
            .ok_or(ScrollRefused::NoScroll)?;
        let key = self
            .edited_run(scene, group, canvas)
            .ok_or(ScrollRefused::NoRun)?;
        let now = self.engine.clock().now();
        let id = match control {
            ScrollControl::Restart => self.group_runs.ids.run().ok_or(ScrollRefused::NoRun)?,
            _ => 0,
        };
        if let Some(run) = self.group_runs.runs.get_mut(&key) {
            run.control(control, now, id);
        }
        let plan = self.graph.plan();
        self.send_program_layers(&plan);
        Ok(())
    }

    /// The occurrences of `group` of `scene` (its held copies included) drawn
    /// transparent on `canvas`, and why: its runtime health (ADR-0193 decision 5).
    pub fn group_render_failures(
        &self,
        scene: SceneId,
        group: GroupId,
        canvas: CanvasId,
    ) -> Vec<GroupFailure> {
        use GroupOccurrence as O;
        let mut lists: Vec<(O, &[BoundLayer], Option<&CompositorPort>)> = Vec::new();
        if canvas == self.settings.canvas {
            lists.push((O::Program, &self.bound.layers, Some(&self.layers)));
            if let Some(sp) = &self.scene_preview {
                lists.push((O::Preview, &sp.bound.layers, Some(&sp.layers)));
            }
        }
        for e in self.extras.iter().filter(|e| e.canvas == canvas) {
            lists.push((O::Program, &e.view.bound.layers, Some(&e.view.layers)));
        }
        if let Some(v) = self.hold_scenes.get(&canvas).and_then(|h| h.view.as_ref()) {
            lists.push((O::Hold, &v.bound.layers, Some(&v.layers)));
        }
        let mut pins: &[Pin] = &[];
        if let Some(t) = self.transitions.get(&canvas) {
            // The program compositor of the canvas draws the outgoing snapshot.
            let program = lists.iter().find(|l| l.0 == O::Program).and_then(|l| l.2);
            lists.push((O::Outgoing, &t.from, program));
            pins = &t.pins;
            if let Some(v) = &t.incoming {
                lists.push((O::Incoming, &v.bound.layers, Some(&v.layers)));
            }
        }
        let mut out = Vec::new();
        for (occurrence, layers, port) in lists {
            let Some(m) = layers.iter().filter_map(|l| l.group).find(|m| {
                (m.canvas, m.group, self.group_runs.origin(m.scene)) == (canvas, group, scene)
            }) else {
                continue;
            };
            let pins = if occurrence == O::Outgoing { pins } else { &[] };
            let tag = self
                .group_runs
                .pinned_key(&m, pins)
                .and_then(|k| self.group_runs.runs.get(&k))
                .map(|r| r.tag);
            let reason = match m.refused {
                Some(SpanRefusal::InvalidBounds) => GroupFailureReason::InvalidBounds,
                Some(SpanRefusal::TooLarge) => GroupFailureReason::TooLarge,
                Some(_) => GroupFailureReason::Capped,
                None => match port.zip(tag).and_then(|(p, tag)| p.span_failure(tag)) {
                    Some(reason) => reason,
                    None => continue,
                },
            };
            out.push(GroupFailure { occurrence, reason });
        }
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    #[test]
    fn renewed_public_ids_keep_the_span_tag_of_lists_already_sent() {
        let key = GroupRunKey {
            canvas: CanvasId::from_raw(1),
            scene: SceneId::from_raw(2),
            group: GroupId::from_raw(3),
            generation: 1,
        };
        let mut runs = GroupRuns::default();
        let id = runs.ids.run().unwrap();
        let run = Run {
            run: ScrollRun::start(Timestamp::from_nanos(0), 10.0),
            scale: 1.0,
            speed: 10.0,
            saved: 10.0,
            paused: false,
            ramp: Duration::ZERO,
            id,
            tag: id,
            finished: false,
        };
        runs.runs.insert(key, run);

        runs.renew_ids();

        let renewed = runs.runs[&key];
        assert_ne!(renewed.id, id, "a new public id");
        assert_eq!(renewed.tag, id, "the tag of the lists already sent");
    }
}
