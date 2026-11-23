//! Bound compositor layers to the compositor node's layer list.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_gpu::compositor::{LayerShape, LayerStyle};
use rbs_gpu::math::Affine2;
use rbs_gpu::node::{Easing, Fade, NodeLayer, PixelCrop, Slide};
use rbs_graph::description::{NodeId, SchedulePlan};
use rbs_scene::binder::{BoundCompositor, BoundLayer, resolve};
use rbs_scene::model::{CollectionGlide, GlideEasing, ItemStyle, Shape, SourceGlide, SourceId};

use crate::scroll::{ScrollRuns, layer_scroll};

/// The layer list for the compositor node of `compositor`, bottom first, with link
/// hints for `plan` (pass the plan the runtime runs, `ApplyReport::plan`). Layers
/// whose producer is not scheduled are left out (scene model spec, "Graph binding").
/// A scrolling layer moves by its run in `runs` (ADR-0150); one whose key has no run
/// is left out too, which the host must not let happen (a debug assertion).
pub fn node_layers(
    compositor: &BoundCompositor,
    plan: &SchedulePlan,
    runs: &ScrollRuns,
) -> Vec<NodeLayer> {
    resolve(compositor, plan)
        .into_iter()
        .filter_map(|(link, layer)| {
            let scroll = match (&layer.scroll, &layer.scroll_key) {
                (Some(scroll), Some(key)) => {
                    let Some(run) = runs.get(key) else {
                        debug_assert!(false, "no scroll run for {key:?}");
                        return None;
                    };
                    Some(layer_scroll(scroll, *run))
                }
                _ => None,
            };
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
            out.crop = pixel_crop(layer.crop);
            out.opacity = layer.opacity;
            out.style = layer_style(&layer.style, layer.box_size);
            out.scroll = scroll;
            Some(out)
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

/// The scene crop as the compositor's crop.
fn pixel_crop(c: rbs_scene::model::Crop) -> PixelCrop {
    PixelCrop {
        left: c.left,
        top: c.top,
        right: c.right,
        bottom: c.bottom,
    }
}

/// True when a source in both scenes moves: its box or its crop changes.
fn moves(old: &BoundLayer, new: &BoundLayer) -> bool {
    old.transform != new.transform || old.crop != new.crop
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

/// Moves a source that was already on the program to its new box and crop, and fades
/// in a source that was not. `current` is the compositor's layer list; `layers` is that
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
            Some(old) if moves(old, layer) => {
                let (duration, easing) = resolve_slide(collection, source_glide(layer.source));
                out.slide = Some(Slide {
                    from: gpu_affine(old.transform),
                    start,
                    duration,
                    easing,
                    // The crop moves with the box (ADR-0128 review, owner 2026-11-23).
                    from_crop: (old.crop != layer.crop).then(|| pixel_crop(old.crop)),
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
        if !moves(old, layer) {
            continue;
        }
        if let Some(g) = source_glide(layer.source) {
            max_ms = max_ms.max(g.duration_ms);
        }
    }
    Duration::from_millis(u64::from(max_ms))
}

/// Puts each layer that leaves on a Move where the outgoing scene stacked it
/// (ADR-0128 review, owner 2026-11-23): just below the incoming layer of the nearest source above it in `from`
/// that the incoming scene also draws, or on top when no such source is above it. So a
/// leaving background fades out under a picture that slides, and a leaving overlay
/// fades out over it. `layers` are the incoming layers; `current` names the source and
/// producer of each incoming bound layer; `from` names the source of each outgoing
/// layer, bottom first; `leaving` pairs the index in `from` of each leaving layer,
/// bottom first, with its layer.
pub fn stack_leaving(
    mut layers: Vec<NodeLayer>,
    current: &[(SourceId, NodeId)],
    from: &[SourceId],
    leaving: Vec<(usize, NodeLayer)>,
) -> Vec<NodeLayer> {
    for (index, layer) in leaving {
        let anchor = from
            .iter()
            .skip(index + 1)
            .find_map(|above| current.iter().find(|(source, _)| source == above))
            .and_then(|(_, producer)| layers.iter().position(|l| l.producer == *producer));
        match anchor {
            Some(at) => layers.insert(at, layer),
            None => layers.push(layer),
        }
    }
    layers
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

    fn nodes(n: usize) -> Vec<NodeId> {
        let mut graph = rbs_graph::description::GraphDescription::new();
        (0..n)
            .map(|_| {
                graph
                    .add_node(std::sync::Arc::new(rbs_gpu::node::compositor_descriptor()))
                    .expect("an empty graph takes nodes")
            })
            .collect()
    }

    fn order(layers: &[NodeLayer]) -> Vec<NodeId> {
        layers.iter().map(|l| l.producer).collect()
    }

    #[test]
    fn a_leaving_background_fades_under_the_picture_that_slides() {
        // Screen_Cam [Dots, Display, Camera] to Full Screen [Display]: Dots stays
        // below the display, the camera (above it before) fades out on top.
        let n = nodes(3);
        let (dots, display, camera) = (n[0], n[1], n[2]);
        let (s_dots, s_display, s_camera) = (
            SourceId::from_raw(10),
            SourceId::from_raw(2),
            SourceId::from_raw(1),
        );
        let layer = |p| NodeLayer::new(p, 0, 0.0, 0.0, 1.0, 1.0);
        let out = stack_leaving(
            vec![layer(display)],
            &[(s_display, display)],
            &[s_dots, s_display, s_camera],
            vec![(0, layer(dots)), (2, layer(camera))],
        );
        assert_eq!(order(&out), vec![dots, display, camera]);
    }

    #[test]
    fn leaving_layers_keep_their_order_under_one_shared_source() {
        // Screen_Cam [Dots, Display, Camera] to Full Cam [Camera, new overlay]: both
        // leaving layers go below the camera in their own order; the overlay that
        // fades in stays above.
        let n = nodes(4);
        let (dots, display, camera, overlay) = (n[0], n[1], n[2], n[3]);
        let layer = |p| NodeLayer::new(p, 0, 0.0, 0.0, 1.0, 1.0);
        let out = stack_leaving(
            vec![layer(camera), layer(overlay)],
            &[
                (SourceId::from_raw(1), camera),
                (SourceId::from_raw(5), overlay),
            ],
            &[
                SourceId::from_raw(10),
                SourceId::from_raw(2),
                SourceId::from_raw(1),
            ],
            vec![(0, layer(dots)), (1, layer(display))],
        );
        assert_eq!(order(&out), vec![dots, display, camera, overlay]);
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
