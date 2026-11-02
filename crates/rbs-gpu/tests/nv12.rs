//! NV12 conversion: BT.709 limited-range values for known colours, within ±1.
//! Spec: docs/specs/gpu-compositor.md, "Encoder conversion (NV12)".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use rbs_gpu::{
    CanvasDesc, Compositor, GpuError, Layer, Nv12Converter, TextureDesc, TexturePool, nv12,
    readback,
};

const W: u32 = 16;
const H: u32 = 8;

fn oetf(l: f64) -> f64 {
    let l = l.clamp(0.0, 1.0);
    if l < 0.018 {
        4.5 * l
    } else {
        1.099 * l.powf(0.45) - 0.099
    }
}

/// Reference BT.709 limited-range (Y, Cb, Cr) for a linear RGB colour.
fn reference(rgb: [f64; 3]) -> [u8; 3] {
    let [r, g, b] = rgb.map(oetf);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    [
        (16.0 + 219.0 * y).round() as u8,
        (128.0 + 224.0 * cb).round() as u8,
        (128.0 + 224.0 * cr).round() as u8,
    ]
}

fn convert(rgb: [f32; 3]) -> Option<([u8; 3], [u8; 3])> {
    let ctx = common::gpu()?;
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        &ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(&ctx, &TextureDesc::canvas(W, H)).unwrap();
    let src = solid(&ctx, &mut pool, 2, 2, [rgb[0], rgb[1], rgb[2], 1.0]);
    comp.render(
        &ctx,
        &pool,
        &[Layer::new(src, 0.0, 0.0, W as f32, H as f32)],
        canvas,
    )
    .unwrap();

    let luma = pool.acquire(&ctx, &nv12::luma_desc(W, H)).unwrap();
    let chroma = pool.acquire(&ctx, &nv12::chroma_desc(W, H)).unwrap();
    let mut conv = Nv12Converter::new(&ctx, W, H).unwrap();
    conv.convert(&ctx, &pool, canvas, luma, chroma).unwrap();

    let y = readback::read_bytes(&ctx, &pool, luma).unwrap();
    let uv = readback::read_bytes(&ctx, &pool, chroma).unwrap();
    assert_eq!(y.len(), (W * H) as usize);
    assert_eq!(uv.len(), (W / 2 * H / 2 * 2) as usize);
    assert!(y.iter().all(|v| *v == y[0]), "uniform luma");
    let want = reference([f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])]);
    Some(([y[0], uv[0], uv[1]], want))
}

fn check(rgb: [f32; 3]) {
    let Some((got, want)) = convert(rgb) else {
        return;
    };
    for i in 0..3 {
        assert!(
            (i32::from(got[i]) - i32::from(want[i])).abs() <= 1,
            "{rgb:?}: got YCbCr {got:?}, want {want:?}"
        );
    }
}

#[test]
fn black() {
    check([0.0, 0.0, 0.0]);
}

#[test]
fn white() {
    check([1.0, 1.0, 1.0]);
}

#[test]
fn mid_grey() {
    check([0.5, 0.5, 0.5]);
}

#[test]
fn pure_red() {
    check([1.0, 0.0, 0.0]);
}

#[test]
fn reference_values_match_bt709_tables() {
    assert_eq!(reference([0.0, 0.0, 0.0]), [16, 128, 128]);
    assert_eq!(reference([1.0, 1.0, 1.0]), [235, 128, 128]);
    assert_eq!(reference([1.0, 0.0, 0.0]), [63, 102, 240]);
}

#[test]
fn odd_sizes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    assert!(matches!(
        Nv12Converter::new(&ctx, 15, 8),
        Err(GpuError::InvalidSize { .. })
    ));
}

#[test]
fn mismatched_planes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = pool.acquire(&ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool.acquire(&ctx, &nv12::luma_desc(W, H)).unwrap();
    let wrong = pool.acquire(&ctx, &nv12::luma_desc(W / 2, H / 2)).unwrap();
    let mut conv = Nv12Converter::new(&ctx, W, H).unwrap();
    assert_eq!(
        conv.convert(&ctx, &pool, canvas, luma, wrong),
        Err(GpuError::TargetMismatch(wrong))
    );
}
