//! The image download (ADR-0117 decision 5; spec docs/specs/gpu-compositor.md, "Image
//! download (RGBA8)"): premultiplied linear pictures are un-premultiplied and encoded
//! sRGB, straight sRGB `Rgba8` pictures are copied as they are.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::{GpuContext, RgbaDownload, TextureDesc, TextureId, TexturePool, readback};

fn premultiplied(pool: &TexturePool, id: TextureId, w: u32, h: u32) -> VideoFrame {
    let format = VideoFormat::new(
        Size::new(w, h),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    pool.frame(id, format, Timestamp::ZERO).unwrap()
}

fn take(ctx: &GpuContext, down: &mut RgbaDownload) -> VideoFrame {
    ctx.wait_idle().unwrap();
    ctx.poll();
    down.take(ctx).unwrap().expect("a completed picture")
}

fn pixels(frame: &VideoFrame) -> (Vec<u8>, usize) {
    let FramePayload::System(sys) = frame.payload() else {
        panic!("not a system frame");
    };
    let (bytes, stride) = sys.plane(0).unwrap();
    (bytes.to_vec(), stride)
}

fn srgb_code(linear: f64) -> u8 {
    let c = if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (c * 255.0).round() as u8
}

#[test]
fn a_premultiplied_picture_becomes_straight_srgb() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    // Linear 0.5 grey at half coverage, premultiplied: 0.25 per channel, alpha 0.5.
    let (w, h) = (5, 3);
    let id = solid(ctx, &mut pool, w, h, [0.25, 0.25, 0.25, 0.5]);
    let mut down = RgbaDownload::new(ctx, w, h).unwrap();
    let copied = ctx.metrics().frames_cpu_copied();
    let picture = premultiplied(&pool, id, w, h);
    assert!(
        down.submit_at(ctx, &pool, &picture, Timestamp::from_nanos(9), 4)
            .unwrap()
    );
    // One readback at a time: a second change waits for the first.
    assert!(
        !down
            .submit_at(ctx, &pool, &picture, Timestamp::ZERO, 5)
            .unwrap()
    );
    assert_eq!(down.stats().busy, 1);

    let frame = take(ctx, &mut down);
    assert_eq!(frame.timestamp(), Timestamp::from_nanos(9));
    assert_eq!(frame.sequence(), 4);
    assert_eq!(
        *frame.format(),
        VideoFormat::new(Size::new(w, h), PixelFormat::Rgba8, ColorSpace::SRGB)
    );
    let (px, stride) = pixels(&frame);
    assert_eq!(stride, w as usize * 4);
    assert_eq!(px.len(), (w * h * 4) as usize);
    let grey = srgb_code(0.5);
    for texel in px.chunks_exact(4) {
        for c in &texel[..3] {
            assert!(c.abs_diff(grey) <= 1, "{texel:?}, want {grey}");
        }
        assert!(texel[3].abs_diff(128) <= 1, "{texel:?}");
    }
    assert!(ctx.metrics().frames_cpu_copied() > copied);
    assert!(!down.in_flight());
    let stats = down.stats();
    assert_eq!((stats.converted, stats.copied, stats.delivered), (1, 0, 1));
}

#[test]
fn a_transparent_pixel_keeps_no_colour() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let id = solid(ctx, &mut pool, 2, 2, [0.0; 4]);
    let mut down = RgbaDownload::new(ctx, 2, 2).unwrap();
    let picture = premultiplied(&pool, id, 2, 2);
    assert!(
        down.submit_at(ctx, &pool, &picture, Timestamp::ZERO, 1)
            .unwrap()
    );
    let (px, _) = pixels(&take(ctx, &mut down));
    assert!(px.iter().all(|b| *b == 0), "{px:?}");
}

#[test]
fn a_straight_srgb_picture_is_copied_as_it_is() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (w, h) = (3, 2);
    let mut desc = TextureDesc::source(w, h, wgpu::TextureFormat::Rgba8UnormSrgb);
    desc.usage |= wgpu::TextureUsages::COPY_SRC;
    let id = pool.acquire(ctx, &desc).unwrap();
    // Straight alpha: the colour stays as written whatever the alpha.
    let data: Vec<u8> = (0..w * h)
        .flat_map(|i| [10 * i as u8, 200, 30, 64 + i as u8])
        .collect();
    readback::upload(ctx, &pool, id, &data).unwrap();
    let format = VideoFormat::new(Size::new(w, h), PixelFormat::Rgba8, ColorSpace::SRGB);
    let picture = pool.frame(id, format, Timestamp::ZERO).unwrap();
    let mut down = RgbaDownload::new(ctx, w, h).unwrap();
    assert!(
        down.submit_at(ctx, &pool, &picture, Timestamp::ZERO, 7)
            .unwrap()
    );
    let (px, _) = pixels(&take(ctx, &mut down));
    assert_eq!(px, data);
    let stats = down.stats();
    assert_eq!((stats.converted, stats.copied), (0, 1));
}

#[test]
fn a_larger_picture_grows_the_download_once_and_a_smaller_one_reuses_it() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut down = RgbaDownload::new(ctx, 4, 4).unwrap();
    for (w, h) in [(4, 4), (80, 20), (8, 8), (80, 20)] {
        let id = solid(ctx, &mut pool, w, h, [1.0, 0.0, 0.0, 1.0]);
        let picture = premultiplied(&pool, id, w, h);
        assert!(
            down.submit_at(ctx, &pool, &picture, Timestamp::ZERO, 1)
                .unwrap()
        );
        let frame = take(ctx, &mut down);
        assert_eq!(frame.format().size, Size::new(w, h));
        let (px, _) = pixels(&frame);
        assert!(
            px.chunks_exact(4).all(|t| t == [255, 0, 0, 255]),
            "{w}x{h}: {:?}",
            &px[..8]
        );
    }
    // The first picture builds the pass target; the 80x20 one grows both the target
    // and the staging buffer; the rest reuse them.
    assert_eq!(down.stats().grown, 2);
}

#[test]
fn other_formats_are_refused() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let id = pool
        .acquire(
            ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Bgra8Unorm),
        )
        .unwrap();
    let format = VideoFormat::new(Size::new(2, 2), PixelFormat::Bgra8, ColorSpace::SRGB);
    let picture = pool.frame(id, format, Timestamp::ZERO).unwrap();
    let mut down = RgbaDownload::new(ctx, 2, 2).unwrap();
    assert!(
        down.submit_at(ctx, &pool, &picture, Timestamp::ZERO, 1)
            .is_err()
    );
    assert!(!down.in_flight());
}
