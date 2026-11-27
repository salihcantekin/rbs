//! The program layers of a Move while its outgoing sources fade out (ADR-0128
//! decision 5 and its owner note of 2026-11-23).

use std::collections::BTreeSet;

use rbs_core::time::Timestamp;
use rbs_gpu::node::{Fade, LayerList};
use rbs_graph::description::{NodeId, SchedulePlan};
use rbs_scene::binder::{BoundCompositor, BoundLayer};
use rbs_scene::model::SourceId;
use rbs_sources::node_layers;

use super::super::group_scroll::{insert, joined, node_list};
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
    ) -> LayerList {
        let mut current = program.clone();
        current.layers.retain(|l| !is_global(l));
        let drawn: BTreeSet<SourceId> = current.layers.iter().map(|l| l.source).collect();
        let mut list = paint_layers(
            &current,
            plan,
            self.glide.as_ref(),
            glide.now,
            glide.canvas,
            (&self.scroll_runs, &self.group_runs),
        );
        let from: Vec<&BoundLayer> = from.iter().filter(|l| !is_global(l)).collect();
        let mut one = program.clone();
        let mut leaving = Vec::new();
        for (index, layer) in from.iter().enumerate() {
            // A leaving group member is not drawn on its own (ADR-0193 decision 5).
            if drawn.contains(&layer.source) || layer.group.is_some() {
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
        // As `rbs_sources::stack_leaving`, keeping the spans.
        for (index, layer) in leaving {
            let anchor = order
                .iter()
                .skip(index + 1)
                .find_map(|above| shared.iter().find(|(source, _)| source == above))
                .and_then(|(_, p)| list.layers.iter().position(|l| l.producer == *p));
            let at = anchor.unwrap_or(list.layers.len());
            insert(&mut list, at, layer);
        }
        let mut above = program.clone();
        above.layers.retain(|l| is_global(l));
        joined(
            list,
            node_list(&above, plan, &self.scroll_runs, &self.group_runs),
        )
    }
}
