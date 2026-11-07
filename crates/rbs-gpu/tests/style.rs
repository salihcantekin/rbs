//! Golden image tests for styled layers (ADR-0053): shapes, borders, antialiasing under
//! rotation, flips and non-uniform scale, crop clamping, and plain layers unchanged.
//! Spec: docs/specs/gpu-compositor.md, "Conformance tests".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, at, f32_to_f16, solid};
use rbs_gpu::{
    Affine2, CanvasDesc, Compositor, Layer, LayerShape, LayerStyle, TextureDesc, TextureId,
    TexturePool, UvRect, readback,
};

const W: u32 = 64;
const H: u32 = 64;
const TOL: f32 = 0.01;
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const NONE: [f32; 4] = [0.0; 4];

fn render(
    build: impl FnOnce(&rbs_gpu::GpuContext, &mut TexturePool) -> Vec<Layer>,
) -> Option<Vec<[f32; 4]>> {
    let ctx = common::gpu()?;
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        8,
    )
    .unwrap();
    let target = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let layers = build(ctx, &mut pool);
    comp.render(ctx, &pool, &layers, target).unwrap();
    Some(readback::read_rgba16f(ctx, &pool, target).unwrap())
}

fn styled(texture: TextureId, transform: Affine2, shape: LayerShape, box_size: [f32; 2]) -> Layer {
    let mut layer = Layer::new(texture, 0.0, 0.0, 1.0, 1.0);
    layer.transform = transform;
    layer.style = LayerStyle::new(shape, 0.0, [0; 4], box_size);
    layer
}

fn red(ctx: &rbs_gpu::GpuContext, pool: &mut TexturePool) -> TextureId {
    solid(ctx, pool, 4, 4, RED)
}

#[test]
fn rounded_corners_cut_the_corners_only() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![styled(
            t,
            Affine2::rect(0.0, 0.0, 64.0, 64.0),
            LayerShape::Rounded { radius: 16.0 },
            [64.0, 64.0],
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 1, 1), NONE, TOL);
    assert_close(at(&px, W, 62, 62), NONE, TOL);
    assert_close(at(&px, W, 32, 0), RED, TOL);
    assert_close(at(&px, W, 0, 32), RED, TOL);
    assert_close(at(&px, W, 32, 32), RED, TOL);
}

#[test]
fn a_circle_is_centred_in_a_wide_box() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![styled(
            t,
            Affine2::rect(0.0, 0.0, 64.0, 32.0),
            LayerShape::Circle,
            [64.0, 32.0],
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 32, 16), RED, TOL);
    assert_close(at(&px, W, 32, 2), RED, TOL);
    assert_close(at(&px, W, 40, 16), RED, TOL);
    // Inside the box, outside the circle.
    assert_close(at(&px, W, 5, 16), NONE, TOL);
    assert_close(at(&px, W, 60, 16), NONE, TOL);
}

#[test]
fn an_ellipse_fills_the_box() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![styled(
            t,
            Affine2::rect(0.0, 0.0, 64.0, 32.0),
            LayerShape::Ellipse,
            [64.0, 32.0],
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 5, 16), RED, TOL);
    assert_close(at(&px, W, 32, 1), RED, TOL);
    assert_close(at(&px, W, 2, 2), NONE, TOL);
    assert_close(at(&px, W, 61, 29), NONE, TOL);
}

#[test]
fn a_half_transparent_border_shows_the_picture_beneath() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        let mut layer = Layer::new(t, 0.0, 0.0, 64.0, 64.0);
        layer.style = LayerStyle::new(LayerShape::Box, 8.0, [255, 255, 255, 128], [64.0, 64.0]);
        vec![layer]
    }) else {
        return;
    };
    let a = 128.0 / 255.0;
    // Border over red: white * a + red * (1 - a).
    assert_close(at(&px, W, 2, 32), [1.0, a, a, 1.0], TOL);
    assert_close(at(&px, W, 32, 61), [1.0, a, a, 1.0], TOL);
    assert_close(at(&px, W, 32, 32), RED, TOL);
    assert_close(at(&px, W, 9, 32), RED, TOL);
}

#[test]
fn opacity_multiplies_shape_and_border() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        let mut layer = Layer::new(t, 0.0, 0.0, 64.0, 64.0);
        layer.style = LayerStyle::new(LayerShape::Box, 4.0, [0, 0, 255, 255], [64.0, 64.0]);
        layer.opacity = 0.5;
        vec![layer]
    }) else {
        return;
    };
    assert_close(at(&px, W, 1, 32), [0.0, 0.0, 0.5, 0.5], TOL);
    assert_close(at(&px, W, 32, 32), [0.5, 0.0, 0.0, 0.5], TOL);
}

#[test]
fn a_rotated_circle_stays_round() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        let transform = Affine2::rect(-16.0, -16.0, 32.0, 32.0)
            .then(&Affine2::rotation(std::f32::consts::FRAC_PI_4))
            .then(&Affine2::translation(32.0, 32.0));
        vec![styled(t, transform, LayerShape::Circle, [32.0, 32.0])]
    }) else {
        return;
    };
    assert_close(at(&px, W, 32, 32), RED, TOL);
    assert_close(at(&px, W, 32, 18), RED, TOL);
    assert_close(at(&px, W, 45, 32), RED, TOL);
    // Inside the rotated box's corner reach, outside the circle.
    assert_close(at(&px, W, 32, 11), NONE, TOL);
    assert_close(at(&px, W, 19, 19), NONE, TOL);
}

