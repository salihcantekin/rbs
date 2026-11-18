//! Golden image tests for scrolling layers (ADR-0150): the window onto a strip of
//! copies, gaps, the seam between copies, `Once`, edge fade, vertical scrolls, styled
//! windows and the blank layer without a scroll binding.
//! Spec: docs/specs/gpu-compositor.md, "Scrolling layers".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::time::Duration;

use common::{assert_close, at, f32_to_f16};
use rbs_core::time::Timestamp;
use rbs_gpu::{
    Affine2, CanvasDesc, Compositor, GpuContext, Layer, LayerScroll, LayerShape, LayerStyle,
    RenderStats, ScrollBinding, ScrollDirection, ScrollRepeat, ScrollRun, ScrollStart, TextureDesc,
    TextureId, TexturePool, UvRect, readback,
};

const W: u32 = 64;
const H: u32 = 64;
const TOL: f32 = 0.01;
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const NONE: [f32; 4] = [0.0; 4];

/// A texture of `w x h` texels whose texel `(x, y)` is `colour(x, y)`.
fn texture(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    w: u32,
    h: u32,
    colour: impl Fn(u32, u32) -> [f32; 4],
) -> TextureId {
    let id = pool
        .acquire(
            ctx,
            &TextureDesc::source(w, h, wgpu::TextureFormat::Rgba16Float),
        )
        .unwrap();
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            for c in colour(x, y) {
                data.extend_from_slice(&f32_to_f16(c).to_le_bytes());
            }
        }
    }
    readback::upload(ctx, pool, id, &data).unwrap();
    id
}

/// Red in the first half along x, green in the second: 4 x 2 texels.
fn halves(ctx: &GpuContext, pool: &mut TexturePool) -> TextureId {
    texture(ctx, pool, 4, 2, |x, _| if x < 2 { RED } else { GREEN })
}

/// Red in the first half along y, green in the second: 2 x 4 texels.
fn halves_down(ctx: &GpuContext, pool: &mut TexturePool) -> TextureId {
    texture(ctx, pool, 2, 4, |_, y| if y < 2 { RED } else { GREEN })
}

fn s(secs: f64) -> Timestamp {
    Timestamp::from_nanos((secs * 1e9) as u64)
}

/// A scroll at 100 per second anchored at 10 s, evaluated at `10 + travelled / 100` s.
struct Scroll {
    direction: ScrollDirection,
    repeat: ScrollRepeat,
    start: ScrollStart,
    gap: f32,
    fade: f32,
    travelled: f64,
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            direction: ScrollDirection::Left,
            repeat: ScrollRepeat::Loop,
            start: ScrollStart::Inside,
            gap: 0.0,
            fade: 0.0,
            travelled: 0.0,
        }
    }
}

/// A layer of `texture` (`tw x th` texels) in `rect`, scrolling as `scroll` says.
fn scrolling(texture: TextureId, tw: u32, th: u32, rect: [f32; 4], scroll: Scroll) -> Layer {
    let [x, y, w, h] = rect;
    let mut layer = Layer::new(texture, x, y, w, h);
    let spec = LayerScroll {
        direction: scroll.direction,
        repeat: scroll.repeat,
        start: scroll.start,
        gap: scroll.gap,
        fade: scroll.fade,
        run: ScrollRun::start(s(10.0), 100.0),
    };
    let now = s(10.0 + scroll.travelled / 100.0);
    let frame = spec
        .frame(&layer.transform, &UvRect::FULL, tw, th, now)
        .expect("frame");
    layer.scroll = Some(frame.gpu(spec.direction, spec.repeat, spec.fade));
    layer
}

fn render_with(
    with_binding: bool,
    build: impl FnOnce(&GpuContext, &mut TexturePool) -> Vec<Layer>,
) -> Option<(Vec<[f32; 4]>, RenderStats)> {
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
    let mut binding = with_binding.then(|| ScrollBinding::new(ctx));
    let target = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let layers = build(ctx, &mut pool);
    let stats = comp
        .render_scrolled(ctx, &pool, &layers, target, binding.as_mut())
        .unwrap();
    Some((readback::read_rgba16f(ctx, &pool, target).unwrap(), stats))
}

