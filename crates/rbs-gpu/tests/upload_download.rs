//! System-memory upload of source frames and the non-blocking NV12 download for encoders
//! (spec docs/specs/gpu-compositor.md, "System-memory upload" and "System-memory
//! download").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::{assert_close, solid};
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat,
    VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::download::{DOWNLOAD_BUFFERS, DOWNLOAD_RING};
use rbs_gpu::{SourceUpload, TexturePool, YuvDownload, YuvFormat, readback};

const W: u32 = 32;
const H: u32 = 16;

fn canvas_frame(pool: &TexturePool, id: rbs_gpu::TextureId, t: u64) -> VideoFrame {
    let format = VideoFormat::new(
        Size::new(W, H),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    pool.frame(id, format, Timestamp::from_nanos(t))
        .unwrap()
        .with_sequence(t)
}

#[test]
fn download_delivers_nv12_codes_and_counts_the_cpu_copy() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    // Linear 0.5 grey: Y = 16 + 219 * oetf(0.5), neutral chroma.
    let canvas = solid(ctx, &mut pool, W, H, [0.5, 0.5, 0.5, 1.0]);
    let mut down = YuvDownload::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let copied = ctx.metrics().frames_cpu_copied();

    assert!(
        down.submit(ctx, &pool, &canvas_frame(&pool, canvas, 7))
            .unwrap()
    );
    ctx.wait_idle().unwrap();
    ctx.poll();
    let frame = down.take_ready(ctx).unwrap().expect("a completed frame");
    assert_eq!(frame.timestamp(), Timestamp::from_nanos(7));
    assert_eq!(frame.sequence(), 7);
    assert_eq!(*frame.format(), *down.format());
    let FramePayload::System(sys) = frame.payload() else {
        panic!("not a system frame");
    };
    let (luma, ls) = sys.plane(0).unwrap();
    let (chroma, cs) = sys.plane(1).unwrap();
    assert_eq!(ls, down.stride());
    let want_y = (16.0 + 219.0 * (1.099 * 0.5f64.powf(0.45) - 0.099)).round() as u8;
    for y in 0..H as usize {
        for x in 0..W as usize {
            assert!(luma[y * ls + x].abs_diff(want_y) <= 1, "luma at {x},{y}");
        }
    }
    for y in 0..H as usize / 2 {
        for x in 0..W as usize {
            assert!(chroma[y * cs + x].abs_diff(128) <= 1, "chroma at {x},{y}");
        }
    }
    assert!(ctx.metrics().frames_cpu_copied() > copied);
    assert_eq!(down.stats().delivered, 1);
    assert_eq!(down.in_flight(), 0);
}

#[test]
fn download_refuses_when_the_ring_is_full_and_keeps_only_the_newest() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = solid(ctx, &mut pool, W, H, [0.2, 0.4, 0.6, 1.0]);
    let mut down = YuvDownload::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    for t in 0..DOWNLOAD_RING as u64 {
        assert!(
            down.submit(ctx, &pool, &canvas_frame(&pool, canvas, t))
                .unwrap()
        );
    }
    // Nothing taken yet: every staging buffer is in flight.
    assert!(
        !down
            .submit(ctx, &pool, &canvas_frame(&pool, canvas, 99))
            .unwrap()
    );
    assert_eq!(down.stats().busy, 1);
    ctx.wait_idle().unwrap();
    ctx.poll();
    let frame = down.take_ready(ctx).unwrap().unwrap();
    assert_eq!(frame.sequence(), DOWNLOAD_RING as u64 - 1, "the newest one");
    assert_eq!(down.stats().stale, DOWNLOAD_RING as u64 - 1);
    assert_eq!(down.in_flight(), 0);
}

