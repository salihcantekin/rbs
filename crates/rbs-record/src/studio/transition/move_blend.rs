//! The program layers of a Move while its outgoing sources fade out (ADR-0128
//! decision 5 and its owner note of 2026-11-23).

use std::collections::BTreeSet;

use rbs_core::time::Timestamp;
use rbs_gpu::node::{Fade, NodeLayer};
use rbs_graph::description::{NodeId, SchedulePlan};
use rbs_scene::binder::{BoundCompositor, BoundLayer};
use rbs_scene::model::SourceId;
use rbs_sources::{node_layers, stack_leaving};

use super::super::{Studio, paint_layers};

/// When and where a Move runs.
pub(super) struct MoveGlide {
    pub(super) canvas: rbs_scene::model::CanvasId,
    pub(super) start: Timestamp,
    pub(super) duration: std::time::Duration,
    pub(super) now: Timestamp,
}

impl Studio {
    /// The incoming layers with their glide, the leaving ones fading out where the
    /// outgoing scene stacked them (owner, 2026-11-23: a leaving background must not
    /// cover a picture that slides), then the globals above everything. `from` is the
    /// outgoing scene's layers.
    pub(super) fn move_layers(
        &self,
        glide: &MoveGlide,
        plan: &SchedulePlan,
        program: &BoundCompositor,
        from: &[BoundLayer],
        is_global: &dyn Fn(&BoundLayer) -> bool,
    ) -> Vec<NodeLayer> {
        let mut current = program.clone();
        current.layers.retain(|l| !is_global(l));
        let drawn: BTreeSet<SourceId> = current.layers.iter().map(|l| l.source).collect();
        let layers = paint_layers(
            &current,
            plan,
            self.glide.as_ref(),
            glide.now,
            glide.canvas,
            &self.scroll_runs,
        );
        let from: Vec<&BoundLayer> = from.iter().filter(|l| !is_global(l)).collect();
        let mut one = program.clone();
        let mut leaving = Vec::new();
        for (index, layer) in from.iter().enumerate() {
            if drawn.contains(&layer.source) {
                continue;
            }
            one.layers.clear();
            one.layers.push((*layer).clone());
            for mut l in node_layers(&one, plan, &self.scroll_runs) {
                l.fade = Some(Fade {
                    from: l.opacity,
                    start: glide.start,
                    duration: glide.duration,
                    // As the glide's fade-ins (ADR-0100).
                    easing: rbs_gpu::node::Easing::Linear,
                });
                l.opacity = 0.0;
                leaving.push((index, l));
            }
        }
        let shared: Vec<(SourceId, NodeId)> = current
            .layers
            .iter()
            .map(|l| (l.source, l.producer))
            .collect();
        let order: Vec<SourceId> = from.iter().map(|l| l.source).collect();
        let mut layers = stack_leaving(layers, &shared, &order, leaving);
        let mut above = program.clone();
        above.layers.retain(|l| is_global(l));
        layers.extend(node_layers(&above, plan, &self.scroll_runs));
        layers
    }
}