fn render(
    build: impl FnOnce(&GpuContext, &mut TexturePool) -> Vec<Layer>,
) -> Option<Vec<[f32; 4]>> {
    render_with(true, build).map(|(px, _)| px)
}

/// A 64 x 16 window at the top: the 4 x 2 picture is 32 wide there.
const STRIP: [f32; 4] = [0.0, 0.0, 64.0, 16.0];

#[test]
fn a_loop_repeats_the_picture_with_a_clean_seam() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        vec![scrolling(t, 4, 2, STRIP, Scroll::default())]
    }) else {
        return;
    };
    assert_close(at(&px, W, 2, 8), RED, TOL);
    assert_close(at(&px, W, 20, 8), GREEN, TOL);
    // The seam: the last column of one copy and the first of the next do not blend.
    assert_close(at(&px, W, 31, 8), GREEN, TOL);
    assert_close(at(&px, W, 32, 8), RED, TOL);
    assert_close(at(&px, W, 56, 8), GREEN, TOL);
    // Below the window: nothing.
    assert_close(at(&px, W, 10, 30), NONE, TOL);
}

#[test]
fn a_left_scroll_moves_the_window_along_the_strip() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        vec![scrolling(
            t,
            4,
            2,
            STRIP,
            Scroll {
                travelled: 16.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    // 16 travelled: the window starts on the green half.
    assert_close(at(&px, W, 4, 8), GREEN, TOL);
    assert_close(at(&px, W, 20, 8), RED, TOL);
    assert_close(at(&px, W, 36, 8), GREEN, TOL);
}

#[test]
fn a_right_scroll_moves_the_other_way() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        vec![scrolling(
            t,
            4,
            2,
            STRIP,
            Scroll {
                direction: ScrollDirection::Right,
                travelled: 8.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    // The picture moved 8 right: the previous copy's green fills the first 8.
    assert_close(at(&px, W, 4, 8), GREEN, TOL);
    assert_close(at(&px, W, 12, 8), RED, TOL);
    assert_close(at(&px, W, 30, 8), GREEN, TOL);
}

#[test]
fn a_gap_is_transparent() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        vec![scrolling(
            t,
            4,
            2,
            STRIP,
            Scroll {
                gap: 32.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 4, 8), RED, TOL);
    assert_close(at(&px, W, 28, 8), GREEN, TOL);
    assert_close(at(&px, W, 36, 8), NONE, TOL);
    assert_close(at(&px, W, 60, 8), NONE, TOL);
}

#[test]
fn once_shows_a_single_copy_entering_from_outside() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        vec![scrolling(
            t,
            4,
            2,
            STRIP,
            Scroll {
                repeat: ScrollRepeat::Once,
                start: ScrollStart::Outside,
                travelled: 16.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    // The picture entered 16 from the right edge: only its red half shows, at the end.
    assert_close(at(&px, W, 10, 8), NONE, TOL);
    assert_close(at(&px, W, 44, 8), NONE, TOL);
    assert_close(at(&px, W, 52, 8), RED, TOL);
    assert_close(at(&px, W, 58, 8), RED, TOL);
}

#[test]
fn the_edge_fade_softens_both_ends() {
    let Some(px) = render(|ctx, pool| {
        let t = texture(ctx, pool, 4, 2, |_, _| RED);
        vec![scrolling(
            t,
            4,
            2,
            STRIP,
            Scroll {
                fade: 16.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    let edge = at(&px, W, 0, 8);
    assert!(edge[3] < 0.1, "left edge faded: {edge:?}");
    let quarter = at(&px, W, 8, 8);
    assert!((quarter[3] - 0.53).abs() < 0.05, "half way in: {quarter:?}");
    assert_close(at(&px, W, 32, 8), RED, TOL);
    let end = at(&px, W, 63, 8);
    assert!(end[3] < 0.1, "right edge faded: {end:?}");
}

#[test]
fn an_up_scroll_moves_along_the_height() {
    let Some(px) = render(|ctx, pool| {
        let t = halves_down(ctx, pool);
        vec![scrolling(
            t,
            2,
            4,
            [0.0, 0.0, 16.0, 64.0],
            Scroll {
                direction: ScrollDirection::Up,
                travelled: 16.0,
                ..Scroll::default()
            },
        )]
    }) else {
        return;
    };
    assert_close(at(&px, W, 8, 4), GREEN, TOL);
    assert_close(at(&px, W, 8, 20), RED, TOL);
    assert_close(at(&px, W, 8, 36), GREEN, TOL);
    assert_close(at(&px, W, 30, 8), NONE, TOL);
}

#[test]
fn a_styled_window_keeps_its_shape_while_the_picture_moves() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        let mut layer = scrolling(
            t,
            4,
            2,
            [0.0, 0.0, 64.0, 32.0],
            Scroll {
                travelled: 16.0,
                ..Scroll::default()
            },
        );
        layer.style = LayerStyle::new(
            LayerShape::Rounded { radius: 12.0 },
            0.0,
            [0; 4],
            [64.0, 32.0],
        );
        vec![layer]
    }) else {
        return;
    };
    // The window is 64 x 32: the picture is 64 wide (texels 16 wide), so 16 travelled
    // shows red up to 8, green from 24 and red again from 56.
    assert_close(at(&px, W, 1, 1), NONE, TOL);
    assert_close(at(&px, W, 4, 16), RED, TOL);
    assert_close(at(&px, W, 30, 16), GREEN, TOL);
    assert_close(at(&px, W, 58, 16), RED, TOL);
}

#[test]
fn without_a_binding_a_scrolling_layer_draws_nothing_and_others_draw() {
    let Some((px, stats)) = render_with(false, |ctx, pool| {
        let t = halves(ctx, pool);
        let plain = texture(ctx, pool, 2, 2, |_, _| GREEN);
        vec![
            scrolling(t, 4, 2, STRIP, Scroll::default()),
            Layer::new(plain, 0.0, 32.0, 64.0, 32.0),
        ]
    }) else {
        return;
    };
    assert_eq!(stats.unscrolled_layers, 1);
    assert_eq!(stats.drawn_layers, 1);
    assert_close(at(&px, W, 10, 8), NONE, TOL);
    assert_close(at(&px, W, 10, 40), GREEN, TOL);
}

#[test]
fn a_plain_layer_beside_a_scrolling_one_draws_as_before() {
    let Some(px) = render(|ctx, pool| {
        let t = halves(ctx, pool);
        let plain = texture(ctx, pool, 2, 2, |_, _| GREEN);
        let mut layers = vec![Layer::new(plain, 0.0, 32.0, 64.0, 32.0)];
        layers.push(scrolling(t, 4, 2, STRIP, Scroll::default()));
        layers.push(Layer::new(plain, 0.0, 20.0, 8.0, 8.0));
        layers
    }) else {
        return;
    };
    assert_close(at(&px, W, 10, 40), GREEN, TOL);
    assert_close(at(&px, W, 2, 8), RED, TOL);
    assert_close(at(&px, W, 4, 24), GREEN, TOL);
}

#[test]
fn a_speed_change_keeps_the_picture_where_it_was() {
    // Rendering the run just before and just after a pause anchor gives the same
    // window: evaluated in f64, sent as the wrapped start only.
    let run = ScrollRun::start(s(0.0), 100.0);
    let paused = run.change_speed(s(5.0), 0.0, Duration::ZERO);
    let spec = |run| LayerScroll {
        direction: ScrollDirection::Left,
        repeat: ScrollRepeat::Loop,
        start: ScrollStart::Inside,
        gap: 0.0,
        fade: 0.0,
        run,
    };
    let t = Affine2::rect(0.0, 0.0, 64.0, 16.0);
    let a = spec(run).frame(&t, &UvRect::FULL, 4, 2, s(5.0)).unwrap();
    let b = spec(paused).frame(&t, &UvRect::FULL, 4, 2, s(9.0)).unwrap();
    assert!((a.start - b.start).abs() < 1e-9);
}
