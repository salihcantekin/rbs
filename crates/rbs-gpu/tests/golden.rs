//! Golden image tests for the compositor: sampled pixels compared with tolerance.
//! Spec: docs/specs/gpu-compositor.md, "Conformance tests".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, at, solid};
use rbs_gpu::{
    Affine2, CanvasDesc, Compositor, GpuContext, GpuError, GpuRequest, Layer, TextureDesc,
    TexturePool, Transfer, UvRect, readback,
};

const W: u32 = 64;
const H: u32 = 64;
const TOL: f32 = 0.01;

fn setup() -> Option<(
    &'static rbs_gpu::GpuContext,
    TexturePool,
    Compositor,
    rbs_gpu::TextureId,
)> {
    let ctx = common::gpu()?;
    let mut pool = TexturePool::new();
    let comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        8,
    )
    .unwrap();
    let target = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    Some((ctx, pool, comp, target))
}

#[test]
fn opaque_layer_is_placed_and_rest_is_cleared() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let red = solid(ctx, &mut pool, 4, 4, [1.0, 0.0, 0.0, 1.0]);
    let stats = comp
        .render(ctx, &pool, &[Layer::new(red, 0.0, 0.0, 32.0, 64.0)], target)
        .unwrap();
    assert_eq!(stats.drawn_layers, 1);
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 10, 10), [1.0, 0.0, 0.0, 1.0], TOL);
    assert_close(at(&px, W, 31, 63), [1.0, 0.0, 0.0, 1.0], TOL);
    assert_close(at(&px, W, 32, 10), [0.0; 4], TOL);
    assert_close(at(&px, W, 60, 60), [0.0; 4], TOL);
}

#[test]
fn later_layers_draw_on_top() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let blue = solid(ctx, &mut pool, 2, 2, [0.0, 0.0, 1.0, 1.0]);
    let layers = [
        Layer::new(red, 0.0, 0.0, 64.0, 64.0),
        Layer::new(blue, 16.0, 16.0, 32.0, 32.0),
    ];
    comp.render(ctx, &pool, &layers, target).unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 32, 32), [0.0, 0.0, 1.0, 1.0], TOL);
    assert_close(at(&px, W, 4, 4), [1.0, 0.0, 0.0, 1.0], TOL);
}

#[test]
fn opacity_blends_over_background() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let white = solid(ctx, &mut pool, 2, 2, [1.0, 1.0, 1.0, 1.0]);
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let mut top = Layer::new(red, 0.0, 0.0, 64.0, 64.0);
    top.opacity = 0.5;
    comp.render(
        ctx,
        &pool,
        &[Layer::new(white, 0.0, 0.0, 64.0, 64.0), top],
        target,
    )
    .unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 32, 32), [1.0, 0.5, 0.5, 1.0], TOL);
}

#[test]
fn premultiplied_translucent_source_over_transparent() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let half_green = solid(ctx, &mut pool, 2, 2, [0.0, 0.5, 0.0, 0.5]);
    comp.render(
        ctx,
        &pool,
        &[Layer::new(half_green, 0.0, 0.0, 64.0, 64.0)],
        target,
    )
    .unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 32, 32), [0.0, 0.5, 0.0, 0.5], TOL);
}

#[test]
fn crop_selects_source_region() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    // 8x8 source: left half red, right half green.
    let src = pool
        .acquire(
            ctx,
            &TextureDesc::source(8, 8, wgpu::TextureFormat::Rgba8Unorm),
        )
        .unwrap();
    let data: Vec<u8> = (0..64)
        .flat_map(|i| {
            if i % 8 < 4 {
                [255, 0, 0, 255]
            } else {
                [0, 255, 0, 255]
            }
        })
        .collect();
    readback::upload(ctx, &pool, src, &data).unwrap();
    let mut layer = Layer::new(src, 0.0, 0.0, 64.0, 64.0);
    layer.crop = UvRect::from_pixels(4, 0, 4, 8, 8, 8);
    comp.render(ctx, &pool, &[layer], target).unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 32, 32), [0.0, 1.0, 0.0, 1.0], TOL);
    assert_close(at(&px, W, 60, 4), [0.0, 1.0, 0.0, 1.0], TOL);
}

