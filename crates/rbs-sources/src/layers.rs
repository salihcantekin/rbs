//! Bound compositor layers to the compositor node's layer list.

use rbs_gpu::math::Affine2;
use rbs_gpu::node::{NodeLayer, PixelCrop};
use rbs_graph::description::SchedulePlan;
use rbs_scene::binder::{BoundCompositor, resolve};

/// The layer list for the compositor node of `compositor`, bottom first, with link
/// hints for `plan` (pass the plan the runtime runs, `ApplyReport::plan`). Layers
/// whose producer is not scheduled are left out (scene model spec, "Graph binding").
pub fn node_layers(compositor: &BoundCompositor, plan: &SchedulePlan) -> Vec<NodeLayer> {
    resolve(compositor, plan)
        .into_iter()
        .map(|(link, layer)| {
            let t = layer.transform;
            let mut out = NodeLayer::new(layer.producer, link, 0.0, 0.0, 1.0, 1.0);
            out.transform = Affine2 {
                a: t.a,
                b: t.b,
                c: t.c,
                d: t.d,
                tx: t.tx,
                ty: t.ty,
            };
            out.crop = PixelCrop {
                left: layer.crop.left,
                top: layer.crop.top,
                right: layer.crop.right,
                bottom: layer.crop.bottom,
            };
            out.opacity = layer.opacity;
            out
        })
        .collect()
}
