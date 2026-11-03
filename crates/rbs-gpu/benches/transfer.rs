#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! System-memory upload and download benchmarks at 1080p (spec
//! docs/specs/gpu-compositor.md, "System-memory upload" and "System-memory download").
//!
//! - `download_1080p/submit_take`: one `YuvDownload::submit`, a wait for the GPU, then
//!   `take_ready` (conversion, readback and the CPU copy into a frame buffer).
//! - `download_1080p/take_copy`: only the render-thread part after the GPU finished
//!   (`take_ready` with the copy), measured by submitting outside the timed section.
//! - `upload_1080p/nv12` and `upload_1080p/bgra`: `SourceUpload::upload` plus a wait for
//!   the GPU (write, and for NV12 the decode).
//!
//! On software adapters (CI) the GPU part is CPU-bound; compare against `main` only.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::{
    Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::{
    GpuContext, GpuRequest, SourceUpload, TextureDesc, TexturePool, YuvDownload, YuvFormat,
};

const W: u32 = 1920;
const H: u32 = 1080;

fn canvas_frame(pool: &TexturePool, id: rbs_gpu::TextureId, i: u64) -> VideoFrame {
    let format = VideoFormat::new(
        Size::new(W, H),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    pool.frame(id, format, Timestamp::from_nanos(i))
        .expect("frame")
        .with_sequence(i)
}

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("SKIPPED transfer benchmarks: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());
    let mut pool = TexturePool::new();
    let canvas = pool
        .acquire(&ctx, &TextureDesc::canvas(W, H))
        .expect("canvas");
    let mut down = YuvDownload::new(&ctx, YuvFormat::Nv12, W, H).expect("download");

    let mut g = c.benchmark_group("download_1080p");
    g.sample_size(20);
    let mut i = 0;
    g.bench_function("submit_take", |b| {
        b.iter(|| {
            i += 1;
            down.submit(&ctx, &pool, &canvas_frame(&pool, canvas, i))
                .expect("submit");
            ctx.wait_idle().expect("wait");
            ctx.poll();
            down.take_ready(&ctx).expect("take").expect("a frame")
        });
    });
    g.bench_function("take_copy", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                i += 1;
                down.submit(&ctx, &pool, &canvas_frame(&pool, canvas, i))
                    .expect("submit");
                ctx.wait_idle().expect("wait");
                ctx.poll();
                let t = Instant::now();
                let frame = down.take_ready(&ctx).expect("take").expect("a frame");
                total += t.elapsed();
                drop(frame);
            }
            total
        });
    });
    g.finish();

    let mut g = c.benchmark_group("upload_1080p");
    g.sample_size(20);
    let (w, h) = (W as usize, H as usize);
    let nv12 = SystemFrame::new(
        VideoFormat::new(
            Size::new(W, H),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        Bytes::from(vec![100u8; w * h * 3 / 2]),
        &[
            PlaneLayout {
                offset: 0,
                stride: w,
            },
            PlaneLayout {
                offset: w * h,
                stride: w,
            },
        ],
    )
    .expect("nv12");
    let bgra = SystemFrame::new(
        VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB),
        Bytes::from(vec![100u8; w * h * 4]),
        &[PlaneLayout {
            offset: 0,
            stride: w * 4,
        }],
    )
    .expect("bgra");
    let mut up = SourceUpload::new();
    for (name, frame) in [("nv12", nv12), ("bgra", bgra)] {
        g.bench_function(name, |b| {
            b.iter(|| {
                let f = VideoFrame::system(Timestamp::ZERO, frame.clone(), None);
                let out = up.upload(&ctx, &mut pool, f).expect("upload");
                ctx.wait_idle().expect("wait");
                out
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
