//! What each built-in filter does to known pictures, and the node's memory behaviour
//! (spec `video-filters.md`, "Tests outside the crate").
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::Arc;

use rbs_filters::catalogue::{BLUR, CHROMA_KEY, COLOR_CORRECTION, SHARPEN, find};
use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{FilterType, Value, Values};
use rbs_gpu::GpuContext;

fn ty(id: &str) -> Arc<dyn FilterType> {
    common::shared(find(id).unwrap())
}

fn values(id: &str, pairs: &[(&str, Value)]) -> Values {
    Values::from_pairs(find(id).unwrap(), pairs.iter().copied()).unwrap()
}

fn srgb_to_linear(e: f32) -> f32 {
    if e <= 0.04045 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Opaque colours that vary across the picture, linear.
fn gradient(w: u32, h: u32) -> Picture {
    let mut p = Picture::solid(w, h, [0.0; 4]);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / (w - 1).max(1) as f32;
            let fy = y as f32 / (h - 1).max(1) as f32;
            p.pixels[(y * w + x) as usize] = [0.05 + 0.9 * fx, 0.2 + 0.6 * fy, 0.5, 1.0];
        }
    }
    p
}

/// Runs one picture through a chain and returns what came out.
fn run(ctx: Arc<GpuContext>, chain: Vec<(Arc<dyn FilterType>, Values)>, input: Picture) -> Picture {
    let size = [input.width, input.height];
    let mut rig = Rig::chain(ctx, chain, Some(size)).unwrap();
    rig.set_input(input);
    rig.tick();
    rig.tick();
    let (p, _, _) = rig.output().expect("an output");
    assert_eq!([p.width, p.height], size);
    p
}

fn assert_close(got: &Picture, want: impl Fn(u32, u32) -> [f32; 4], tol: f32) {
    for y in 0..got.height {
        for x in 0..got.width {
            let (g, w) = (got.at(x, y), want(x, y));
            assert!(
                g.iter().zip(w).all(|(a, b)| (a - b).abs() <= tol),
                "({x}, {y}): got {g:?}, want {w:?}"
            );
        }
    }
}

#[test]
fn colour_correction_defaults_change_nothing() {
    let Some(ctx) = common::gpu() else { return };
    let input = gradient(16, 8);
    let want = input.clone();
    let got = run(
        ctx,
        vec![(ty(COLOR_CORRECTION), values(COLOR_CORRECTION, &[]))],
        input,
    );
    assert_close(&got, |x, y| want.at(x, y), 4e-3);
}

#[test]
fn brightness_adds_to_the_encoded_value() {
    let Some(ctx) = common::gpu() else { return };
    let input = gradient(16, 8);
    let want = input.clone();
    let v = values(COLOR_CORRECTION, &[("brightness", Value::Number(0.1))]);
    let got = run(ctx, vec![(ty(COLOR_CORRECTION), v)], input);
    let f = |l: f32| srgb_to_linear((linear_to_srgb(l) + 0.1).clamp(0.0, 1.0));
    assert_close(
        &got,
        |x, y| {
            let p = want.at(x, y);
            [f(p[0]), f(p[1]), f(p[2]), 1.0]
        },
        5e-3,
    );
}

#[test]
fn colour_correction_decodes_srgb_input() {
    let Some(ctx) = common::gpu() else { return };
    let input = Picture::solid(4, 4, [0.5, 0.5, 0.5, 1.0]).encoded();
    let got = run(
        ctx,
        vec![(ty(COLOR_CORRECTION), values(COLOR_CORRECTION, &[]))],
        input,
    );
    let g = srgb_to_linear(128.0 / 255.0);
    assert_close(&got, |_, _| [g, g, g, 1.0], 4e-3);
}

#[test]
fn chroma_key_removes_the_key_colour_only() {
    let Some(ctx) = common::gpu() else { return };
    let mut input = Picture::solid(4, 2, [0.0, 1.0, 0.0, 1.0]);
    for x in 0..4 {
        input.pixels[(4 + x) as usize] = [0.8, 0.1, 0.1, 1.0];
    }
    let got = run(ctx, vec![(ty(CHROMA_KEY), values(CHROMA_KEY, &[]))], input);
    for x in 0..4 {
        assert!(got.at(x, 0)[3] < 1e-3, "green kept: {:?}", got.at(x, 0));
        assert!(got.at(x, 1)[3] > 0.999, "red keyed: {:?}", got.at(x, 1));
    }
}

