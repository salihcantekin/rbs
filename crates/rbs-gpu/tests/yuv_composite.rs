//! 4:2:0 layers sampled directly by the compositor (perf audit R-11 step b): plane
//! pairs in one pool slot, drawn with the layer's colour space, and decoded to RGB on
//! demand for consumers that need it. Spec: docs/specs/gpu-compositor.md, "Compositor"
//! and "YUV source decode".

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, at, solid};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, TextureId, VideoFormat};
use rbs_core::time::Timestamp;
use rbs_gpu::context::CAP_P010;
use rbs_gpu::pool::is_plane_pair;
use rbs_gpu::yuv_decode::YuvToRgb;
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, LayerShape, LayerStyle, TextureDesc,
    TexturePool, YuvConverter, YuvFormat, readback, yuv,
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

fn has_p010(ctx: &GpuContext) -> bool {
    ctx.capabilities().get(&CAP_P010) == Some(&CapabilityValue::Flag(true))
}

fn compositor(ctx: &GpuContext, layers: u32) -> Compositor {
    Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        layers,
    )
    .unwrap()
}

fn draw(ctx: &GpuContext, pool: &mut TexturePool, layers: &[Layer]) -> (Vec<[f32; 4]>, u32) {
    let mut comp = compositor(ctx, layers.len() as u32);
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let stats = comp.render(ctx, pool, layers, canvas).unwrap();
    let px = readback::read_linear(ctx, pool, canvas).unwrap();
    pool.release(canvas).unwrap();
    (px, stats.skipped_layers)
}

fn planar_format(format: YuvFormat) -> wgpu::TextureFormat {
    match format {
        YuvFormat::Nv12 => wgpu::TextureFormat::NV12,
        YuvFormat::P010 => wgpu::TextureFormat::P010,
    }
}

/// A 4:2:0 pool slot (plane pair) holding the encoder conversion of a solid colour.
fn picture(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    conv: &mut YuvConverter,
    format: YuvFormat,
    rgb: [f32; 3],
) -> TextureId {
    let src = solid(ctx, pool, 2, 2, [rgb[0], rgb[1], rgb[2], 1.0]);
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let mut comp = compositor(ctx, 1);
    comp.render(
        ctx,
        pool,
        &[Layer::new(src, 0.0, 0.0, W as f32, H as f32)],
        canvas,
    )
    .unwrap();
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    conv.convert(ctx, pool, canvas, luma, chroma).unwrap();
    let mut desc = TextureDesc::planar(W, H, planar_format(format));
    desc.usage |= wgpu::TextureUsages::COPY_SRC;
    assert!(is_plane_pair(&desc));
    let slot = pool.acquire(ctx, &desc).unwrap();
    let mut encoder = ctx
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    for (plane, from) in [(0, luma), (1, chroma)] {
        let from = pool.get(from).unwrap().texture();
        let to = pool.get(slot).unwrap().plane_texture(plane).unwrap();
        encoder.copy_texture_to_texture(from.as_image_copy(), to.as_image_copy(), from.size());
    }
    ctx.queue().submit(Some(encoder.finish()));
    for id in [src, canvas, luma, chroma] {
        pool.release(id).unwrap();
    }
    slot
}

fn layer(slot: TextureId) -> Layer {
    Layer::new(slot, 0.0, 0.0, W as f32, H as f32)
}

/// Encoder planes of a solid colour, drawn as a layer: the original colour again.
fn round_trip(format: YuvFormat, tol: f32) {
    let Some(ctx) = common::gpu() else { return };
    if format == YuvFormat::P010 && !has_p010(ctx) {
        eprintln!("SKIPPED: device lacks gpu.p010");
        return;
    }
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, format, W, H).unwrap();
    for rgb in COLOURS {
        let slot = picture(ctx, &mut pool, &mut conv, format, rgb);
        let (px, skipped) = draw(ctx, &mut pool, &[layer(slot)]);
        assert_eq!(skipped, 0);
        for (x, y) in [(0, 0), (7, 3), (W - 1, H - 1)] {
            assert_close(at(&px, W, x, y), [rgb[0], rgb[1], rgb[2], 1.0], tol);
        }
        pool.release(slot).unwrap();
    }
}

#[test]
fn nv12_layers_draw_their_colour() {
    round_trip(YuvFormat::Nv12, 0.02);
}

#[test]
fn p010_layers_draw_their_colour() {
    round_trip(YuvFormat::P010, 0.01);
}

#[test]
fn yuv_and_rgb_layers_mix_and_opacity_applies() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let red = picture(ctx, &mut pool, &mut conv, YuvFormat::Nv12, [1.0, 0.0, 0.0]);
    let green = solid(ctx, &mut pool, 2, 2, [0.0, 1.0, 0.0, 1.0]);
    let mut half = Layer::new(red, W as f32 / 2.0, 0.0, W as f32 / 2.0, H as f32);
    half.opacity = 0.5;
    // RGB, then 4:2:0, then RGB again: the pipeline switches both ways.
    let layers = [
        Layer::new(green, 0.0, 0.0, W as f32 / 2.0, H as f32),
        half,
        Layer::new(green, 0.0, 0.0, 2.0, 2.0),
    ];
    let (px, skipped) = draw(ctx, &mut pool, &layers);
    assert_eq!(skipped, 0);
    assert_close(at(&px, W, 4, 4), [0.0, 1.0, 0.0, 1.0], 0.02);
    assert_close(at(&px, W, 12, 4), [0.5, 0.0, 0.0, 0.5], 0.02);
    assert_close(at(&px, W, 0, 0), [0.0, 1.0, 0.0, 1.0], 0.02);
}

