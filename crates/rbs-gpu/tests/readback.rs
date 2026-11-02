//! Readback is counted in `frames_cpu_copied` (spec `gpu-compositor.md`, "Readback").
//!
//! Its own test binary: the counter is per context, and the binary's shared context must
//! not be used by other tests at the same time. A second Vulkan device in one process
//! crashes lavapipe when created while others render.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use rbs_gpu::{CanvasDesc, Compositor, TextureDesc, TexturePool, readback};

#[test]
fn readback_is_counted() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: 16,
            height: 16,
        },
        1,
    )
    .unwrap();
    let target = pool.acquire(ctx, &TextureDesc::canvas(16, 16)).unwrap();
    let before = ctx.metrics().frames_cpu_copied();
    comp.render(ctx, &pool, &[], target).unwrap();
    assert_eq!(
        ctx.metrics().frames_cpu_copied(),
        before,
        "render alone never copies to the CPU"
    );
    readback::read_rgba16f(ctx, &pool, target).unwrap();
    assert_eq!(ctx.metrics().frames_cpu_copied(), before + 1);
}
