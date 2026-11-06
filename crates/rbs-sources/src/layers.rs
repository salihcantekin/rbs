//! Bound compositor layers to the compositor node's layer list.

use rbs_gpu::compositor::{LayerShape, LayerStyle};
use rbs_gpu::math::Affine2;
use rbs_gpu::node::{NodeLayer, PixelCrop};
use rbs_graph::description::SchedulePlan;
use rbs_scene::binder::{BoundCompositor, resolve};
use rbs_scene::model::{ItemStyle, Shape};

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
            out.style = layer_style(&layer.style, layer.box_size);
            out
        })
        .collect()
}

/// The compositor's form of an item's style (ADR-0053): the border colour in linear
/// light, premultiplied. The shadow is not drawn yet.
pub fn layer_style(style: &ItemStyle, box_size: [f32; 2]) -> LayerStyle {
    let shape = match style.shape {
        Shape::Rectangle { radius } if radius > 0.0 => LayerShape::Rounded { radius },
        Shape::Circle => LayerShape::Circle,
        Shape::Ellipse => LayerShape::Ellipse,
        _ => LayerShape::Box,
    };
    let (width, color) = style.border.map_or((0.0, [0; 4]), |b| (b.width, b.color));
    LayerStyle::new(shape, width, color, box_size)
}