#[test]
fn srgb_sources_are_decoded_to_linear() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let grey = [128u8, 128, 128, 255];
    let expected = ((128.0f32 / 255.0 + 0.055) / 1.055).powf(2.4);

    // Shader decode: sRGB values stored in a UNORM texture.
    let unorm = pool
        .acquire(
            ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8Unorm),
        )
        .unwrap();
    readback::upload(ctx, &pool, unorm, &grey.repeat(4)).unwrap();
    let mut a = Layer::new(unorm, 0.0, 0.0, 32.0, 64.0);
    a.source_transfer = Transfer::Srgb;

    // Sampler decode: an sRGB texture format with `Transfer::Linear`.
    let srgb = pool
        .acquire(
            ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8UnormSrgb),
        )
        .unwrap();
    readback::upload(ctx, &pool, srgb, &grey.repeat(4)).unwrap();
    let b = Layer::new(srgb, 32.0, 0.0, 32.0, 64.0);

    comp.render(ctx, &pool, &[a, b], target).unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    let want = [expected, expected, expected, 1.0];
    assert_close(at(&px, W, 16, 32), want, TOL);
    assert_close(at(&px, W, 48, 32), want, TOL);
}

#[test]
fn rotated_layer_covers_rotated_area() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    // A 32x8 bar rotated a quarter turn about its origin, then moved to (40, 8):
    // it covers x in 32..40, y in 8..40.
    let mut layer = Layer::new(red, 0.0, 0.0, 1.0, 1.0);
    layer.transform = Affine2::rect(0.0, 0.0, 32.0, 8.0)
        .then(&Affine2::rotation(std::f32::consts::FRAC_PI_2))
        .then(&Affine2::translation(40.0, 8.0));
    comp.render(ctx, &pool, &[layer], target).unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 36, 20), [1.0, 0.0, 0.0, 1.0], TOL);
    assert_close(at(&px, W, 36, 44), [0.0; 4], TOL);
    assert_close(at(&px, W, 20, 12), [0.0; 4], TOL);
}

#[test]
fn stale_layer_is_skipped_and_counted() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let gone = solid(ctx, &mut pool, 2, 2, [0.0, 1.0, 0.0, 1.0]);
    pool.release(gone).unwrap();
    let layers = [
        Layer::new(red, 0.0, 0.0, 64.0, 64.0),
        Layer::new(gone, 0.0, 0.0, 64.0, 64.0),
    ];
    let stats = comp.render(ctx, &pool, &layers, target).unwrap();
    assert_eq!(stats.drawn_layers, 1);
    assert_eq!(stats.skipped_layers, 1);
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 32, 32), [1.0, 0.0, 0.0, 1.0], TOL);
}

#[test]
fn too_many_layers_is_an_error_until_reserved() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let layers = vec![Layer::new(red, 0.0, 0.0, 64.0, 64.0); 9];
    assert_eq!(
        comp.render(ctx, &pool, &layers, target),
        Err(GpuError::TooManyLayers {
            requested: 9,
            reserved: 8
        })
    );
    comp.reserve_layers(ctx, 16).unwrap();
    assert_eq!(
        comp.render(ctx, &pool, &layers, target)
            .unwrap()
            .drawn_layers,
        9
    );
}

#[test]
fn wrong_target_is_rejected() {
    let Some((ctx, mut pool, mut comp, _target)) = setup() else {
        return;
    };
    let small = pool.acquire(ctx, &TextureDesc::canvas(32, 32)).unwrap();
    assert_eq!(
        comp.render(ctx, &pool, &[], small),
        Err(GpuError::TargetMismatch(small))
    );
    let src = solid(ctx, &mut pool, W, H, [0.0; 4]);
    assert_eq!(
        comp.render(ctx, &pool, &[], src),
        Err(GpuError::TargetMismatch(src))
    );
}

#[test]
fn readback_is_counted() {
    // Own context: the shared one is used by tests running in parallel, which read back.
    let Some(_) = common::gpu() else { return };
    let ctx: &'static GpuContext = Box::leak(Box::new(
        GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        })
        .unwrap(),
    ));
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let target = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    assert_eq!(ctx.metrics().frames_cpu_copied(), 0);
    comp.render(ctx, &pool, &[], target).unwrap();
    assert_eq!(
        ctx.metrics().frames_cpu_copied(),
        0,
        "render alone never copies to the CPU"
    );
    readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_eq!(ctx.metrics().frames_cpu_copied(), 1);
}

#[test]
fn opaque_layer_ignores_undefined_source_alpha() {
    let Some((ctx, mut pool, mut comp, target)) = setup() else {
        return;
    };
    // An XRGB-style source: colour red, fourth byte 0.
    let xrgb = pool
        .acquire(
            ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8Unorm),
        )
        .unwrap();
    readback::upload(ctx, &pool, xrgb, &[255u8, 0, 0, 0].repeat(4)).unwrap();
    let mut layer = Layer::new(xrgb, 0.0, 0.0, W as f32, 64.0);
    layer.opaque = true;
    comp.render(ctx, &pool, &[layer], target).unwrap();
    let px = readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_close(at(&px, W, 16, 16), [1.0, 0.0, 0.0, 1.0], TOL);
}