#[test]
fn a_flipped_layer_keeps_its_rounded_corners() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![styled(
            t,
            Affine2::rect(64.0, 0.0, -64.0, 64.0),
            LayerShape::Rounded { radius: 16.0 },
            [64.0, 64.0],
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 1, 1), NONE, TOL);
    assert_close(at(&px, W, 62, 1), NONE, TOL);
    assert_close(at(&px, W, 32, 32), RED, TOL);
}

#[test]
fn a_squashed_circle_follows_the_picture_with_a_one_pixel_edge() {
    // A circle in its own 64x64 box, placed 4:1 squashed as a nested scene would: on
    // the canvas it is the 64x16 ellipse the picture fills. Its top edge is at 24.25,
    // so the pixel row 24 (centre 24.5) is a quarter pixel inside: 0.75 coverage. A
    // mean pixel width would give 1.0 here.
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![styled(
            t,
            Affine2::rect(0.0, 24.25, 64.0, 16.0),
            LayerShape::Circle,
            [64.0, 64.0],
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 4, 32), RED, TOL);
    assert_close(at(&px, W, 32, 30), RED, TOL);
    assert_close(at(&px, W, 2, 26), NONE, TOL);
    let edge = at(&px, W, 32, 24);
    assert!((edge[3] - 0.75).abs() < 0.08, "edge coverage {edge:?}");
}

/// Left half blue, right half red, cropped to the red half, placed over the canvas.
fn half_red(ctx: &rbs_gpu::GpuContext, pool: &mut TexturePool, shape: LayerShape) -> Layer {
    let id = pool
        .acquire(
            ctx,
            &TextureDesc::source(4, 1, wgpu::TextureFormat::Rgba16Float),
        )
        .unwrap();
    let blue = [0.0, 0.0, 1.0, 1.0];
    let data: Vec<u8> = [blue, blue, RED, RED]
        .iter()
        .flat_map(|c| c.iter().flat_map(|v| f32_to_f16(*v).to_le_bytes()))
        .collect();
    readback::upload(ctx, pool, id, &data).unwrap();
    let mut layer = styled(id, Affine2::rect(0.0, 0.0, 64.0, 64.0), shape, [64.0, 64.0]);
    layer.crop = UvRect {
        u0: 0.5,
        v0: 0.0,
        u1: 1.0,
        v1: 1.0,
    };
    layer
}

#[test]
fn a_cropped_styled_layer_samples_what_a_plain_one_does() {
    // The grown quad must not reach past the crop: inside the shape, a styled layer's
    // pixels equal the plain layer's, also on the edge column, where bilinear filtering
    // already blends the texel beyond the crop line.
    let Some(plain) = render(|ctx, pool| vec![half_red(ctx, pool, LayerShape::Box)]) else {
        return;
    };
    let Some(px) =
        render(|ctx, pool| vec![half_red(ctx, pool, LayerShape::Rounded { radius: 8.0 })])
    else {
        return;
    };
    for (x, y) in [(0, 32), (1, 32), (32, 32), (63, 32), (32, 0)] {
        assert_close(at(&px, W, x, y), at(&plain, W, x, y), TOL);
    }
}

#[test]
fn plain_layers_around_styled_ones_draw_as_before() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        let blue = solid(ctx, pool, 2, 2, [0.0, 0.0, 1.0, 1.0]);
        vec![
            Layer::new(blue, 0.0, 0.0, 64.0, 64.0),
            styled(
                t,
                Affine2::rect(0.0, 0.0, 64.0, 64.0),
                LayerShape::Circle,
                [64.0, 64.0],
            ),
            Layer::new(blue, 0.0, 0.0, 8.0, 8.0),
        ]
    }) else {
        return;
    };
    assert_close(at(&px, W, 1, 1), [0.0, 0.0, 1.0, 1.0], TOL);
    assert_close(at(&px, W, 62, 62), [0.0, 0.0, 1.0, 1.0], TOL);
    assert_close(at(&px, W, 32, 32), RED, TOL);
}

#[test]
fn boxes_without_area_draw_nothing() {
    let Some(px) = render(|ctx, pool| {
        let t = red(ctx, pool);
        vec![
            styled(
                t,
                Affine2::rect(0.0, 0.0, 64.0, 64.0),
                LayerShape::Circle,
                [0.0, 64.0],
            ),
            styled(
                t,
                Affine2::rect(0.0, 0.0, 0.0, 0.0),
                LayerShape::Circle,
                [64.0, 64.0],
            ),
        ]
    }) else {
        return;
    };
    // The first is plain (a zero local size draws as the box would); the second has no
    // area on the canvas.
    assert_close(at(&px, W, 32, 32), RED, TOL);
    assert_close(at(&px, W, 1, 1), RED, TOL);
}

#[test]
fn a_crop_that_leaves_no_picture_draws_nothing_plain_or_styled() {
    // ADR-0057: a reversed or empty crop (a window that shrank under its crop) is
    // skipped, not drawn as a mirrored strip.
    for shape in [LayerShape::Box, LayerShape::Rounded { radius: 8.0 }] {
        let Some(px) = render(|ctx, pool| {
            let mut layer = half_red(ctx, pool, shape);
            layer.crop = UvRect {
                u0: 0.75,
                v0: 0.0,
                u1: 0.25,
                v1: 1.0,
            };
            let mut empty = half_red(ctx, pool, shape);
            empty.crop.u1 = empty.crop.u0;
            vec![layer, empty]
        }) else {
            return;
        };
        for (x, y) in [(2, 2), (32, 32), (60, 60)] {
            assert_close(at(&px, W, x, y), NONE, TOL);
        }
    }
}
