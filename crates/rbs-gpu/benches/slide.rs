#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! The slide show change pass (ADR-0171 decision 7; spec docs/specs/gpu-compositor.md,
//! "Slide show change"): one 1080p draw of the change, fade and slide, each iteration
//! waiting for the GPU. Between changes the node draws nothing, so the steady state
//! costs no pass at all; this is the cost per tick only during a change.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_core::media::{ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_gpu::upload::ReservedUpload;
use rbs_gpu::{GpuContext, GpuRequest, ReservedTargets, SlideKind, SlidePass, TexturePool};

const W: u32 = 1920;
const H: u32 = 1080;

fn picture(seed: u8) -> SystemFrame {
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Rgba8, ColorSpace::SRGB);
    let bytes: Vec<u8> = (0..W * H)
        .flat_map(|i| [(i % 251) as u8 ^ seed, 90, 200, 255])
        .collect();
    SystemFrame::new(
        format,
        bytes.into(),
        &[PlaneLayout {
            offset: 0,
            stride: W as usize * 4,
        }],
    )
    .expect("frame")
}

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("SKIPPED slide benchmarks: {e}");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());
    let mut pool = TexturePool::new();
    let mut up = ReservedUpload::reserve(&ctx, &mut pool, W, H, PixelFormat::Rgba8).expect("up");
    let a = up
        .upload_into(&ctx, &mut pool, &picture(0), Timestamp::ZERO)
        .expect("upload")
        .expect("a texture");
    let b = up
        .upload_into(&ctx, &mut pool, &picture(7), Timestamp::ZERO)
        .expect("upload")
        .expect("a texture");
    let id = |f: &rbs_core::media::VideoFrame| match *f.payload() {
        rbs_core::media::FramePayload::Texture(id) => id,
        _ => panic!("not a texture"),
    };
    let (from, to) = (id(&a), id(&b));
    drop((a, b));
    let targets = ReservedTargets::reserve(&ctx, &mut pool, W, H).expect("targets");
    let pass = SlidePass::new(&ctx);
    let mut group = c.benchmark_group("slide_1080p");
    for (name, kind) in [("fade", SlideKind::Fade), ("slide", SlideKind::Slide)] {
        group.bench_with_input(BenchmarkId::new("draw", name), &kind, |bch, kind| {
            let mut t = 0.0f32;
            bch.iter(|| {
                t = (t + 0.01) % 1.0;
                let frame = pass
                    .draw(&ctx, &pool, &targets, from, to, *kind, t, Timestamp::ZERO)
                    .expect("draw")
                    .expect("a free target");
                ctx.wait_idle().expect("wait");
                drop(frame);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
