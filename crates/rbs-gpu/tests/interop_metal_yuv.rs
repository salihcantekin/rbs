//! Import of bi-planar 4:2:0 IOSurfaces ('420v', 'x420') on Metal, decoded to RGB (spec
//! `gpu-compositor.md`, "Native import", rule 6, and "YUV source decode"). The producer
//! is the engine's own `SurfaceExporter`, so the test is a full round trip: canvas, NV12 or
//! P010 IOSurface, import, decode, canvas again.
#![cfg(target_vendor = "apple")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, at, solid};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{FramePayload, PixelFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::context::CAP_P010;
use rbs_gpu::export::SurfaceExporter;
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, Importer, Layer, TextureDesc, TextureId, TexturePool,
    YuvFormat, readback,
};

const W: u32 = 64;
const H: u32 = 32;
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREY: [f32; 4] = [0.2, 0.2, 0.2, 1.0];

/// Red on the left half, grey on the right.
fn canvas(ctx: &GpuContext, pool: &mut TexturePool) -> TextureId {
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        2,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let red = solid(ctx, pool, 2, 2, RED);
    let grey = solid(ctx, pool, 2, 2, GREY);
    let half = W as f32 / 2.0;
    let layers = [
        Layer::new(red, 0.0, 0.0, half, H as f32),
        Layer::new(grey, half, 0.0, half, H as f32),
    ];
    comp.render(ctx, pool, &layers, canvas).unwrap();
    canvas
}

fn exported(
    ctx: &GpuContext,
    pool: &TexturePool,
    exporter: &mut SurfaceExporter,
    canvas: TextureId,
    n: u64,
) -> VideoFrame {
    exporter
        .export(ctx, pool, canvas, Timestamp::from_nanos(n), n)
        .unwrap();
    ctx.wait_idle().unwrap();
    exporter.take_ready(ctx).unwrap().expect("frame ready")
}

fn round_trip(format: YuvFormat, tol: f32) {
    let Some(ctx) = common::gpu() else { return };
    let p010 = ctx.capabilities().get(&CAP_P010) == Some(&CapabilityValue::Flag(true));
    if format == YuvFormat::P010 && !p010 {
        eprintln!("SKIPPED: device lacks gpu.p010");
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let mut exporter = SurfaceExporter::new(ctx, format, W, H, 2).unwrap();
    let mut importer = Importer::new();
    let opened_before = ctx.metrics().surfaces_opened();

    for n in 1..=3 {
        let native = exported(ctx, &pool, &mut exporter, canvas, n);
        assert_eq!(native.format().pixel_format, format.pixel_format());
        let imported = importer.import(ctx, &mut pool, native).unwrap();
        assert_eq!(imported.format().pixel_format, PixelFormat::Rgba16Float);
        assert_eq!(imported.sequence(), n);
        let FramePayload::Texture(id) = *imported.payload() else {
            panic!("import must return a texture frame");
        };
        let px = readback::read_rgba16f(ctx, &pool, id).unwrap();
        assert_close(at(&px, W, 4, 4), RED, tol);
        assert_close(at(&px, W, W - 5, H - 5), GREY, tol);

        drop(imported);
        importer.retire(ctx, &mut pool, Timestamp::ZERO);
        ctx.wait_idle().unwrap();
        importer.retire(ctx, &mut pool, Timestamp::ZERO);
        assert_eq!(exporter.held(), 0, "surface handed back after the decode");
    }
    assert_eq!(
        ctx.metrics().surfaces_opened() - opened_before,
        2,
        "a ring of two surfaces is opened twice, then reused"
    );
    assert_eq!(importer.open_surfaces(), 2);
}

#[test]
fn nv12_io_surface_is_decoded() {
    round_trip(YuvFormat::Nv12, 0.02);
}

#[test]
fn p010_io_surface_is_decoded() {
    round_trip(YuvFormat::P010, 0.01);
}

#[test]
fn decoded_frame_outlives_the_surface_lease() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 2).unwrap();
    let mut importer = Importer::new();
    let native = exported(ctx, &pool, &mut exporter, canvas, 1);
    let imported = importer.import(ctx, &mut pool, native).unwrap();
    // The decoded frame is a copy: the producer's surface goes back while it is held.
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    ctx.wait_idle().unwrap();
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    assert_eq!(exporter.held(), 0);
    let FramePayload::Texture(id) = *imported.payload() else {
        panic!("texture frame");
    };
    let px = readback::read_rgba16f(ctx, &pool, id).unwrap();
    assert_close(at(&px, W, 4, 4), RED, 0.02);
}
