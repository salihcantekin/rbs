#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! Scrolling layers (ADR-0150 decision 13; spec docs/specs/gpu-compositor.md,
//! "Scrolling layers").
//!
//! - `scroll_eval/64`: CPU cost of evaluating 64 scroll runs for one tick (the node's
//!   per-tick work for 64 scrolling layers).
//! - `compositor_new`: building one compositor; nothing in it depends on scrolling.
//! - `frame_1080p_scroll/*`: a 1080p frame of a full-canvas picture and seven tiles,
//!   plain, with one 1920 x 80 ticker on top, with all eight scrolling, with all eight
//!   scrolling with a rounded style (`styled_8_scrolling`, the style on the window) and
//!   eight Library shapes (2 x 2 fills with a style) scrolling (`shapes_8_scrolling`,
//!   the style moving with the picture, ADR-0177); each
//!   iteration renders and waits for the GPU. On software adapters this is CPU-bound and
//!   only useful against `composite` of `benches/compositor.rs` on the same machine.
//!
//! Prints how long the device's wrap pipelines took to build, once.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_core::time::Timestamp;
use rbs_gpu::{
    Affine2, CanvasDesc, Compositor, GpuContext, GpuRequest, Layer, LayerScroll, LayerShape,
    LayerStyle, ScrollBinding, ScrollDirection, ScrollRepeat, ScrollRun, ScrollStart, TextureDesc,
    TexturePool, UvRect, readback,
};

const W: u32 = 1920;
const H: u32 = 1080;

fn spec(i: usize) -> LayerScroll {
    LayerScroll {
        direction: ScrollDirection::Left,
        repeat: ScrollRepeat::Loop,
        start: ScrollStart::Outside,
        gap: 120.0,
        fade: 24.0,
        run: ScrollRun::start(Timestamp::ZERO, 90.0 + i as f64).change_speed(
            Timestamp::from_nanos(3_000_000_000),
            140.0,
            Duration::from_millis(300),
        ),
    }
}

fn bench(c: &mut Criterion) {
    let now = Timestamp::from_nanos(86_400_000_000_000);
    let specs: Vec<LayerScroll> = (0..64).map(spec).collect();
    let boxes: Vec<Affine2> = (0..64)
        .map(|i| Affine2::rect(0.0, i as f32 * 16.0, 1920.0, 80.0))
        .collect();
    c.bench_function("scroll_eval/64", |b| {
        b.iter(|| {
            let mut moving = 0u32;
            for (s, t) in specs.iter().zip(&boxes) {
                if let Some(f) = s.frame(t, &UvRect::FULL, 4096, 96, now) {
                    std::hint::black_box(f.gpu(s.direction, s.repeat, s.fade));
                }
                moving += u32::from(s.moving_at(now, false));
            }
            std::hint::black_box(moving)
        });
    });

    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("SKIPPED scroll GPU benchmarks: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());
    let canvas_desc = CanvasDesc {
        width: W,
        height: H,
    };
    c.bench_function("compositor_new", |b| {
        b.iter(|| Compositor::new(&ctx, canvas_desc, 32).expect("compositor"));
    });
    let started = Instant::now();
    ctx.prepare_scrolling();
    eprintln!("wrap pipelines built in {:?}", started.elapsed());
    let mut binding = ScrollBinding::new(&ctx);

    let mut pool = TexturePool::new();
    let canvas = pool
        .acquire(&ctx, &TextureDesc::canvas(W, H))
        .expect("canvas");
    let src = pool
        .acquire(
            &ctx,
            &TextureDesc::source(1920, 1080, wgpu::TextureFormat::Rgba8UnormSrgb),
        )
        .expect("source");
    let pixels: Vec<u8> = (0..W * H)
        .flat_map(|i| [(i % 251) as u8, 90, 200, 255])
        .collect();
    readback::upload(&ctx, &pool, src, &pixels).expect("upload");
    // A text-like ticker picture: 4096 x 96.
    let line = pool
        .acquire(
            &ctx,
            &TextureDesc::source(4096, 96, wgpu::TextureFormat::Rgba8UnormSrgb),
        )
        .expect("line");
    let text: Vec<u8> = (0..4096 * 96)
        .flat_map(|i| [255, 255, 255, if (i / 7) % 3 == 0 { 255 } else { 0 }])
        .collect();
    readback::upload(&ctx, &pool, line, &text).expect("upload");
    let mut comp = Compositor::new(&ctx, canvas_desc, 32).expect("compositor");

    let plain: Vec<Layer> = (0..8)
        .map(|i| match i {
            0 => Layer::new(src, 0.0, 0.0, W as f32, H as f32),
            _ => Layer::new(
                src,
                (i % 4) as f32 * 480.0,
                (i / 4 % 4) as f32 * 270.0,
                480.0,
                270.0,
            ),
        })
        .collect();
    let scrolled = |layer: &Layer, texture, tw: u32, th: u32, i: usize| {
        let s = spec(i);
        let mut l = *layer;
        l.texture = texture;
        let f = s
            .frame(&l.transform, &UvRect::FULL, tw, th, now)
            .expect("frame");
        l.scroll = Some(f.gpu(s.direction, s.repeat, s.fade));
        l
    };
    let mut ticker = plain.clone();
    let bar = Layer::new(line, 0.0, 1000.0, 1920.0, 80.0);
    ticker.push(scrolled(&bar, line, 4096, 96, 0));
    let mut plain_bar = plain.clone();
    plain_bar.push(Layer::new(line, 0.0, 1000.0, 1920.0, 80.0));
    let all: Vec<Layer> = plain
        .iter()
        .enumerate()
        .map(|(i, l)| scrolled(l, src, 1920, 1080, i))
        .collect();

    let rounded = |l: &Layer| {
        let mut l = *l;
        let size = [l.transform.a.abs(), l.transform.d.abs()];
        l.style = LayerStyle::new(LayerShape::Rounded { radius: 24.0 }, 6.0, [255; 4], size);
        l
    };
    let styled: Vec<Layer> = all.iter().map(rounded).collect();
    let fill = pool
        .acquire(
            &ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8UnormSrgb),
        )
        .expect("fill");
    readback::upload(&ctx, &pool, fill, &[40, 110, 230, 255].repeat(4)).expect("upload");
    let shapes: Vec<Layer> = plain
        .iter()
        .enumerate()
        .map(|(i, l)| rounded(&scrolled(l, fill, 2, 2, i)))
        .collect();

    let mut group = c.benchmark_group("frame_1080p_scroll");
    group.sample_size(20);
    for (name, layers) in [
        ("plain_8", &plain),
        ("plain_8_bar", &plain_bar),
        ("ticker_8_plus_1", &ticker),
        ("all_8_scrolling", &all),
        ("styled_8_scrolling", &styled),
        ("shapes_8_scrolling", &shapes),
    ] {
        group.bench_with_input(BenchmarkId::new("composite", name), layers, |b, layers| {
            b.iter(|| {
                comp.render_scrolled(&ctx, &pool, layers, canvas, Some(&mut binding))
                    .expect("render");
                ctx.wait_idle().expect("wait");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
