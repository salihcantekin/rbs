//! A manual transition's picture at a held progress (ADR-0135 decision 3) and a push
//! Swipe's outgoing list.

use rbs_gpu::compositor::{LayerShape, LayerStyle};
use rbs_gpu::node::{LayerList, Slide};
use rbs_graph::description::NodeId;
use rbs_scene::GlideEasing;

use super::{Blend, easing, shifted, swipe_offset};

/// The outgoing `list` pushed by `by` canvas pixels, each moved box given the slide
/// `slide(from)` makes: a group span moves as one picture, its members stay put in its
/// intermediate, so it is not drawn again as it moves.
pub(super) fn pushed(
    list: &mut LayerList,
    by: (f32, f32),
    slide: impl Fn(rbs_gpu::math::Affine2) -> Option<Slide>,
) {
    let spans = &mut list.spans;
    let member = |i: usize| {
        spans
            .iter()
            .any(|s| (s.first as usize..(s.first + s.count) as usize).contains(&i))
    };
    for (i, l) in list.layers.iter_mut().enumerate() {
        if !member(i) {
            let at = l.transform;
            l.transform = shifted(at, by);
            l.slide = slide(at).or(l.slide);
        }
    }
    for s in spans.iter_mut() {
        let [x, y, w, h] = s.window;
        let at = rbs_gpu::math::Affine2::rect(x + s.offset[0], y + s.offset[1], w, h);
        s.offset = [s.offset[0] + by.0, s.offset[1] + by.1];
        s.slide = slide(at).or(s.slide);
    }
}

/// The incoming canvas over the outgoing `layers`, frozen at progress `t` (ADR-0135
/// decision 3): what the timed transition shows at `t` times its duration. Returns the
/// layers to draw above `layers`, and moves `layers` out for a Slide.
pub(super) fn frozen_blend(
    compositor: NodeId,
    (w, h): (u32, u32),
    blend: Blend,
    ease: GlideEasing,
    t: f32,
    outgoing: &mut LayerList,
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
                pushed(outgoing, (-dx * gone, -dy * gone), |_| None);
            }
        }
    }
    out.push(top);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::time::Timestamp;
    use rbs_scene::SwipeDirection;

    #[test]
    fn a_frozen_blend_shows_what_the_timed_one_shows_at_t() {
        let node = super::super::tests::any_node();
        let size = (1920, 1080);
        let lin = GlideEasing::Linear;
        // A crossfade: the incoming opacity is e(t).
        let top = frozen_blend(
            node,
            size,
            Blend::Fade { via: None },
            lin,
            0.3,
            &mut LayerList::default(),
        );
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
        let first = frozen_blend(node, size, via, lin, 0.25, &mut LayerList::default());
        assert!((first[0].opacity - 0.5).abs() < 1e-6);
        assert_eq!(first[1].opacity, 0.0);
        let second = frozen_blend(node, size, via, lin, 0.75, &mut LayerList::default());
        assert_eq!(second[0].opacity, 1.0);
        assert!((second[1].opacity - 0.5).abs() < 1e-6);
        // A Slide at half way: the incoming canvas half in, the outgoing half out. A
        // group span moves as one picture; its member stays put in its intermediate.
        let layer = || rbs_gpu::node::NodeLayer::new(node, 0, 0.0, 0.0, 1920.0, 1080.0);
        let mut out = LayerList {
            layers: vec![layer(), layer()],
            spans: vec![rbs_gpu::node::GroupSpan {
                first: 1,
                count: 1,
                window: [0.0, 0.0, 1920.0, 1080.0],
                scroll: rbs_gpu::LayerScroll {
                    direction: rbs_gpu::ScrollDirection::Left,
                    repeat: rbs_gpu::ScrollRepeat::Loop,
                    start: rbs_gpu::ScrollStart::Inside,
                    gap: 0.0,
                    fade: 0.0,
                    run: rbs_gpu::ScrollRun::start(Timestamp::ZERO, 0.0),
                },
                offset: [0.0; 2],
                slide: None,
                tag: 0,
            }],
        };
        let before = out.layers[0].transform.tx;
        let slide = Blend::Swipe {
            direction: SwipeDirection::Left,
            push: true,
        };
        let top = frozen_blend(node, size, slide, lin, 0.5, &mut out);
        assert!((top[0].transform.tx - 960.0).abs() < 1e-3);
        assert!((out.layers[0].transform.tx - (before - 960.0)).abs() < 1e-3);
        assert_eq!(
            out.layers[1].transform.tx, before,
            "the member is not moved"
        );
        assert!((out.spans[0].offset[0] + 960.0).abs() < 1e-3);
    }
}
