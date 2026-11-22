//! The slide show change pass (ADR-0171 decision 7; spec `docs/specs/gpu-compositor.md`,
//! "Slide show change"): fade, slide and swipe between two reserved pictures, and the
//! lease rule of the reserved targets.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::upload::ReservedUpload;
use rbs_gpu::{ReservedTargets, SlideKind, SlidePass, TexturePool, readback};

const W: u32 = 8;
const H: u32 = 2;

fn picture(px: [u8; 4]) -> SystemFrame {
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Rgba8, ColorSpace::SRGB);
    let bytes: Vec<u8> = std::iter::repeat_n(px, (W * H) as usize)
        .flatten()
        .collect();
    SystemFrame::new(
        format,
        bytes.into(),
        &[PlaneLayout {
            offset: 0,
            stride: W as usize * 4,
        }],
    )
    .unwrap()
}

fn id(frame: &VideoFrame) -> rbs_gpu::TextureId {
    match *frame.payload() {
        FramePayload::Texture(id) => id,
        _ => panic!("not a texture frame"),
    }
}

/// Uploads red then blue and returns their texture ids (the frames are dropped, so
/// neither is leased).
fn two_pictures(
    ctx: &rbs_gpu::GpuContext,
    pool: &mut TexturePool,
) -> (ReservedUpload, rbs_gpu::TextureId, rbs_gpu::TextureId) {
    let mut up = ReservedUpload::reserve(ctx, pool, W, H, PixelFormat::Rgba8).unwrap();
    let red = up
        .upload_into(ctx, pool, &picture([255, 0, 0, 255]), Timestamp::ZERO)
        .unwrap()
        .unwrap();
    // Hold red's frame so the second upload goes into the other texture.
    let blue = up
        .upload_into(ctx, pool, &picture([0, 0, 255, 255]), Timestamp::ZERO)
        .unwrap()
        .unwrap();
    let (r, b) = (id(&red), id(&blue));
    assert_ne!(r, b);
    drop((red, blue));
    (up, r, b)
}

fn pixel(bytes: &[u8], x: u32) -> [u8; 4] {
    let i = (x as usize) * 4;
    [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
}

fn near(got: [u8; 4], want: [u8; 4]) -> bool {
    got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 2)
}

#[test]
fn fade_mixes_in_linear_light() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (_up, red, blue) = two_pictures(ctx, &mut pool);
    let targets = ReservedTargets::reserve(ctx, &mut pool, W, H).unwrap();
    let pass = SlidePass::new(ctx);
    for (t, want) in [
        (0.0, [255, 0, 0, 255]),
        (1.0, [0, 0, 255, 255]),
        // Half of linear 1.0 is sRGB 188, not 128.
        (0.5, [188, 0, 188, 255]),
    ] {
        let frame = pass
            .draw(
                ctx,
                &pool,
                &targets,
                red,
                blue,
                SlideKind::Fade,
                t,
                Timestamp::ZERO,
            )
            .unwrap()
            .unwrap();
        let bytes = readback::read_bytes(ctx, &pool, id(&frame)).unwrap();
        assert!(
            near(pixel(&bytes, 3), want),
            "t {t}: {:?}",
            pixel(&bytes, 3)
        );
        assert_eq!(frame.format().pixel_format, PixelFormat::Rgba8);
    }
}

#[test]
fn slide_and_swipe_bring_the_picture_in_from_the_right() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (_up, red, blue) = two_pictures(ctx, &mut pool);
    let targets = ReservedTargets::reserve(ctx, &mut pool, W, H).unwrap();
    let pass = SlidePass::new(ctx);
    for kind in [SlideKind::Slide, SlideKind::Swipe] {
        let frame = pass
            .draw(ctx, &pool, &targets, red, blue, kind, 0.25, Timestamp::ZERO)
            .unwrap()
            .unwrap();
        let bytes = readback::read_bytes(ctx, &pool, id(&frame)).unwrap();
        // A quarter in: the right two of eight columns are blue, the rest red.
        for x in 0..W {
            let want = if x >= 6 {
                [0, 0, 255, 255]
            } else {
                [255, 0, 0, 255]
            };
            assert!(near(pixel(&bytes, x), want), "{kind:?} x {x}");
        }
    }
}

#[test]
fn a_held_target_is_never_drawn_into() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (_up, red, blue) = two_pictures(ctx, &mut pool);
    let targets = ReservedTargets::reserve(ctx, &mut pool, W, H).unwrap();
    let pass = SlidePass::new(ctx);
    let draw = |pool: &TexturePool| {
        pass.draw(
            ctx,
            pool,
            &targets,
            red,
            blue,
            SlideKind::Fade,
            0.5,
            Timestamp::ZERO,
        )
        .unwrap()
    };
    let first = draw(&pool).unwrap();
    let second = draw(&pool).unwrap();
    assert_ne!(id(&first), id(&second));
    // Both held: the draw is skipped.
    assert!(draw(&pool).is_none());
    drop(first);
    assert!(draw(&pool).is_some());
    drop(second);
    targets.give_back(&mut pool);
}

#[test]
fn a_picture_of_another_size_is_refused() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (_up, red, blue) = two_pictures(ctx, &mut pool);
    let targets = ReservedTargets::reserve(ctx, &mut pool, W * 2, H).unwrap();
    let pass = SlidePass::new(ctx);
    assert!(
        pass.draw(
            ctx,
            &pool,
            &targets,
            red,
            blue,
            SlideKind::Fade,
            0.5,
            Timestamp::ZERO
        )
        .is_err()
    );
    assert!(ReservedTargets::reserve(ctx, &mut pool, 0, H).is_err());
}
