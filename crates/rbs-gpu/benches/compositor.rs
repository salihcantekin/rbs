#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! Compositor and NV12 benchmarks (spec docs/specs/gpu-compositor.md).
//!
//! Each iteration renders one 1080p frame, converts it to NV12 and waits for the GPU,
//! so the numbers are frame time on the adapter in use. On software adapters (CI)
//! this is CPU-bound and only useful for relative comparisons against `main`.
//! Prints a skip message and measures nothing when no adapter exists.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuRequest, Layer, Nv12Converter, TextureDesc, TexturePool,
    nv12, readback,
};

const W: u32 = 1920;
const H: u32 = 1080;

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("SKIPPED compositor benchmarks: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());
    let mut pool = TexturePool::new();
    let canvas = pool
        .acquire(&ctx, &TextureDesc::canvas(W, H))
        .expect("canvas");
    let luma = pool.acquire(&ctx, &nv12::luma_desc(W, H)).expect("luma");
    let chroma = pool
        .acquire(&ctx, &nv12::chroma_desc(W, H))
        .expect("chroma");
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
    let mut comp = Compositor::new(
        &ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        32,
    )
    .expect("compositor");
    let mut conv = Nv12Converter::new(&ctx, W, H).expect("nv12");

    let mut group = c.benchmark_group("frame_1080p");
    group.sample_size(20);
    for count in [1usize, 8, 32] {
        // First layer fills the canvas; the rest are 480x270 tiles.
        let layers: Vec<Layer> = (0..count)
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
        group.bench_with_input(
            BenchmarkId::new("composite", count),
            &layers,
            |b, layers| {
                b.iter(|| {
                    comp.render(&ctx, &pool, layers, canvas).expect("render");
                    ctx.wait_idle().expect("wait");
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("composite_nv12", count),
            &layers,
            |b, layers| {
                b.iter(|| {
                    comp.render(&ctx, &pool, layers, canvas).expect("render");
                    conv.convert(&ctx, &pool, canvas, luma, chroma)
                        .expect("convert");
                    ctx.wait_idle().expect("wait");
                });
            },
        );
    }
    group.finish();

    // CPU cost of recording and submitting only (no wait): what the render thread pays.
    let layers: Vec<Layer> = (0..32)
        .map(|_| Layer::new(src, 0.0, 0.0, 64.0, 64.0))
        .collect();
    let small = pool
        .acquire(&ctx, &TextureDesc::canvas(64, 64))
        .expect("small canvas");
    let mut tiny = Compositor::new(
        &ctx,
        CanvasDesc {
            width: 64,
            height: 64,
        },
        32,
    )
    .expect("tiny");
    c.bench_function("record_submit_32_layers_64px", |b| {
        b.iter(|| {
            tiny.render(&ctx, &pool, &layers, small).expect("render");
            ctx.poll();
        });
    });
    ctx.wait_idle().expect("wait");
}

criterion_group!(benches, bench);
criterion_main!(benches);
