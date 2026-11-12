//! Bound compositor layers to the compositor node's layer list.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::compositor::{LayerShape, LayerStyle};
use rbs_gpu::math::Affine2;
use rbs_gpu::node::{Easing, Fade, NodeLayer, PixelCrop, Slide};
use rbs_graph::description::SchedulePlan;
use rbs_scene::binder::{BoundCompositor, BoundLayer, resolve};
use rbs_scene::model::{CollectionGlide, GlideEasing, ItemStyle, Shape, SourceGlide, SourceId};

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

/// The default glide duration when none is configured (ADR-0100; kept for callers that
/// only need the shipped default).
pub const GLIDE: Duration = Duration::from_millis(400);

/// Maps a document easing to the compositor's (ADR-0100; `rbs-gpu` must not depend on
/// `rbs-scene`).
pub fn gpu_easing(e: GlideEasing) -> Easing {
    match e {
        GlideEasing::Linear => Easing::Linear,
        GlideEasing::EaseInOut => Easing::EaseInOut,
        _ => Easing::EaseInOut,
    }
}

/// Duration and slide easing for a source on this transition (ADR-0100).
pub fn resolve_slide(
    collection: CollectionGlide,
    override_: Option<SourceGlide>,
) -> (Duration, Easing) {
    match override_ {
        Some(g) => (
            Duration::from_millis(u64::from(g.duration_ms)),
            gpu_easing(g.easing),
        ),
        None => (
            Duration::from_millis(u64::from(collection.duration_ms)),
            gpu_easing(collection.easing),
        ),
    }
}

/// The scene transform as the compositor's transform.
fn gpu_affine(t: rbs_scene::Affine2) -> Affine2 {
    Affine2 {
        a: t.a,
        b: t.b,
        c: t.c,
        d: t.d,
        tx: t.tx,
        ty: t.ty,
    }
}

/// Moves a source that was already on the program to its new box, and fades in a
/// source that was not. `current` is the compositor's layer list; `layers` is that
/// list after producers that are not scheduled have been left out. `from` is the
/// program's layers before the scene change. Slides use the collection glide or a
/// per-source override; fade-ins use the collection duration and linear easing
/// (ADR-0100).
pub fn glide_layers(
    mut layers: Vec<NodeLayer>,
    current: &[BoundLayer],
    from: &[BoundLayer],
    start: Timestamp,
    collection: CollectionGlide,
    source_glide: impl Fn(SourceId) -> Option<SourceGlide>,
) -> Vec<NodeLayer> {
    let fade_duration = Duration::from_millis(u64::from(collection.duration_ms));
    for out in &mut layers {
        let Some(layer) = current.iter().find(|layer| layer.producer == out.producer) else {
            continue;
        };
        match from.iter().find(|old| old.source == layer.source) {
            Some(old) if old.transform != layer.transform => {
                let (duration, easing) = resolve_slide(collection, source_glide(layer.source));
                out.slide = Some(Slide {
                    from: gpu_affine(old.transform),
                    start,
                    duration,
                    easing,
                });
            }
            None => {
                out.fade = Some(Fade {
                    from: 0.0,
                    start,
                    duration: fade_duration,
                    easing: Easing::Linear,
                });
            }
            Some(_) => {}
        }
    }
    layers
}

/// Longest morph on this transition: the collection duration and every source that
/// actually slides (ADR-0100 decision 5).
pub fn glide_max_duration(
    current: &[BoundLayer],
    from: &[BoundLayer],
    collection: CollectionGlide,
    source_glide: impl Fn(SourceId) -> Option<SourceGlide>,
) -> Duration {
    let mut max_ms = collection.duration_ms;
    for layer in current {
        let Some(old) = from.iter().find(|old| old.source == layer.source) else {
            continue;
        };
        if old.transform == layer.transform {
            continue;
        }
        if let Some(g) = source_glide(layer.source) {
            max_ms = max_ms.max(g.duration_ms);
        }
    }
    Duration::from_millis(u64::from(max_ms))
}

/// The compositor's form of an item's style (ADR-0053): the border colour in linear
/// light, premultiplied. The shadow is not drawn yet.
pub fn layer_style(style: &ItemStyle, box_size: [f32; 2]) -> LayerStyle {
    let shape = match style.shape {
        Shape::Rectangle { radius } if radius > 0.0 => LayerShape::Rounded { radius },
        Shape::Circle => LayerShape::Circle,
        Shape::Ellipse => LayerShape::Ellipse,
        Shape::Triangle => LayerShape::Triangle,
        Shape::Arrow => LayerShape::Arrow,
        Shape::Rectangle { .. } => LayerShape::Box,
        // A shape this build does not draw yet stays a plain box.
        _ => LayerShape::Box,
    };
    let (width, color) = style.border.map_or((0.0, [0; 4]), |b| (b.width, b.color));
    LayerStyle::new(shape, width, color, box_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_slide_prefers_the_source_override() {
        let collection = CollectionGlide {
            duration_ms: 400,
            easing: GlideEasing::EaseInOut,
        };
        let (d, e) = resolve_slide(collection, None);
        assert_eq!(d, Duration::from_millis(400));
        assert_eq!(e, Easing::EaseInOut);
        let (d, e) = resolve_slide(
            collection,
            Some(SourceGlide {
                duration_ms: 800,
                easing: GlideEasing::Linear,
            }),
        );
        assert_eq!(d, Duration::from_millis(800));
        assert_eq!(e, Easing::Linear);
    }

    #[test]
    fn max_duration_without_slides_is_the_collection() {
        let collection = CollectionGlide::default();
        assert_eq!(
            glide_max_duration(&[], &[], collection, |_| None),
            Duration::from_millis(400)
        );
    }
}
