//! NV12 and P010 conversion: BT.709 limited-range values for known colours (NV12 within ±1
//! of 8-bit codes, P010 within ±2 of 10-bit codes), single surfaces equal to the plane
//! output. Spec: docs/specs/gpu-compositor.md, "Encoder conversion (NV12, P010)".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use rbs_core::capability::{CapabilityKey, CapabilityValue};
use rbs_core::media::ColorSpace;
use rbs_gpu::context::{CAP_NV12_SURFACE, CAP_P010, CAP_P010_SURFACE};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, ScaleKernel, TextureDesc, TextureId,
    TexturePool, YuvConverter, YuvFormat, readback, yuv,
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
fn oetf_srgb(l: f64) -> f64 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.0031308 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Reference sRGB full-range (Y, Cb, Cr) codes for a linear RGB colour.
fn reference_screen(format: YuvFormat, rgb: [f64; 3]) -> [u16; 3] {
    let max = f64::from((1u32 << bits(format)) - 1);
    let mid = f64::from(1u32 << (bits(format) - 1));
    let [r, g, b] = rgb.map(oetf_srgb);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    let code = |v: f64| v.round().clamp(0.0, max) as u16;
    [code(max * y), code(mid + max * cb), code(mid + max * cr)]
}

/// Open shadows: a power of 0.7 about middle grey, identity at and above it.
fn open_shadows_linear(l: f64) -> f64 {
    const KNEE: f64 = 0.18;
    const GAMMA: f64 = 0.7;
    let l = l.clamp(0.0, 1.0);
    if l > 0.0 && l < KNEE {
        KNEE * (l / KNEE).powf(GAMMA)
    } else {
        l
    }
}

fn reference_open(format: YuvFormat, rgb: [f64; 3]) -> [u16; 3] {
    reference(format, rgb.map(open_shadows_linear))
}

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

fn check_color(format: YuvFormat, color: ColorSpace, rgb: [f32; 3], want: [u16; 3]) {
    let Some(ctx) = common::gpu() else { return };
    if !supported(ctx, format, false) {
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool, &[rgb]);
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    let mut conv = YuvConverter::with_color(ctx, format, W, H, color).unwrap();
    conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
    let y = codes(format, &readback::read_bytes(ctx, &pool, luma).unwrap());
    let uv = codes(format, &readback::read_bytes(ctx, &pool, chroma).unwrap());
    let got = [y[0], uv[0], uv[1]];
    let tolerance = if bits(format) == 8 { 1 } else { 2 };
    for i in 0..3 {
        assert!(
            (i32::from(got[i]) - i32::from(want[i])).abs() <= tolerance,
            "{format:?} {color:?} {rgb:?}: got YCbCr {got:?}, want {want:?}"
        );
    }
}

fn check_open(format: YuvFormat, rgb: [f32; 3]) {
    let Some(ctx) = common::gpu() else { return };
    if !supported(ctx, format, false) {
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool, &[rgb]);
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    let mut conv =
        YuvConverter::with_tone(ctx, format, W, H, ColorSpace::BT709_LIMITED, true).unwrap();
    conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
    let y = codes(format, &readback::read_bytes(ctx, &pool, luma).unwrap());
    let uv = codes(format, &readback::read_bytes(ctx, &pool, chroma).unwrap());
    let got = [y[0], uv[0], uv[1]];
    let want = reference_open(
        format,
        [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])],
    );
    let tolerance = if bits(format) == 8 { 1 } else { 2 };
    for i in 0..3 {
        assert!(
            (i32::from(got[i]) - i32::from(want[i])).abs() <= tolerance,
            "{format:?} open shadows {rgb:?}: got YCbCr {got:?}, want {want:?}"
        );
    }
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
fn screen_nv12_matches_srgb_full_range() {
    for rgb in COLOURS {
        let want = reference_screen(
            YuvFormat::Nv12,
            [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])],
        );
        check_color(YuvFormat::Nv12, ColorSpace::SRGB_VIDEO, rgb, want);
    }
    // A dark UI grey sits in the BT.709 toe and would be far below this sRGB code.
    let dark = [0.013_f32, 0.013, 0.013];
    let want = reference_screen(YuvFormat::Nv12, [0.013, 0.013, 0.013]);
    assert!(
        want[0] > 20,
        "sRGB code of a dark grey stays above the video toe: {want:?}"
    );
    check_color(YuvFormat::Nv12, ColorSpace::SRGB_VIDEO, dark, want);
}