#[test]
fn take_oldest_passes_every_completed_frame_in_order() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = solid(ctx, &mut pool, W, H, [0.2, 0.4, 0.6, 1.0]);
    let mut down = YuvDownload::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    for t in 0..DOWNLOAD_RING as u64 {
        assert!(
            down.submit(ctx, &pool, &canvas_frame(&pool, canvas, t))
                .unwrap()
        );
    }
    ctx.wait_idle().unwrap();
    ctx.poll();
    // All completed in the same poll; none is dropped as stale.
    for t in 0..DOWNLOAD_RING as u64 {
        assert_eq!(down.in_flight(), DOWNLOAD_RING - t as usize);
        let frame = down.take_oldest(ctx).unwrap().expect("a completed frame");
        assert_eq!(frame.sequence(), t);
    }
    assert!(down.take_oldest(ctx).unwrap().is_none());
    assert_eq!(down.in_flight(), 0);
    let stats = down.stats();
    assert_eq!((stats.stale, stats.delivered), (0, DOWNLOAD_RING as u64));
}

#[test]
fn download_drops_frames_while_consumers_hold_every_buffer_and_reuses_them_after() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = solid(ctx, &mut pool, W, H, [0.2, 0.4, 0.6, 1.0]);
    let mut down = YuvDownload::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let mut held = Vec::new();
    let mut ptrs = Vec::new();
    for t in 0..DOWNLOAD_BUFFERS as u64 + 1 {
        down.submit(ctx, &pool, &canvas_frame(&pool, canvas, t))
            .unwrap();
        ctx.wait_idle().unwrap();
        ctx.poll();
        match down.take_ready(ctx).unwrap() {
            Some(f) => {
                let FramePayload::System(sys) = f.payload() else {
                    panic!()
                };
                ptrs.push(sys.data().as_ptr());
                held.push(f);
            }
            None => assert_eq!(t, DOWNLOAD_BUFFERS as u64, "dropped too early"),
        }
    }
    assert_eq!(down.stats().no_buffer, 1);
    held.clear();
    down.submit(ctx, &pool, &canvas_frame(&pool, canvas, 100))
        .unwrap();
    ctx.wait_idle().unwrap();
    ctx.poll();
    let again = down.take_ready(ctx).unwrap().unwrap();
    let FramePayload::System(sys) = again.payload() else {
        panic!()
    };
    assert!(ptrs.contains(&sys.data().as_ptr()), "a recycled buffer");
}

fn system(format: VideoFormat, data: Vec<u8>, planes: &[PlaneLayout]) -> VideoFrame {
    let frame = SystemFrame::new(format, Bytes::from(data), planes).unwrap();
    VideoFrame::system(Timestamp::from_nanos(5), frame, None).with_sequence(3)
}

#[test]
fn upload_writes_bgra_with_padded_rows() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let stride = W as usize * 4 + 12;
    let mut data = vec![0u8; stride * H as usize];
    for row in data.chunks_mut(stride) {
        for px in row[..W as usize * 4].chunks_mut(4) {
            px.copy_from_slice(&[10, 20, 30, 255]); // B, G, R, A
        }
    }
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB);
    let frame = system(format, data, &[PlaneLayout { offset: 0, stride }]);
    let mut up = SourceUpload::new();
    let out = up.upload(ctx, &mut pool, frame).unwrap();
    assert_eq!(out.timestamp(), Timestamp::from_nanos(5));
    assert_eq!(out.sequence(), 3);
    let FramePayload::Texture(id) = *out.payload() else {
        panic!("not a texture");
    };
    let bytes = readback::read_bytes(ctx, &pool, id).unwrap();
    assert_eq!(&bytes[..4], &[10, 20, 30, 255]);
    assert_eq!(up.uploaded(), 1);
}

