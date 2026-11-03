//! YUV source decode: NV12 and P010 planes back to linear RGB. Round trips through the
//! encoder conversion and back, so both directions agree. Spec:
//! docs/specs/gpu-compositor.md, "YUV source decode".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, at, solid};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{ColorSpace, TextureId};
use rbs_gpu::context::{CAP_NV12_SURFACE, CAP_P010};
use rbs_gpu::yuv_decode::decoded_desc;
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, TextureDesc, TexturePool, YuvConverter,
    YuvDecoder, YuvFormat, YuvSource, readback, yuv,
};

const W: u32 = 16;
const H: u32 = 8;

const COLOURS: [[f32; 3]; 6] = [
    [0.0, 0.0, 0.0],
    [1.0, 1.0, 1.0],
    [0.18, 0.18, 0.18],
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.05, 0.2, 0.9],
];

fn flag(ctx: &GpuContext, key: &rbs_core::capability::CapabilityKey) -> bool {
    ctx.capabilities().get(key) == Some(&CapabilityValue::Flag(true))
}

fn canvas(ctx: &GpuContext, pool: &mut TexturePool, rgb: [f32; 3]) -> TextureId {
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let src = solid(ctx, pool, 2, 2, [rgb[0], rgb[1], rgb[2], 1.0]);
    comp.render(
        ctx,
        pool,
        &[Layer::new(src, 0.0, 0.0, W as f32, H as f32)],
        canvas,
    )
    .unwrap();
    canvas
}

/// Encoder planes of a solid colour, decoded again: within one 8-bit code step.
fn round_trip(format: YuvFormat, tol: f32) {
    let Some(ctx) = common::gpu() else { return };
    if format == YuvFormat::P010 && !flag(ctx, &CAP_P010) {
        eprintln!("SKIPPED: device lacks gpu.p010");
        return;
    }
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, format, W, H).unwrap();
    let mut dec = YuvDecoder::new(ctx);
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    let out = pool.acquire(ctx, &decoded_desc(W, H)).unwrap();
    for rgb in COLOURS {
        let canvas = canvas(ctx, &mut pool, rgb);
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
        dec.decode(
            ctx,
            &pool,
            YuvSource::Planes { luma, chroma },
            ColorSpace::BT709_LIMITED,
            out,
        )
        .unwrap();
        let px = readback::read_rgba16f(ctx, &pool, out).unwrap();
        for (x, y) in [(0, 0), (7, 3), (W - 1, H - 1)] {
            assert_close(at(&px, W, x, y), [rgb[0], rgb[1], rgb[2], 1.0], tol);
        }
    }
    assert_eq!(dec.cached_sources(), 1, "one source, one cached bind group");
}

#[test]
fn nv12_round_trip() {
    round_trip(YuvFormat::Nv12, 0.02);
}

#[test]
fn p010_round_trip() {
    round_trip(YuvFormat::P010, 0.01);
}

#[test]
fn full_range_and_bt601_codes() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut dec = YuvDecoder::new(ctx);
    let luma = pool
        .acquire(
            ctx,
            &TextureDesc::source(W, H, wgpu::TextureFormat::R8Unorm),
        )
        .unwrap();
    let chroma = pool
        .acquire(
            ctx,
            &TextureDesc::source(W / 2, H / 2, wgpu::TextureFormat::Rg8Unorm),
        )
        .unwrap();
    let out = pool.acquire(ctx, &decoded_desc(W, H)).unwrap();
    // BT.601 full range red: Y' 76, Cb 85, Cr 255 (JPEG / webcam MJPEG levels).
    let fill = |id, w: u32, h: u32, texel: &[u8]| {
        let data: Vec<u8> = (0..w * h).flat_map(|_| texel.iter().copied()).collect();
        readback::upload(ctx, &pool, id, &data).unwrap();
    };
    fill(luma, W, H, &[76]);
    fill(chroma, W / 2, H / 2, &[85, 255]);
    let bt601_full = ColorSpace::new(
        rbs_core::media::ColorPrimaries::Bt601Ntsc,
        rbs_core::media::TransferFunction::Bt709,
        rbs_core::media::MatrixCoefficients::Bt601,
        rbs_core::media::ColorRange::Full,
    );
    dec.decode(
        ctx,
        &pool,
        YuvSource::Planes { luma, chroma },
        bt601_full,
        out,
    )
    .unwrap();
    let px = readback::read_rgba16f(ctx, &pool, out).unwrap();
    assert_close(at(&px, W, 3, 3), [1.0, 0.0, 0.0, 1.0], 0.02);
}

#[test]
fn surface_source_matches_planes() {
    let Some(ctx) = common::gpu() else { return };
    if !flag(ctx, &CAP_NV12_SURFACE) {
        eprintln!("SKIPPED: device lacks gpu.nv12_surface");
        return;
    }
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let mut dec = YuvDecoder::new(ctx);
    let mut desc = yuv::surface_desc(YuvFormat::Nv12, W, H);
    desc.usage |= wgpu::TextureUsages::TEXTURE_BINDING;
    let surface = pool.acquire(ctx, &desc).unwrap();
    let out = pool.acquire(ctx, &decoded_desc(W, H)).unwrap();
    let rgb = COLOURS[5];
    let canvas = canvas(ctx, &mut pool, rgb);
    conv.convert_to_surface(ctx, &pool, canvas, surface)
        .unwrap();
    dec.decode(
        ctx,
        &pool,
        YuvSource::Surface(surface),
        ColorSpace::BT709_LIMITED,
        out,
    )
    .unwrap();
    let px = readback::read_rgba16f(ctx, &pool, out).unwrap();
    assert_close(at(&px, W, 5, 5), [rgb[0], rgb[1], rgb[2], 1.0], 0.02);
}

#[test]
fn wrong_targets_and_colour_spaces_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut dec = YuvDecoder::new(ctx);
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let src = YuvSource::Planes { luma, chroma };
    let small = pool.acquire(ctx, &decoded_desc(W / 2, H)).unwrap();
    assert!(matches!(
        dec.decode(ctx, &pool, src, ColorSpace::BT709_LIMITED, small),
        Err(GpuError::TargetMismatch(_))
    ));
    let out = pool.acquire(ctx, &decoded_desc(W, H)).unwrap();
    assert!(matches!(
        dec.decode(ctx, &pool, src, ColorSpace::BT2100_PQ, out),
        Err(GpuError::Unsupported(_))
    ));
    // Planes swapped: the chroma plane is not the luma format.
    assert!(matches!(
        dec.decode(
            ctx,
            &pool,
            YuvSource::Planes {
                luma: chroma,
                chroma: luma
            },
            ColorSpace::BT709_LIMITED,
            out,
        ),
        Err(GpuError::TargetMismatch(_))
    ));
}
