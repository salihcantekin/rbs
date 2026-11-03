//! NV12 and P010 conversion: BT.709 limited-range values for known colours (NV12 within ±1
//! of 8-bit codes, P010 within ±2 of 10-bit codes), single surfaces equal to the plane
//! output. Spec: docs/specs/gpu-compositor.md, "Encoder conversion (NV12, P010)".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use rbs_core::capability::{CapabilityKey, CapabilityValue};
use rbs_gpu::context::{CAP_NV12_SURFACE, CAP_P010, CAP_P010_SURFACE};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, TextureDesc, TextureId, TexturePool,
    YuvConverter, YuvFormat, readback, yuv,
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

fn bits(format: YuvFormat) -> u32 {
    match format {
        YuvFormat::Nv12 => 8,
        YuvFormat::P010 => 10,
    }
}

/// Reference BT.709 limited-range (Y, Cb, Cr) codes for a linear RGB colour.
fn reference(format: YuvFormat, rgb: [f64; 3]) -> [u16; 3] {
    let k = f64::from(1u32 << (bits(format) - 8));
    let [r, g, b] = rgb.map(oetf);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    [
        (k * (16.0 + 219.0 * y)).round() as u16,
        (k * (128.0 + 224.0 * cb)).round() as u16,
        (k * (128.0 + 224.0 * cr)).round() as u16,
    ]
}

/// Plane bytes as codes: bytes for NV12; little-endian words shifted down by six for P010,
/// after checking that the low six bits are zero.
fn codes(format: YuvFormat, bytes: &[u8]) -> Vec<u16> {
    match format {
        YuvFormat::Nv12 => bytes.iter().map(|b| u16::from(*b)).collect(),
        YuvFormat::P010 => bytes
            .chunks_exact(2)
            .map(|w| {
                let word = u16::from_le_bytes([w[0], w[1]]);
                assert_eq!(
                    word & 0x3f,
                    0,
                    "P010 low six bits must be zero: {word:#06x}"
                );
                word >> 6
            })
            .collect(),
    }
}

fn flag(ctx: &GpuContext, key: &CapabilityKey) -> bool {
    ctx.capabilities().get(key) == Some(&CapabilityValue::Flag(true))
}

/// Whether the device supports `format` (and, with `surface`, its multi-planar texture).
/// `RBS_REQUIRE_NV12_SURFACE` turns a missing capability into a failure on runners known to
/// have NV12 and P010 surfaces.
fn supported(ctx: &GpuContext, format: YuvFormat, surface: bool) -> bool {
    let key = match (format, surface) {
        (YuvFormat::Nv12, false) => return true,
        (YuvFormat::Nv12, true) => &CAP_NV12_SURFACE,
        (YuvFormat::P010, false) => &CAP_P010,
        (YuvFormat::P010, true) => &CAP_P010_SURFACE,
    };
    let has = flag(ctx, key);
    if !has {
        assert!(
            std::env::var_os("RBS_REQUIRE_NV12_SURFACE").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_NV12_SURFACE is set but the device lacks {key:?}"
        );
        eprintln!("SKIPPED: device lacks {key:?}");
    }
    has
}

/// A canvas holding `layers` solid colours side by side (linear RGB).
fn canvas(ctx: &GpuContext, pool: &mut TexturePool, colours: &[[f32; 3]]) -> TextureId {
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        colours.len() as u32,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let width = W as f32 / colours.len() as f32;
    let layers: Vec<Layer> = colours
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let src = solid(ctx, pool, 2, 2, [c[0], c[1], c[2], 1.0]);
            Layer::new(src, i as f32 * width, 0.0, width, H as f32)
        })
        .collect();
    comp.render(ctx, pool, &layers, canvas).unwrap();
    canvas
}

fn check(format: YuvFormat, rgb: [f32; 3]) {
    let Some(ctx) = common::gpu() else { return };
    if !supported(ctx, format, false) {
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool, &[rgb]);
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    let mut conv = YuvConverter::new(ctx, format, W, H).unwrap();
    assert_eq!(conv.format(), format);
    conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();

    let y = codes(format, &readback::read_bytes(ctx, &pool, luma).unwrap());
    let uv = codes(format, &readback::read_bytes(ctx, &pool, chroma).unwrap());
    assert_eq!(y.len(), (W * H) as usize);
    assert_eq!(uv.len(), (W / 2 * H / 2 * 2) as usize);
    assert!(y.iter().all(|v| *v == y[0]), "uniform luma");
    let got = [y[0], uv[0], uv[1]];
    let want = reference(
        format,
        [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])],
    );
    let tolerance = if bits(format) == 8 { 1 } else { 2 };
    for i in 0..3 {
        assert!(
            (i32::from(got[i]) - i32::from(want[i])).abs() <= tolerance,
            "{format:?} {rgb:?}: got YCbCr {got:?}, want {want:?}"
        );
    }
}

