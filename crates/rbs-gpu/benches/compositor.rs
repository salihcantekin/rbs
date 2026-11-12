#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! Compositor, NV12 and P010 benchmarks (spec docs/specs/gpu-compositor.md).
//!
//! Each iteration renders one 1080p frame, converts it to NV12 and waits for the GPU,
//! so the numbers are frame time on the adapter in use. On software adapters (CI)
//! this is CPU-bound and only useful for relative comparisons against `main`.
//! Prints a skip message and measures nothing when no adapter exists.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuRequest, Layer, LayerShape, LayerStyle, ScaleKernel,
    TextureDesc, TexturePool, YuvConverter, YuvFormat, readback, yuv,
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
    let luma = pool
        .acquire(&ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .expect("luma");
    let chroma = pool
        .acquire(&ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
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
    let mut conv = YuvConverter::new(&ctx, YuvFormat::Nv12, W, H).expect("nv12");
    assert_eq!(
        conv.kernel(),
        ScaleKernel::OneToOne,
        "1:1 converter must stay on textureLoad"
    );

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
        // The same scene with every tile styled (ADR-0053): rounded corners and a border
        // on even tiles, a circle with a border on odd ones; the full-canvas layer stays
        // plain, so the pipeline switches between them.
        let styled: Vec<Layer> = layers
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let mut l = *l;
                if i > 0 {
                    let shape = if i % 2 == 0 {
                        LayerShape::Rounded { radius: 24.0 }
                    } else {
                        LayerShape::Circle
                    };
                    l.style = LayerStyle::new(shape, 6.0, [255, 255, 255, 255], [480.0, 270.0]);
                }
                l
            })
            .collect();
        group.bench_with_input(
            BenchmarkId::new("composite_styled", count),
            &styled,
            |b, layers| {
                b.iter(|| {
                    comp.render(&ctx, &pool, layers, canvas).expect("render");
                    ctx.wait_idle().expect("wait");
                });
            },
        );
    }
    // NV12 conversion alone: two plane textures vs one NV12 texture (when supported; on
    // software adapters the single surface uses the staged copy path).
    group.bench_function("nv12_planes", |b| {
        b.iter(|| {
            conv.convert(&ctx, &pool, canvas, luma, chroma)
                .expect("convert");
            ctx.wait_idle().expect("wait");
        });
    });
    // Scaled conversion for the virtual camera (ADR-0066): the 1080p canvas to 720p NV12.
    // 1920×1080 → 1280×720 is not an integer ratio, so it stays the bilinear entry
    // (`fs_luma_scaled` / `fs_chroma_scaled`).
    let mut scaled =
        YuvConverter::scaled(&ctx, YuvFormat::Nv12, W, H, 1280, 720).expect("scaled nv12");
    assert_eq!(scaled.kernel(), ScaleKernel::Bilinear);
    let luma720 = pool
        .acquire(&ctx, &yuv::luma_desc(YuvFormat::Nv12, 1280, 720))
        .expect("luma 720");
    let chroma720 = pool
        .acquire(&ctx, &yuv::chroma_desc(YuvFormat::Nv12, 1280, 720))
        .expect("chroma 720");
    group.bench_function("nv12_planes_scaled_to_720p", |b| {
        b.iter(|| {
            scaled
                .convert(&ctx, &pool, canvas, luma720, chroma720)
                .expect("convert");
            ctx.wait_idle().expect("wait");
        });
    });
    // Integer 2× box downscale (ADR-0102): the same 1080p canvas to 960×540 NV12.
    // `YuvConverter::scaled` selects `fs_luma_box` / `fs_chroma_box` for this ratio.
    let mut box2 = YuvConverter::scaled(&ctx, YuvFormat::Nv12, W, H, 960, 540).expect("box 2x");
    assert_eq!(box2.kernel(), ScaleKernel::Box(2));
    let luma540 = pool
        .acquire(&ctx, &yuv::luma_desc(YuvFormat::Nv12, 960, 540))
        .expect("luma 540");
    let chroma540 = pool
        .acquire(&ctx, &yuv::chroma_desc(YuvFormat::Nv12, 960, 540))
        .expect("chroma 540");
    group.bench_function("nv12_planes_box_2x", |b| {
        b.iter(|| {
            box2.convert(&ctx, &pool, canvas, luma540, chroma540)
                .expect("convert");
            ctx.wait_idle().expect("wait");
        });
    });
    // Source decode (camera / native NV12 import): planes back to a canvas-format texture.
    let mut decoder = rbs_gpu::YuvDecoder::new(&ctx);
    let decoded = pool
        .acquire(&ctx, &rbs_gpu::yuv_decode::decoded_desc(W, H))
        .expect("decoded");
    group.bench_function("nv12_decode", |b| {
        b.iter(|| {
            decoder
                .decode(
                    &ctx,
                    &pool,
                    rbs_gpu::YuvSource::Planes { luma, chroma },
                    rbs_core::media::ColorSpace::BT709_LIMITED,
                    decoded,
                )
                .expect("decode");
            ctx.wait_idle().expect("wait");
        });
    });
    if let Ok(surface) = pool.acquire(&ctx, &yuv::surface_desc(YuvFormat::Nv12, W, H)) {
        group.bench_function("nv12_surface", |b| {
            b.iter(|| {
                conv.convert_to_surface(&ctx, &pool, canvas, surface)
                    .expect("convert");
                ctx.wait_idle().expect("wait");
            });
        });
    }
    // P010 conversion alone, where the device has 16-bit normalized targets.
    let has = |key: &rbs_core::capability::CapabilityKey| {
        ctx.capabilities().get(key) == Some(&rbs_core::capability::CapabilityValue::Flag(true))
    };
    if has(&rbs_gpu::context::CAP_P010) {
        let mut p010 = YuvConverter::new(&ctx, YuvFormat::P010, W, H).expect("p010");
        let luma = pool
            .acquire(&ctx, &yuv::luma_desc(YuvFormat::P010, W, H))
            .expect("p010 luma");
        let chroma = pool
            .acquire(&ctx, &yuv::chroma_desc(YuvFormat::P010, W, H))
            .expect("p010 chroma");
        group.bench_function("p010_planes", |b| {
            b.iter(|| {
                p010.convert(&ctx, &pool, canvas, luma, chroma)
                    .expect("convert");
                ctx.wait_idle().expect("wait");
            });
        });
        if has(&rbs_gpu::context::CAP_P010_SURFACE) {
            let surface = pool
                .acquire(&ctx, &yuv::surface_desc(YuvFormat::P010, W, H))
                .expect("p010 surface");
            group.bench_function("p010_surface", |b| {
                b.iter(|| {
                    p010.convert_to_surface(&ctx, &pool, canvas, surface)
                        .expect("convert");
                    ctx.wait_idle().expect("wait");
                });
            });
        }
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
    // The same with every other layer styled: the CPU cost of the style records and
    // pipeline switches (ADR-0053).
    let styled: Vec<Layer> = layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let mut l = *l;
            if i % 2 == 1 {
                l.style = LayerStyle::new(LayerShape::Circle, 2.0, [255; 4], [64.0, 64.0]);
            }
            l
        })
        .collect();
    c.bench_function("record_submit_32_layers_64px_half_styled", |b| {
        b.iter(|| {
            tiny.render(&ctx, &pool, &styled, small).expect("render");
            ctx.poll();
        });
    });
    ctx.wait_idle().expect("wait");

    // Pool hot path: reuse a free texture, and share a frame's lease.
    let desc = TextureDesc::canvas(64, 64);
    let warm = pool.acquire(&ctx, &desc).expect("warm");
    pool.release(warm).expect("release");
    c.bench_function("pool_acquire_release_reuse", |b| {
        b.iter(|| {
            let id = pool.acquire(&ctx, &desc).expect("acquire");
            pool.release(id).expect("release");
        });
    });
    // `trim_idle` at the end of a steady tick: eight descriptions in use every tick
    // (ADR-0062 decision 5; nothing turns idle, so nothing is dropped).
    let descs: Vec<TextureDesc> = (1..=8).map(|i| TextureDesc::canvas(32 * i, 32)).collect();
    c.bench_function("pool_steady_tick_8_descriptions_trim_idle", |b| {
        b.iter(|| {
            for d in &descs {
                let id = pool.acquire(&ctx, d).expect("acquire");
                pool.release(id).expect("release");
            }
            pool.trim_idle(120);
        });
    });
    // A tick that creates the textures of a new 1080p size, and one that drops them
    // (a filter's input changed size, ADR-0062 decision 5).
    let big = TextureDesc::canvas(W, H);
    c.bench_function("pool_create_1080p_texture", |b| {
        b.iter_custom(|iters| {
            let mut spent = std::time::Duration::ZERO;
            for _ in 0..iters {
                let t = std::time::Instant::now();
                let id = pool.acquire(&ctx, &big).expect("acquire");
                spent += t.elapsed();
                pool.release(id).expect("release");
                pool.forget(&big, 1);
            }
            spent
        });
    });
    c.bench_function("pool_drop_1080p_texture", |b| {
        b.iter_custom(|iters| {
            let mut spent = std::time::Duration::ZERO;
            for _ in 0..iters {
                let id = pool.acquire(&ctx, &big).expect("acquire");
                pool.release(id).expect("release");
                let t = std::time::Instant::now();
                pool.forget(&big, 1);
                spent += t.elapsed();
            }
            spent
        });
    });
    ctx.wait_idle().expect("wait");
    let lease = pool.lease(canvas).expect("lease");
    c.bench_function("lease_clone_drop", |b| {
        b.iter(|| drop(std::hint::black_box(lease.clone())));
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
