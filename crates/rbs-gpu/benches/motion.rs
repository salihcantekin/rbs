#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! Motion probe benchmarks (ADR-0126; spec docs/specs/gpu-compositor.md, "Motion
//! probe"). Hot path while a sync offset measurement runs: once per new picture.
//!
//! - `submit_1080p`: the render thread's share, recording and submitting the dispatch
//!   and the 4-byte copy and starting the map, for a 1080p picture. Never waits.
//! - `round_trip_1080p`: submit, wait for the GPU, take the value: frame time on the
//!   adapter in use. On software adapters this is CPU-bound and only useful for
//!   relative comparisons.
//!
//! Prints a skip message and measures nothing when no adapter exists.

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::{ColorSpace, PixelFormat, Size, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_gpu::{GpuContext, GpuRequest, MotionProbe, TextureDesc, TexturePool, readback};

const W: u32 = 1920;
const H: u32 = 1080;

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("SKIPPED motion benchmarks: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());
    let mut pool = TexturePool::new();
    let src = pool
        .acquire(
            &ctx,
            &TextureDesc::source(W, H, wgpu::TextureFormat::Rgba8UnormSrgb),
        )
        .expect("source");
    let pixels: Vec<u8> = (0..W * H)
        .flat_map(|i| [(i % 251) as u8, 90, 200, 255])
        .collect();
    readback::upload(&ctx, &pool, src, &pixels).expect("upload");
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Rgba8, ColorSpace::SRGB);
    let picture = pool.frame(src, format, Timestamp::ZERO).expect("frame");
    let mut probe = MotionProbe::new(&ctx).expect("probe");
    let mut t = 0u64;

    c.bench_function("motion_probe/submit_1080p", |b| {
        b.iter_custom(|iters| {
            let mut spent = std::time::Duration::ZERO;
            for _ in 0..iters {
                let start = std::time::Instant::now();
                let ok = probe.submit(&ctx, &pool, &picture, t).expect("submit");
                spent += start.elapsed();
                assert!(ok, "the ring is drained after every submit");
                t += 1;
                // The GPU's share and the take are outside the render thread's share.
                ctx.wait_idle().expect("idle");
                ctx.poll();
                probe.take(&ctx, |_, m| {
                    std::hint::black_box(m);
                });
            }
            spent
        })
    });
    ctx.wait_idle().expect("idle");
    ctx.poll();
    probe.take(&ctx, |_, _| {});

    c.bench_function("motion_probe/round_trip_1080p", |b| {
        b.iter(|| {
            probe.submit(&ctx, &pool, &picture, t).expect("submit");
            t += 1;
            ctx.wait_idle().expect("idle");
            ctx.poll();
            probe.take(&ctx, |_, m| {
                std::hint::black_box(m);
            });
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