#[test]
fn blur_keeps_a_uniform_colour_and_spreads_a_dot() {
    let Some(ctx) = common::gpu() else { return };
    let c = [0.3, 0.6, 0.1, 1.0];
    let got = run(
        ctx.clone(),
        vec![(ty(BLUR), values(BLUR, &[]))],
        Picture::solid(33, 17, c),
    );
    assert_close(&got, |_, _| c, 3e-3);

    let mut dot = Picture::solid(32, 32, [0.0; 4]);
    for (x, y) in [(15, 15), (16, 15), (15, 16), (16, 16)] {
        dot.pixels[y * 32 + x] = [1.0; 4];
    }
    let v = values(BLUR, &[("radius", Value::Number(8.0))]);
    let got = run(ctx, vec![(ty(BLUR), v)], dot);
    let centre = got.at(16, 16)[3];
    assert!(centre > 0.0 && centre < 1.0, "centre {centre}");
    assert!(got.at(20, 16)[3] > 0.0, "nothing spread");
    assert!(got.at(0, 0)[3] < centre, "corner as bright as the centre");
}

#[test]
fn sharpen_at_zero_changes_nothing() {
    let Some(ctx) = common::gpu() else { return };
    let input = gradient(16, 8);
    let want = input.clone();
    let v = values(SHARPEN, &[("amount", Value::Number(0.0))]);
    let got = run(ctx, vec![(ty(SHARPEN), v)], input);
    assert_close(&got, |x, y| want.at(x, y), 2e-3);
}

#[test]
fn a_chain_applies_filters_in_order() {
    let Some(ctx) = common::gpu() else { return };
    let bright = values(COLOR_CORRECTION, &[("brightness", Value::Number(0.1))]);
    let alone = run(
        ctx.clone(),
        vec![(ty(COLOR_CORRECTION), bright)],
        gradient(16, 8),
    );
    let chained = run(
        ctx,
        vec![
            (ty(COLOR_CORRECTION), bright),
            (
                ty(SHARPEN),
                values(SHARPEN, &[("amount", Value::Number(0.0))]),
            ),
        ],
        gradient(16, 8),
    );
    assert_close(&chained, |x, y| alone.at(x, y), 3e-3);
}

#[test]
fn a_disabled_filter_gives_its_memory_back() {
    let Some(ctx) = common::gpu() else { return };
    let mut rig = Rig::new(ctx, ty(BLUR), values(BLUR, &[]), Some([64, 32])).unwrap();
    rig.set_input(gradient(64, 32));
    rig.tick();
    rig.tick();
    let with = rig.allocated();
    rig.disable_filter().unwrap();
    for _ in 0..=rbs_gpu::node::POOL_IDLE_TICKS + 1 {
        rig.tick();
    }
    let without = rig.allocated();
    // Only the feed's texture is left.
    assert_eq!(without, 1, "with the filter: {with}");
}

#[test]
fn a_disabled_filter_gives_its_memory_back_when_the_canvas_shares_its_size() {
    let Some(ctx) = common::gpu() else { return };
    let size = [64, 32];
    let mut rig =
        Rig::chain_with_canvas(ctx, vec![(ty(BLUR), values(BLUR, &[]))], Some(size), size).unwrap();
    rig.set_input(gradient(size[0], size[1]));
    rig.tick();
    rig.tick();
    let with = rig.allocated();
    rig.disable_filter().unwrap();
    for _ in 0..4 {
        rig.tick();
    }
    // The feed's texture and the canvas user's one; the filter's two outputs and its
    // intermediate are gone although the canvas acquires their description every tick.
    assert_eq!(rig.allocated(), 2, "with the filter: {with}");
}

#[test]
fn a_resize_sweep_keeps_memory_to_one_size() {
    let Some(ctx) = common::gpu() else { return };
    let chain = vec![
        (ty(COLOR_CORRECTION), values(COLOR_CORRECTION, &[])),
        (ty(BLUR), values(BLUR, &[])),
    ];
    let mut rig = Rig::chain(ctx, chain, Some([100, 60])).unwrap();
    rig.set_input(gradient(100, 60));
    rig.tick();
    rig.tick();
    let steady = rig.allocated();
    let mut most = steady;
    // A window dragged for two seconds: a new size every frame.
    for i in 1..=120u32 {
        let size = [100 + i, 60 + i / 2];
        rig.set_input(Picture::solid(size[0], size[1], [0.5, 0.5, 0.5, 1.0]));
        rig.tick();
        let (p, _, _) = rig.output().expect("an output on every frame");
        assert_eq!([p.width, p.height], size);
        most = most.max(rig.allocated());
        assert!(rig.filter_health().is_some_and(|(e, b)| e == 0 && !b));
    }
    for (_, stats) in &rig.chain {
        assert_eq!(stats.resizes(), 120);
    }
    // At most one frame's worth of old textures is still held by readers.
    assert!(most <= steady + 4, "steady {steady}, most {most}");
    rig.tick();
    assert!(
        rig.allocated() <= steady + 1,
        "steady {steady}, after {}",
        rig.allocated()
    );
}