#[test]
fn upload_decodes_nv12_to_linear_rgb() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    // Limited-range grey: Y = 126 (about 0.5 after the inverse OETF... checked loosely),
    // Cb = Cr = 128.
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![126u8; w * h];
    data.extend(std::iter::repeat_n(128u8, w * h / 2));
    let format = VideoFormat::new(
        Size::new(W, H),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let frame = system(
        format,
        data,
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
    );
    let mut up = SourceUpload::new();
    let out = up.upload(ctx, &mut pool, frame).unwrap();
    assert_eq!(out.format().pixel_format, PixelFormat::Rgba16Float);
    let FramePayload::Texture(id) = *out.payload() else {
        panic!("not a texture");
    };
    let px = readback::read_rgba16f(ctx, &pool, id).unwrap();
    let g = px[0][0];
    assert_close(px[0], [g, g, g, 1.0], 0.01);
    assert!(g > 0.1 && g < 0.4, "grey {g}");
    // The second frame reuses the plane textures.
    let allocated = pool.allocated();
    let mut data = vec![126u8; w * h];
    data.extend(std::iter::repeat_n(128u8, w * h / 2));
    let frame = system(
        format,
        data,
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
    );
    drop(out);
    up.upload(ctx, &mut pool, frame).unwrap();
    assert_eq!(pool.allocated(), allocated);
    up.release(&mut pool);
}

/// ADR-0066 decision 4: a wide canvas fitted into a 4:3 output keeps its aspect ratio,
/// with black bars above and below and the picture's colour in the middle.
#[test]
fn a_scaled_download_fits_the_canvas_with_black_bars() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    // 32 x 16 (2:1) into 16 x 16: the picture is 16 x 8, centred, rows 4..12.
    let canvas = solid(ctx, &mut pool, W, H, [0.5, 0.5, 0.5, 1.0]);
    let (ow, oh) = (16u32, 16u32);
    let mut down =
        YuvDownload::scaled(ctx, YuvFormat::Nv12, W, H, ow, oh, DOWNLOAD_BUFFERS + 4).unwrap();
    assert_eq!(down.format().size, Size::new(ow, oh));
    assert!(
        down.submit(ctx, &pool, &canvas_frame(&pool, canvas, 1))
            .unwrap()
    );
    ctx.wait_idle().unwrap();
    ctx.poll();
    let frame = down.take_ready(ctx).unwrap().expect("a completed frame");
    let FramePayload::System(sys) = frame.payload() else {
        panic!("not a system frame");
    };
    let (luma, ls) = sys.plane(0).unwrap();
    let (chroma, cs) = sys.plane(1).unwrap();
    let grey = (16.0 + 219.0 * (1.099 * 0.5f64.powf(0.45) - 0.099)).round() as u8;
    for y in 0..oh as usize {
        let inside = (4..12).contains(&y);
        for x in 0..ow as usize {
            let want = if inside { grey } else { 16 };
            // The picture's edge rows mix with black through the filter.
            if y == 4 || y == 11 {
                continue;
            }
            assert!(
                luma[y * ls + x].abs_diff(want) <= 1,
                "luma at {x},{y}: {}",
                luma[y * ls + x]
            );
        }
    }
    for y in 0..oh as usize / 2 {
        for x in 0..ow as usize {
            assert!(chroma[y * cs + x].abs_diff(128) <= 1, "chroma at {x},{y}");
        }
    }
}

#[test]
fn the_same_size_through_scaled_is_the_one_to_one_path() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = solid(ctx, &mut pool, W, H, [0.2, 0.4, 0.6, 1.0]);
    let read = |down: &mut YuvDownload| {
        assert!(
            down.submit(ctx, &pool, &canvas_frame(&pool, canvas, 1))
                .unwrap()
        );
        ctx.wait_idle().unwrap();
        ctx.poll();
        let f = down.take_ready(ctx).unwrap().unwrap();
        let FramePayload::System(sys) = f.payload() else {
            panic!()
        };
        sys.data().to_vec()
    };
    let mut a = YuvDownload::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let mut b = YuvDownload::scaled(ctx, YuvFormat::Nv12, W, H, W, H, 3).unwrap();
    assert_eq!(read(&mut a), read(&mut b));
    assert!(YuvDownload::scaled(ctx, YuvFormat::Nv12, W, H, W, H, 1).is_err());
}