#[test]
fn screen_reference_is_full_range() {
    let nv12 = |rgb| reference_screen(YuvFormat::Nv12, rgb);
    assert_eq!(nv12([0.0, 0.0, 0.0]), [0, 128, 128]);
    assert_eq!(nv12([1.0, 1.0, 1.0]), [255, 128, 128]);
}

#[test]
fn open_shadows_lifts_only_the_dark_end() {
    let dark = [0.013, 0.013, 0.013];
    let plain = reference(YuvFormat::Nv12, dark);
    let opened = reference_open(YuvFormat::Nv12, dark);
    assert!(
        opened[0] >= plain[0] + 8,
        "a dark grey moves up by more than the quantiser step: {opened:?} vs {plain:?}"
    );
    assert_eq!(
        reference_open(YuvFormat::Nv12, [0.0, 0.0, 0.0]),
        [16, 128, 128]
    );
    assert_eq!(
        reference_open(YuvFormat::Nv12, [1.0, 1.0, 1.0]),
        [235, 128, 128]
    );
    assert_eq!(
        reference_open(YuvFormat::Nv12, [0.18, 0.18, 0.18]),
        reference(YuvFormat::Nv12, [0.18, 0.18, 0.18])
    );
    let plain10 = reference(YuvFormat::P010, dark);
    let open10 = reference_open(YuvFormat::P010, dark);
    assert!(
        open10[0] >= plain10[0] + 32,
        "the same lift, in 10-bit codes: {open10:?} vs {plain10:?}"
    );
}

#[test]
fn open_shadows_matches_on_nv12_and_p010() {
    for format in [YuvFormat::Nv12, YuvFormat::P010] {
        for rgb in [
            [0.0, 0.0, 0.0],
            [0.013, 0.013, 0.013],
            [0.18, 0.18, 0.18],
            [1.0, 1.0, 1.0],
        ] {
            check_open(format, rgb);
        }
    }
}

#[test]
fn open_shadows_on_screen_colour_is_rejected() {
    let Some(ctx) = common::gpu() else { return };
    assert!(matches!(
        YuvConverter::with_tone(ctx, YuvFormat::Nv12, W, H, ColorSpace::SRGB_VIDEO, true),
        Err(GpuError::Unsupported(_))
    ));
}

#[test]
fn other_colours_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    assert!(matches!(
        YuvConverter::with_color(ctx, YuvFormat::Nv12, W, H, ColorSpace::BT709_FULL),
        Err(GpuError::Unsupported(_))
    ));
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
        conv.convert(ctx, &pool, canvas, luma, wrong).err(),
        Some(GpuError::TargetMismatch(wrong))
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
        conv.convert(ctx, &pool, canvas, luma, chroma).err(),
        Some(GpuError::TargetMismatch(luma))
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

fn paint(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    width: u32,
    height: u32,
    layers: &[Layer],
) -> TextureId {
    let mut comp = Compositor::new(ctx, CanvasDesc { width, height }, layers.len() as u32).unwrap();
    let canvas = pool
        .acquire(ctx, &TextureDesc::canvas(width, height))
        .unwrap();
    comp.render(ctx, pool, layers, canvas).unwrap();
    canvas
}

fn flat_canvas(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    width: u32,
    height: u32,
    rgb: [f32; 3],
) -> TextureId {
    let src = solid(ctx, pool, 2, 2, [rgb[0], rgb[1], rgb[2], 1.0]);
    paint(
        ctx,
        pool,
        width,
        height,
        &[Layer::new(src, 0.0, 0.0, width as f32, height as f32)],
    )
}

