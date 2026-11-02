//! Heap allocations per frame on the render path (spec docs/specs/gpu-compositor.md,
//! "Performance requirements").
//!
//! RBS code in `Compositor::render` and `Nv12Converter::convert` does not allocate; wgpu
//! allocates internally while recording and submitting a command buffer. This test
//! measures the total per frame and asserts that it (a) does not grow over time and
//! (b) grows by at most a small constant per extra layer, so it stays bounded.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use common::solid;
use rbs_gpu::{CanvasDesc, Compositor, Layer, Nv12Converter, TextureDesc, TexturePool, nv12};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call unchanged to the system allocator; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's, forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const W: u32 = 320;
const H: u32 = 180;
const FRAMES: u64 = 200;

/// Average allocations per frame over `FRAMES` frames after a warm-up.
fn allocs_per_frame(layer_count: usize) -> Option<(f64, f64)> {
    let ctx = common::gpu()?;
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        &ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        64,
    )
    .unwrap();
    let mut conv = Nv12Converter::new(&ctx, W, H).unwrap();
    let canvas = pool.acquire(&ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool.acquire(&ctx, &nv12::luma_desc(W, H)).unwrap();
    let chroma = pool.acquire(&ctx, &nv12::chroma_desc(W, H)).unwrap();
    let src = solid(&ctx, &mut pool, 16, 16, [0.2, 0.4, 0.6, 1.0]);
    let layers: Vec<Layer> = (0..layer_count)
        .map(|i| Layer::new(src, i as f32, i as f32, 64.0, 36.0))
        .collect();

    let mut frame = || {
        comp.render(&ctx, &pool, &layers, canvas).unwrap();
        conv.convert(&ctx, &pool, canvas, luma, chroma).unwrap();
        ctx.poll();
    };
    for _ in 0..50 {
        frame();
    }
    ctx.wait_idle().unwrap();

    let mut window = |n: u64| {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..n {
            frame();
        }
        (ALLOCS.load(Ordering::Relaxed) - before) as f64 / n as f64
    };
    let first = window(FRAMES);
    let second = window(FRAMES);
    ctx.wait_idle().unwrap();
    Some((first, second))
}

#[test]
fn allocations_per_frame_are_bounded() {
    let Some((one_a, one_b)) = allocs_per_frame(1) else {
        return;
    };
    let Some((many_a, many_b)) = allocs_per_frame(32) else {
        return;
    };
    eprintln!(
        "allocations per frame (render + NV12 + poll): 1 layer {one_a:.1} / {one_b:.1}, 32 layers {many_a:.1} / {many_b:.1}"
    );

    // (a) No growth over time: the second window is not larger than the first (+10 %).
    assert!(one_b <= one_a * 1.1 + 1.0, "1 layer: {one_a} then {one_b}");
    assert!(
        many_b <= many_a * 1.1 + 1.0,
        "32 layers: {many_a} then {many_b}"
    );
    // (b) Per-layer cost is a small constant (wgpu command recording), not unbounded.
    let per_layer = (many_b - one_b) / 31.0;
    eprintln!("extra allocations per layer: {per_layer:.2}");
    assert!(per_layer <= 4.0, "per-layer allocations {per_layer}");
}