#[test]
fn styled_yuv_layers_keep_their_shape() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let white = picture(ctx, &mut pool, &mut conv, YuvFormat::Nv12, [1.0, 1.0, 1.0]);
    let mut l = layer(white);
    l.style = LayerStyle::new(LayerShape::Ellipse, 0.0, [0; 4], [W as f32, H as f32]);
    let (px, _) = draw(ctx, &mut pool, &[l]);
    // Inside the ellipse: the picture; the corner: nothing.
    assert_close(at(&px, W, 8, 4), [1.0, 1.0, 1.0, 1.0], 0.02);
    assert_close(at(&px, W, 0, 0), [0.0; 4], 0.02);
}

#[test]
fn a_colour_space_the_decode_cannot_handle_skips_the_layer() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let slot = picture(ctx, &mut pool, &mut conv, YuvFormat::Nv12, [1.0, 1.0, 1.0]);
    let mut l = layer(slot);
    l.ycbcr = ColorSpace::BT2100_PQ;
    let (px, skipped) = draw(ctx, &mut pool, &[l]);
    assert_eq!(skipped, 1);
    assert_close(at(&px, W, 8, 4), [0.0; 4], 0.0);
}

#[test]
fn plane_pairs_need_even_sizes() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let desc = TextureDesc::planar(W + 1, H, wgpu::TextureFormat::NV12);
    assert!(matches!(
        pool.acquire(ctx, &desc),
        Err(GpuError::InvalidSize { .. })
    ));
    // A planar slot has no RGB bind group: RGB consumers must decode it.
    let slot = pool
        .acquire(ctx, &TextureDesc::planar(W, H, wgpu::TextureFormat::NV12))
        .unwrap();
    let tex = pool.get(slot).unwrap();
    assert!(tex.is_planar());
    assert!(tex.sampled_bind_group().is_none());
    assert_eq!(tex.plane_texture(1).unwrap().width(), W / 2);
}

#[test]
fn rgb_of_a_yuv_frame_is_decoded_once_per_picture() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let slot = picture(ctx, &mut pool, &mut conv, YuvFormat::Nv12, [0.0, 1.0, 0.0]);
    let format = VideoFormat::new(
        Size::new(W, H),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let frame = pool
        .frame(slot, format, Timestamp::from_nanos(5))
        .unwrap()
        .with_sequence(7);
    assert!(YuvToRgb::needs_decode(&pool, &frame));
    let mut rgb = YuvToRgb::new();
    let first = rgb.rgb(ctx, &mut pool, &frame).unwrap();
    assert_eq!(first.format().pixel_format, PixelFormat::Rgba8Srgb);
    assert_eq!(
        (first.timestamp(), first.sequence()),
        (frame.timestamp(), 7)
    );
    let FramePayload::Texture(id) = *first.payload() else {
        panic!("not a texture");
    };
    assert!(!YuvToRgb::needs_decode(&pool, first));
    let px = readback::read_linear(ctx, &pool, id).unwrap();
    assert_close(px[0], [0.0, 1.0, 0.0, 1.0], 0.02);
    // The same picture again: the kept frame, no new decode.
    let again = rgb.rgb(ctx, &mut pool, &frame).unwrap();
    assert_eq!(*again.payload(), FramePayload::Texture(id));
    // A new picture of the same slot decodes again.
    let next = pool
        .frame(slot, format, Timestamp::from_nanos(6))
        .unwrap()
        .with_sequence(8);
    let next_rgb = rgb.rgb(ctx, &mut pool, &next).unwrap();
    assert_eq!(next_rgb.sequence(), 8);
    rgb.release();
}

#[test]
fn scrolling_yuv_layers_wrap_with_their_gap() {
    use rbs_gpu::{
        LayerScroll, ScrollBinding, ScrollDirection, ScrollRepeat, ScrollRun, ScrollStart, UvRect,
    };
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let red = picture(ctx, &mut pool, &mut conv, YuvFormat::Nv12, [1.0, 0.0, 0.0]);
    let mut l = layer(red);
    // The picture is 16 long, the gap 16: 8 travelled shows picture then gap.
    let spec = LayerScroll {
        direction: ScrollDirection::Left,
        repeat: ScrollRepeat::Loop,
        start: ScrollStart::Inside,
        gap: 16.0,
        fade: 0.0,
        run: ScrollRun::start(Timestamp::ZERO, 8.0),
    };
    let frame = spec
        .frame(
            &l.transform,
            &UvRect::FULL,
            W,
            H,
            Timestamp::from_nanos(1_000_000_000),
        )
        .unwrap();
    l.scroll = Some(frame.gpu(spec.direction, spec.repeat, spec.fade));
    let binding = ScrollBinding::new(ctx);
    let mut comp = compositor(ctx, 1);
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let stats = comp
        .render_scrolled(ctx, &pool, &[l], canvas, Some(&binding))
        .unwrap();
    assert_eq!(stats.drawn_layers, 1);
    let px = readback::read_linear(ctx, &pool, canvas).unwrap();
    assert_close(at(&px, W, 3, 4), [1.0, 0.0, 0.0, 1.0], 0.02);
    assert_close(at(&px, W, 12, 4), [0.0; 4], 0.02);
}