/// NV12 planes from `scaled`, plus the kernel the converter built.
fn scaled_nv12(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    canvas: TextureId,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> (ScaleKernel, Vec<u16>, Vec<u16>) {
    let mut conv = YuvConverter::scaled(ctx, YuvFormat::Nv12, src_w, src_h, dst_w, dst_h).unwrap();
    let kernel = conv.kernel();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, dst_w, dst_h))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, dst_w, dst_h))
        .unwrap();
    conv.convert(ctx, pool, canvas, luma, chroma).unwrap();
    let y = codes(
        YuvFormat::Nv12,
        &readback::read_bytes(ctx, pool, luma).unwrap(),
    );
    let uv = codes(
        YuvFormat::Nv12,
        &readback::read_bytes(ctx, pool, chroma).unwrap(),
    );
    (kernel, y, uv)
}

fn near(got: u16, want: u16, what: &str) {
    assert!(
        (i32::from(got) - i32::from(want)).abs() <= 1,
        "{what}: got {got}, want {want}"
    );
}

fn luma_at(y: &[u16], width: u32, x: u32, row: u32) -> u16 {
    y[(row * width + x) as usize]
}

/// Limited-range codes of an already-encoded (OETF) colour. Chroma averages these.
fn ycbcr_prime(p: [f64; 3]) -> [u16; 3] {
    let y = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let cb = (p[2] - y) / 1.8556;
    let cr = (p[0] - y) / 1.5748;
    [
        (16.0 + 219.0 * y).round() as u16,
        (128.0 + 224.0 * cb).round() as u16,
        (128.0 + 224.0 * cr).round() as u16,
    ]
}

/// Keys cubic, `a = -0.5`.
fn keys(d: f32) -> f32 {
    let x = d.abs();
    let near = (1.5 * x - 2.5) * x * x + 1.0;
    let far = ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
    if x <= 1.0 {
        near
    } else if x <= 2.0 {
        far
    } else {
        0.0
    }
}

fn sample_clamped(src: &[[f32; 3]], sw: i32, sh: i32, x: i32, y: i32) -> [f32; 3] {
    let x = x.clamp(0, sw - 1);
    let y = y.clamp(0, sh - 1);
    src[(y * sw + x) as usize]
}

/// Bilinear in linear light at an output pixel centre, matching `fs_luma_scaled`.
fn bilinear_at(src: &[[f32; 3]], sw: i32, sh: i32, fit: [f32; 4], p: [f32; 2]) -> [f32; 3] {
    let uvx = (p[0] - fit[0]) / fit[2];
    let uvy = (p[1] - fit[1]) / fit[3];
    if uvx < 0.0 || uvy < 0.0 || uvx > 1.0 || uvy > 1.0 {
        return [0.0, 0.0, 0.0];
    }
    let x = uvx * sw as f32 - 0.5;
    let y = uvy * sh as f32 - 0.5;
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let mut acc = [0.0_f32; 3];
    for dy in 0..2 {
        let wy = if dy == 0 { 1.0 - fy } else { fy };
        for dx in 0..2 {
            let wx = if dx == 0 { 1.0 - fx } else { fx };
            let tex = sample_clamped(src, sw, sh, x0 + dx, y0 + dy);
            for (channel, sample) in acc.iter_mut().zip(tex) {
                *channel += sample * wx * wy;
            }
        }
    }
    acc
}

/// Catmull-Rom in linear light at an output pixel centre, matching `fs_luma_catmull`.
fn catmull_at(src: &[[f32; 3]], sw: i32, sh: i32, fit: [f32; 4], p: [f32; 2]) -> [f32; 3] {
    let uvx = (p[0] - fit[0]) / fit[2];
    let uvy = (p[1] - fit[1]) / fit[3];
    if uvx < 0.0 || uvy < 0.0 || uvx > 1.0 || uvy > 1.0 {
        return [0.0, 0.0, 0.0];
    }
    let x = uvx * sw as f32 - 0.5;
    let y = uvy * sh as f32 - 0.5;
    let bx = x.floor();
    let by = y.floor();
    let fx = x - bx;
    let fy = y - by;
    let mut acc = [0.0_f32; 3];
    for iy in -1..=2 {
        let wy = keys(fy - iy as f32);
        for ix in -1..=2 {
            let wx = keys(fx - ix as f32);
            let tex = sample_clamped(src, sw, sh, bx as i32 + ix, by as i32 + iy);
            for (channel, sample) in acc.iter_mut().zip(tex) {
                *channel += sample * wx * wy;
            }
        }
    }
    acc
}

