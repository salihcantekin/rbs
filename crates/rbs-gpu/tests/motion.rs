//! The motion probe (ADR-0126 decision 3; spec docs/specs/gpu-compositor.md, "Motion
//! probe"): the mean absolute luma difference of a 64 x 36 grid against the previous
//! picture, read back 4 bytes at a time without blocking, in submission order.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use rbs_core::media::{ColorSpace, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::{GpuContext, MOTION_RING, MotionProbe, TextureId, TexturePool};

fn picture(pool: &TexturePool, id: TextureId, w: u32, h: u32) -> VideoFrame {
    let format = VideoFormat::new(
        Size::new(w, h),
        PixelFormat::Rgba16Float,
        ColorSpace::LINEAR_BT709,
    );
    pool.frame(id, format, Timestamp::ZERO).unwrap()
}

fn drain(ctx: &GpuContext, probe: &mut MotionProbe) -> Vec<(u64, f32)> {
    ctx.wait_idle().unwrap();
    ctx.poll();
    let mut out = Vec::new();
    probe.take(ctx, |t, m| out.push((t, m)));
    out
}

#[test]
fn motion_is_the_mean_luma_change_against_the_previous_picture() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (w, h) = (128, 72);
    let dark = solid(ctx, &mut pool, w, h, [0.25, 0.25, 0.25, 1.0]);
    let light = solid(ctx, &mut pool, w, h, [0.75, 0.75, 0.75, 1.0]);
    let mut probe = MotionProbe::new(ctx).unwrap();
    let copied = ctx.metrics().frames_cpu_copied();

    for (i, id) in [dark, dark, light, light, dark].into_iter().enumerate() {
        assert!(
            probe
                .submit(ctx, &pool, &picture(&pool, id, w, h), i as u64 * 10)
                .unwrap()
        );
        // Drain each time so the ring never fills in this test.
        let got = drain(ctx, &mut probe);
        // The first picture has nothing to compare with.
        assert_eq!(got.len(), usize::from(i > 0), "{got:?}");
        if let Some(&(t, m)) = got.first() {
            assert_eq!(t, i as u64 * 10);
            let want = if i == 1 || i == 3 { 0.0 } else { 0.5 };
            assert!((m - want).abs() < 0.002, "picture {i}: {m}, want {want}");
        }
    }
    assert_eq!(probe.stats().delivered, 4);
    assert_eq!(ctx.metrics().frames_cpu_copied() - copied, 4);
    assert_eq!(probe.in_flight(), 0);
}

#[test]
fn a_full_ring_drops_the_picture_and_values_come_in_order() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let (w, h) = (64, 36);
    let a = solid(ctx, &mut pool, w, h, [0.0, 0.0, 0.0, 1.0]);
    let b = solid(ctx, &mut pool, w, h, [1.0, 1.0, 1.0, 1.0]);
    let mut probe = MotionProbe::new(ctx).unwrap();
    for i in 0..MOTION_RING as u64 {
        let id = if i % 2 == 0 { a } else { b };
        assert!(
            probe
                .submit(ctx, &pool, &picture(&pool, id, w, h), i)
                .unwrap()
        );
    }
    assert!(
        !probe
            .submit(ctx, &pool, &picture(&pool, a, w, h), 99)
            .unwrap()
    );
    assert_eq!(probe.stats().busy, 1);
    let got = drain(ctx, &mut probe);
    let times: Vec<u64> = got.iter().map(|g| g.0).collect();
    assert_eq!(times, (1..MOTION_RING as u64).collect::<Vec<_>>());
    for (_, m) in got {
        assert!((m - 1.0).abs() < 0.002, "{m}");
    }
    assert_eq!(probe.in_flight(), 0);
}
