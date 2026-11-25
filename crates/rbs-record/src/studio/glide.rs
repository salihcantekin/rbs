//! The glide of a program change (ADR-0100): a source in both scenes slides to its new
//! box and a source only in the new scene fades in, for as long as the longest slide.

use std::collections::BTreeMap;

use rbs_core::time::Timestamp;
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BoundCompositor, BoundLayer};
use rbs_scene::model::{CanvasId, SceneId};
use rbs_sources::{glide_layers, glide_max_duration, node_layers};

use super::group_scroll::{GroupRuns, with_spans};
use super::{Studio, program_scene, transition};
use rbs_gpu::node::LayerList;
use rbs_sources::scroll::ScrollRuns;

/// Boxes to glide from when a canvas's program scene changes.
pub(super) struct Glide {
    /// Master-clock time the scene changed.
    start: Timestamp,
    /// The program layers each canvas was drawing.
    from: BTreeMap<CanvasId, Vec<BoundLayer>>,
    /// Longest morph still running (ADR-0100).
    max_duration: std::time::Duration,
    /// Collection glide used to build the morph.
    collection: rbs_scene::CollectionGlide,
    /// Per-source slide overrides snapshot at the scene change.
    source_glide: BTreeMap<rbs_scene::model::SourceId, rbs_scene::SourceGlide>,
}

pub(super) fn paint_layers(
    bound: &BoundCompositor,
    plan: &rbs_graph::description::SchedulePlan,
    glide: Option<&Glide>,
    now: Timestamp,
    canvas: CanvasId,
    (runs, groups): (&ScrollRuns, &GroupRuns),
) -> LayerList {
    let layers = node_layers(bound, plan, runs);
    let from = glide
        .filter(|g| now.saturating_since(g.start) < g.max_duration)
        .and_then(|g| Some((g, g.from.get(&canvas)?)));
    let Some((glide, from)) = from else {
        return with_spans(bound, plan, layers, groups, &[]);
    };
    let overrides = &glide.source_glide;
    let layers = glide_layers(
        layers,
        &bound.layers,
        from,
        glide.start,
        glide.collection,
        |id| overrides.get(&id).copied(),
    );
    with_spans(bound, plan, layers, groups, &[])
}

impl Studio {
    /// Starts a glide on each canvas whose program scene changed. A scene that was
    /// empty does not glide, so the first picture does not fade in from nothing.
    pub(super) fn note_scene_change(
        &mut self,
        collection: &SceneCollection,
        effective: &transition::Effective,
        old_main: Option<SceneId>,
        old_extra: &BTreeMap<CanvasId, Option<SceneId>>,
        old_layers: BTreeMap<CanvasId, Vec<BoundLayer>>,
    ) {
        // Only Move glides (ADR-0128 decision 2): Cut and the kinds that draw the
        // incoming picture whole change the boxes at once.
        if !effective.glides() {
            self.glide = None;
            return;
        }
        let mut from = BTreeMap::new();
        let new_main = program_scene(collection, self.settings.canvas);
        if old_main.is_some() && old_main != new_main {
            if let Some(layers) = old_layers.get(&self.settings.canvas) {
                from.insert(self.settings.canvas, layers.clone());
            }
        }
        for extra in &self.extras {
            let previous = old_extra.get(&extra.canvas).copied().flatten();
            let next = program_scene(collection, extra.canvas);
            if previous.is_some() && previous != next {
                if let Some(layers) = old_layers.get(&extra.canvas) {
                    from.insert(extra.canvas, layers.clone());
                }
            }
        }
        if from.is_empty() {
            return;
        }
        let collection_glide = effective.glide;
        // A one-off duration replaces every per-source override for this change.
        let source_glide: BTreeMap<_, _> = if effective.source_glides {
            collection
                .sources()
                .filter_map(|s| s.glide.map(|g| (s.id, g)))
                .collect()
        } else {
            BTreeMap::new()
        };
        // The longest per-source slide is known once `reconcile` has bound the new
        // layers (`refresh_glide_duration`); until then the collection's duration.
        let max_duration =
            std::time::Duration::from_millis(u64::from(collection_glide.duration_ms));
        self.glide = Some(Glide {
            start: self.engine.clock().now(),
            from,
            max_duration,
            collection: collection_glide,
            source_glide,
        });
    }

    /// When the glide of the latest program change ends: its start plus the longest
    /// slide, per-source overrides included (ADR-0100). `None` when no change glides.
    pub fn glide_end(&self) -> Option<Timestamp> {
        self.glide.as_ref().map(|g| g.start + g.max_duration)
    }

    /// Sets the glide's length from the layers bound for the new program: the
    /// collection duration and every source that actually slides (ADR-0100 decision
    /// 5). Called after `reconcile`, which binds them; before, `self.bound` still held
    /// the outgoing layers and a longer per-source slide was cut short.
    pub(super) fn refresh_glide_duration(&mut self) {
        let Some(glide) = &self.glide else {
            return;
        };
        let overrides = &glide.source_glide;
        let mut max = std::time::Duration::from_millis(u64::from(glide.collection.duration_ms));
        if let Some(old) = glide.from.get(&self.settings.canvas) {
            max = max.max(glide_max_duration(
                &self.bound.layers,
                old,
                glide.collection,
                |id| overrides.get(&id).copied(),
            ));
        }
        for extra in &self.extras {
            if let Some(old) = glide.from.get(&extra.canvas) {
                max = max.max(glide_max_duration(
                    &extra.view.bound.layers,
                    old,
                    glide.collection,
                    |id| overrides.get(&id).copied(),
                ));
            }
        }
        if let Some(glide) = &mut self.glide {
            glide.max_duration = max;
        }
    }
}