fn fit32(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> [f32; 4] {
    yuv::fit_rect(src_w, src_h, dst_w, dst_h).map(|v| v as f32)
}

#[test]
fn equal_size_matches_the_one_to_one_frame() {
    // The choice itself does not need a device: equal sizes stay on textureLoad.
    assert_eq!(yuv::scale_kernel(8, 4, 8, 4), ScaleKernel::OneToOne);
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let green = solid(ctx, &mut pool, 2, 2, [0.0, 1.0, 0.0, 1.0]);
    let canvas = paint(
        ctx,
        &mut pool,
        8,
        4,
        &[
            Layer::new(red, 0.0, 0.0, 4.0, 4.0),
            Layer::new(green, 4.0, 0.0, 4.0, 4.0),
        ],
    );
    let mut read = |conv: &mut YuvConverter| {
        let luma = pool
            .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, 8, 4))
            .unwrap();
        let chroma = pool
            .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, 8, 4))
            .unwrap();
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
        let mut bytes = readback::read_bytes(ctx, &pool, luma).unwrap();
        bytes.extend(readback::read_bytes(ctx, &pool, chroma).unwrap());
        bytes
    };
    let mut one_to_one = YuvConverter::new(ctx, YuvFormat::Nv12, 8, 4).unwrap();
    let mut scaled = YuvConverter::scaled(ctx, YuvFormat::Nv12, 8, 4, 8, 4).unwrap();
    assert_eq!(one_to_one.kernel(), ScaleKernel::OneToOne);
    assert_eq!(scaled.kernel(), ScaleKernel::OneToOne);
    let direct = read(&mut one_to_one);
    let same = read(&mut scaled);
    assert_eq!(direct, same, "equal sizes stay on the 1:1 frame");
    // The two halves are different, so a kernel that averaged them would not match.
    assert_ne!(direct[0], direct[7]);
}

#[test]
fn integer_downscale_by_two_box_averages_flat_blocks() {
    assert_eq!(yuv::scale_kernel(4, 4, 2, 2), ScaleKernel::Box(2));
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let white = solid(ctx, &mut pool, 2, 2, [1.0, 1.0, 1.0, 1.0]);
    let green = solid(ctx, &mut pool, 2, 2, [0.0, 1.0, 0.0, 1.0]);
    // 4×4 canvas, 2×2 output. (0,0) flat red; (1,0) a black/white checker;
    // (0,1) flat white; (1,1) flat green. Clear is black.
    let canvas = paint(
        ctx,
        &mut pool,
        4,
        4,
        &[
            Layer::new(red, 0.0, 0.0, 2.0, 2.0),
            Layer::new(white, 3.0, 0.0, 1.0, 1.0),
            Layer::new(white, 2.0, 1.0, 1.0, 1.0),
            Layer::new(white, 0.0, 2.0, 2.0, 2.0),
            Layer::new(green, 2.0, 2.0, 2.0, 2.0),
        ],
    );
    let (kernel, y, uv) = scaled_nv12(ctx, &mut pool, canvas, 4, 4, 2, 2);
    assert_eq!(kernel, ScaleKernel::Box(2));
    let red_code = reference(YuvFormat::Nv12, [1.0, 0.0, 0.0]);
    let grey = reference(YuvFormat::Nv12, [0.5, 0.5, 0.5]);
    let white_code = reference(YuvFormat::Nv12, [1.0, 1.0, 1.0]);
    let green_code = reference(YuvFormat::Nv12, [0.0, 1.0, 0.0]);
    near(luma_at(&y, 2, 0, 0), red_code[0], "flat red block");
    near(luma_at(&y, 2, 1, 0), grey[0], "checker mean");
    near(luma_at(&y, 2, 0, 1), white_code[0], "flat white block");
    near(luma_at(&y, 2, 1, 1), green_code[0], "flat green block");
    // A flat block stays that colour. The checker is the linear mean, not the
    // mean of the two codes (OETF is not linear: 16 and 235 do not average to grey).
    assert!((i32::from(grey[0]) - (16 + 235) / 2).abs() > 20);
    let primes = [
        [1.0, 0.0, 0.0],
        [0.5, 0.5, 0.5],
        [1.0, 1.0, 1.0],
        [0.0, 1.0, 0.0],
    ]
    .map(|rgb| rgb.map(oetf));
    let mut mean = [0.0; 3];
    for prime in primes {
        for (channel, sample) in mean.iter_mut().zip(prime) {
            *channel += sample;
        }
    }
    for channel in &mut mean {
        *channel /= 4.0;
    }
    let chroma = ycbcr_prime(mean);
    near(uv[0], chroma[1], "chroma Cb after the four luma kernels");
    near(uv[1], chroma[2], "chroma Cr after the four luma kernels");
}