const COLOURS: [[f32; 3]; 4] = [
    [0.0, 0.0, 0.0],
    [1.0, 1.0, 1.0],
    [0.5, 0.5, 0.5],
    [1.0, 0.0, 0.0],
];

#[test]
fn nv12_known_colours() {
    for rgb in COLOURS {
        check(YuvFormat::Nv12, rgb);
    }
}

#[test]
fn p010_known_colours() {
    for rgb in COLOURS {
        check(YuvFormat::P010, rgb);
    }
}

#[test]
fn reference_values_match_bt709_tables() {
    let nv12 = |rgb| reference(YuvFormat::Nv12, rgb);
    assert_eq!(nv12([0.0, 0.0, 0.0]), [16, 128, 128]);
    assert_eq!(nv12([1.0, 1.0, 1.0]), [235, 128, 128]);
    assert_eq!(nv12([1.0, 0.0, 0.0]), [63, 102, 240]);
    let p010 = |rgb| reference(YuvFormat::P010, rgb);
    assert_eq!(p010([0.0, 0.0, 0.0]), [64, 512, 512]);
    assert_eq!(p010([1.0, 1.0, 1.0]), [940, 512, 512]);
    assert_eq!(p010([1.0, 0.0, 0.0]), [250, 409, 960]);
}

#[test]
fn odd_sizes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    assert!(matches!(
        YuvConverter::new(ctx, YuvFormat::Nv12, 15, 8),
        Err(GpuError::InvalidSize { .. })
    ));
}

#[test]
fn p010_without_capability_is_unsupported() {
    let Some(ctx) = common::gpu() else { return };
    let result = YuvConverter::new(ctx, YuvFormat::P010, W, H);
    if flag(ctx, &CAP_P010) {
        assert!(result.is_ok());
    } else {
        assert!(matches!(result, Err(GpuError::Unsupported(_))));
    }
}

#[test]
fn mismatched_planes_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let wrong = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W / 2, H / 2))
        .unwrap();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    assert_eq!(
        conv.convert(ctx, &pool, canvas, luma, wrong),
        Err(GpuError::TargetMismatch(wrong))
    );
}

#[test]
fn p010_rejects_nv12_planes() {
    let Some(ctx) = common::gpu() else { return };
    if !supported(ctx, YuvFormat::P010, false) {
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let mut conv = YuvConverter::new(ctx, YuvFormat::P010, W, H).unwrap();
    assert_eq!(
        conv.convert(ctx, &pool, canvas, luma, chroma),
        Err(GpuError::TargetMismatch(luma))
    );
}

fn surface_matches_planes(format: YuvFormat) {
    let Some(ctx) = common::gpu() else { return };
    if !supported(ctx, format, true) {
        return;
    }
    let mut pool = TexturePool::new();
    // Two colours side by side, so the planes are not uniform and the layout is checked.
    let canvas = canvas(ctx, &mut pool, &[[1.0, 0.0, 0.0], [0.0, 0.2, 1.0]]);
    let mut conv = YuvConverter::new(ctx, format, W, H).unwrap();
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
    let surface = pool.acquire(ctx, &yuv::surface_desc(format, W, H)).unwrap();
    conv.convert_to_surface(ctx, &pool, canvas, surface)
        .unwrap();

    let y = readback::read_bytes(ctx, &pool, luma).unwrap();
    let uv = readback::read_bytes(ctx, &pool, chroma).unwrap();
    let codes_y = codes(format, &y);
    assert_ne!(
        codes_y[0],
        codes_y[(W - 1) as usize],
        "the canvas has two colours"
    );
    assert_eq!(readback::read_plane(ctx, &pool, surface, 0).unwrap(), y);
    assert_eq!(readback::read_plane(ctx, &pool, surface, 1).unwrap(), uv);
}

#[test]
fn nv12_surface_matches_the_two_plane_output() {
    surface_matches_planes(YuvFormat::Nv12);
}

#[test]
fn p010_surface_matches_the_two_plane_output() {
    surface_matches_planes(YuvFormat::P010);
}

#[test]
fn wrong_surface_is_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    assert_eq!(
        conv.convert_to_surface(ctx, &pool, canvas, luma),
        Err(GpuError::TargetMismatch(luma))
    );
}