#[test]
fn integer_downscale_by_three_is_the_block_mean() {
    // A 2× sample at the block centre has the same weights as a box. 3× does not:
    // bilinear returns the centre texel, the box returns the mean of all nine.
    assert_eq!(yuv::scale_kernel(6, 6, 2, 2), ScaleKernel::Box(3));
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let white = solid(ctx, &mut pool, 2, 2, [1.0, 1.0, 1.0, 1.0]);
    let red = solid(ctx, &mut pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let canvas = paint(
        ctx,
        &mut pool,
        6,
        6,
        &[
            Layer::new(white, 1.0, 1.0, 1.0, 1.0),
            Layer::new(red, 3.0, 0.0, 3.0, 3.0),
        ],
    );
    let (kernel, y, _) = scaled_nv12(ctx, &mut pool, canvas, 6, 6, 2, 2);
    assert_eq!(kernel, ScaleKernel::Box(3));
    let mean = reference(YuvFormat::Nv12, [1.0 / 9.0, 1.0 / 9.0, 1.0 / 9.0]);
    let centre = reference(YuvFormat::Nv12, [1.0, 1.0, 1.0]);
    let red_code = reference(YuvFormat::Nv12, [1.0, 0.0, 0.0]);
    near(
        luma_at(&y, 2, 0, 0),
        mean[0],
        "3×3 block with one white texel",
    );
    assert!(
        (i32::from(luma_at(&y, 2, 0, 0)) - i32::from(centre[0])).abs() > 20,
        "the box mean is not the centre texel"
    );
    near(luma_at(&y, 2, 1, 0), red_code[0], "flat red 3×3 block");
}

#[test]
fn upscale_catmull_rom_keeps_an_edge_sharper_than_bilinear() {
    // 4×2 canvas, left half black, right half white, drawn at 8×4.
    let sw = 4i32;
    let sh = 2i32;
    let mut src = vec![[0.0_f32; 3]; (sw * sh) as usize];
    for y in 0..sh {
        for x in 2..sw {
            src[(y * sw + x) as usize] = [1.0, 1.0, 1.0];
        }
    }
    let fit = fit32(4, 2, 8, 4);
    let at = |x: u32| [x as f32 + 0.5, 0.5];
    let cat_lo = catmull_at(&src, sw, sh, fit, at(3));
    let cat_hi = catmull_at(&src, sw, sh, fit, at(4));
    let bil_lo = bilinear_at(&src, sw, sh, fit, at(3));
    let bil_hi = bilinear_at(&src, sw, sh, fit, at(4));
    assert!((f64::from(cat_lo[0]) - 0.203125).abs() < 1e-5);
    assert!((f64::from(cat_hi[0]) - 0.796875).abs() < 1e-5);
    assert!((f64::from(bil_lo[0]) - 0.25).abs() < 1e-5);
    assert!((f64::from(bil_hi[0]) - 0.75).abs() < 1e-5);
    let y_of = |rgb: [f32; 3]| {
        reference(
            YuvFormat::Nv12,
            [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])],
        )[0]
    };
    let cat_gap = i32::from(y_of(cat_hi)) - i32::from(y_of(cat_lo));
    let bil_gap = i32::from(y_of(bil_hi)) - i32::from(y_of(bil_lo));
    assert!(
        cat_gap >= bil_gap + 4,
        "Catmull-Rom reference gap {cat_gap} should clear the bilinear gap {bil_gap}"
    );
    assert_eq!(yuv::scale_kernel(4, 2, 8, 4), ScaleKernel::CatmullRom);

    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let white = solid(ctx, &mut pool, 2, 2, [1.0, 1.0, 1.0, 1.0]);
    let canvas = paint(
        ctx,
        &mut pool,
        4,
        2,
        &[Layer::new(white, 2.0, 0.0, 2.0, 2.0)],
    );
    let (kernel, y, _) = scaled_nv12(ctx, &mut pool, canvas, 4, 2, 8, 4);
    assert_eq!(kernel, ScaleKernel::CatmullRom);
    let black = reference(YuvFormat::Nv12, [0.0, 0.0, 0.0])[0];
    let white_code = reference(YuvFormat::Nv12, [1.0, 1.0, 1.0])[0];
    near(
        luma_at(&y, 8, 0, 0),
        black,
        "far side of the edge stays black",
    );
    near(
        luma_at(&y, 8, 7, 0),
        white_code,
        "far side of the edge stays white",
    );
    near(luma_at(&y, 8, 3, 0), y_of(cat_lo), "black side of the edge");
    near(luma_at(&y, 8, 4, 0), y_of(cat_hi), "white side of the edge");
    let got_gap = i32::from(luma_at(&y, 8, 4, 0)) - i32::from(luma_at(&y, 8, 3, 0));
    assert!(
        got_gap > bil_gap,
        "upscaled edge gap {got_gap} should be sharper than bilinear {bil_gap}"
    );
}

#[test]
fn non_integer_downscale_matches_bilinear() {
    assert_eq!(yuv::scale_kernel(8, 8, 6, 6), ScaleKernel::Bilinear);
    // Vertical stripes: even columns black, odd columns white. 8 → 6 is 4/3.
    let sw = 8i32;
    let sh = 8i32;
    let mut src = vec![[0.0_f32; 3]; (sw * sh) as usize];
    for y in 0..sh {
        for x in 0..sw {
            if x % 2 == 1 {
                src[(y * sw + x) as usize] = [1.0, 1.0, 1.0];
            }
        }
    }
    let fit = fit32(8, 8, 6, 6);
    let mut blended = false;
    let mut expect_y = Vec::new();
    for row in 0..6u32 {
        for x in 0..6u32 {
            let linear = bilinear_at(&src, sw, sh, fit, [x as f32 + 0.5, row as f32 + 0.5]);
            let code = reference(
                YuvFormat::Nv12,
                [
                    f64::from(linear[0]),
                    f64::from(linear[1]),
                    f64::from(linear[2]),
                ],
            )[0];
            if code != 16 && code != 235 {
                blended = true;
            }
            expect_y.push(code);
        }
    }
    assert!(blended, "the stripe pattern must actually filter");

    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let white = solid(ctx, &mut pool, 2, 2, [1.0, 1.0, 1.0, 1.0]);
    let layers: Vec<Layer> = (0..4)
        .map(|i| Layer::new(white, (i * 2 + 1) as f32, 0.0, 1.0, 8.0))
        .collect();
    let canvas = paint(ctx, &mut pool, 8, 8, &layers);
    let pixels = readback::read_rgba16f(ctx, &pool, canvas).unwrap();
    for row in 0..8u32 {
        for x in 0..8u32 {
            let want = if x % 2 == 1 { 1.0 } else { 0.0 };
            let px = common::at(&pixels, 8, x, row);
            assert!(
                (px[0] - want).abs() < 0.01 && (px[1] - want).abs() < 0.01,
                "canvas stripe {x},{row}: {px:?}"
            );
        }
    }
    let (kernel, y, uv) = scaled_nv12(ctx, &mut pool, canvas, 8, 8, 6, 6);
    assert_eq!(kernel, ScaleKernel::Bilinear);
    for (i, want) in expect_y.iter().enumerate() {
        near(y[i], *want, &format!("bilinear luma {i}"));
    }
    for cy in 0..3u32 {
        for cx in 0..3u32 {
            let mut mean = [0.0_f64; 3];
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let linear = bilinear_at(
                        &src,
                        sw,
                        sh,
                        fit,
                        [(cx * 2 + dx) as f32 + 0.5, (cy * 2 + dy) as f32 + 0.5],
                    );
                    let prime = [
                        oetf(f64::from(linear[0])),
                        oetf(f64::from(linear[1])),
                        oetf(f64::from(linear[2])),
                    ];
                    for (channel, sample) in mean.iter_mut().zip(prime) {
                        *channel += sample;
                    }
                }
            }
            for channel in &mut mean {
                *channel /= 4.0;
            }
            let want = ycbcr_prime(mean);
            let i = ((cy * 3 + cx) * 2) as usize;
            near(uv[i], want[1], &format!("bilinear chroma Cb {cx},{cy}"));
            near(uv[i + 1], want[2], &format!("bilinear chroma Cr {cx},{cy}"));
        }
    }
}

#[test]
fn scaled_colour_modes_build_the_matching_kernel() {
    // Flat fields: every kernel reproduces the colour, so a wrong entry point
    // (plain instead of open, limited instead of sRGB) fails the code check.
    let dark = [0.013_f32, 0.013, 0.013];
    let red = [1.0_f32, 0.0, 0.0];
    let cases = [
        (4u32, 2u32, false, true, dark, ScaleKernel::Box(2)),
        (4, 8, false, true, dark, ScaleKernel::CatmullRom),
        (8, 6, false, true, dark, ScaleKernel::Bilinear),
        (4, 2, true, false, red, ScaleKernel::Box(2)),
        (4, 8, true, false, red, ScaleKernel::CatmullRom),
        (8, 6, true, false, red, ScaleKernel::Bilinear),
    ];
    for (src, dst, _, _, _, want_kernel) in cases {
        assert_eq!(
            yuv::scale_kernel(src, src, dst, dst),
            want_kernel,
            "{src} -> {dst}"
        );
    }
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    for (src, dst, screen, open, rgb, want_kernel) in cases {
        let canvas = flat_canvas(ctx, &mut pool, src, src, rgb);
        let color = if screen {
            ColorSpace::SRGB_VIDEO
        } else {
            ColorSpace::BT709_LIMITED
        };
        let mut conv =
            YuvConverter::scaled_tone(ctx, YuvFormat::Nv12, src, src, dst, dst, color, open)
                .unwrap();
        assert_eq!(conv.kernel(), want_kernel);
        let luma = pool
            .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, dst, dst))
            .unwrap();
        let chroma = pool
            .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, dst, dst))
            .unwrap();
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
        let y = codes(
            YuvFormat::Nv12,
            &readback::read_bytes(ctx, &pool, luma).unwrap(),
        );
        let linear = [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])];
        let want = if open {
            reference_open(YuvFormat::Nv12, linear)[0]
        } else {
            reference_screen(YuvFormat::Nv12, linear)[0]
        };
        let plain = reference(YuvFormat::Nv12, linear)[0];
        assert!(
            (i32::from(want) - i32::from(plain)).abs() > 4,
            "the colour mode must differ from plain video, or this check is vacuous"
        );
        near(y[0], want, "first pixel");
        near(y[y.len() - 1], want, "last pixel");
    }
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
        conv.convert_to_surface(ctx, &pool, canvas, luma).err(),
        Some(GpuError::TargetMismatch(luma))
    );
}
