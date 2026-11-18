//! Heap allocations per frame on the render path (spec docs/specs/gpu-compositor.md,
//! "Performance requirements").
//!
//! RBS code in `Compositor::render` and `YuvConverter::convert` does not allocate; wgpu
//! allocates internally while recording and submitting a command buffer. This test
//! measures the total per frame and asserts that it (a) does not grow over time and
//! (b) grows by at most a small constant per extra layer, so it stays bounded.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use common::solid;
use rbs_gpu::{
    CanvasDesc, Compositor, Layer, TextureDesc, TexturePool, YuvConverter, YuvFormat, yuv,
};

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
/// With `planar`, the layers are a 4:2:0 plane pair drawn by the YUV pipelines (R-11).
fn allocs_per_frame(layer_count: usize, planar: bool) -> Option<(f64, f64)> {
    allocs_per_frame_of(layer_count, planar, false)
}

/// [`allocs_per_frame`]; with `scrolling`, every layer scrolls (ADR-0150).
fn allocs_per_frame_of(layer_count: usize, planar: bool, scrolling: bool) -> Option<(f64, f64)> {
    let ctx = common::gpu()?;
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        64,
    )
    .unwrap();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let src = if planar {
        pool.acquire(ctx, &TextureDesc::planar(16, 16, wgpu::TextureFormat::NV12))
            .unwrap()
    } else {
        solid(ctx, &mut pool, 16, 16, [0.2, 0.4, 0.6, 1.0])
    };
    let mut layers: Vec<Layer> = (0..layer_count)
        .map(|i| Layer::new(src, i as f32, i as f32, 64.0, 36.0))
        .collect();
    let binding = scrolling.then(|| rbs_gpu::ScrollBinding::new(ctx));
    if scrolling {
        let spec = rbs_gpu::LayerScroll {
            direction: rbs_gpu::ScrollDirection::Left,
            repeat: rbs_gpu::ScrollRepeat::Loop,
            start: rbs_gpu::ScrollStart::Inside,
            gap: 8.0,
            fade: 4.0,
            run: rbs_gpu::ScrollRun::start(rbs_core::time::Timestamp::ZERO, 50.0),
        };
        for l in &mut layers {
            let f = spec
                .frame(
                    &l.transform,
                    &rbs_gpu::UvRect::FULL,
                    16,
                    16,
                    rbs_core::time::Timestamp::from_nanos(1_000_000_000),
                )
                .unwrap();
            l.scroll = Some(f.gpu(spec.direction, spec.repeat, spec.fade));
        }
    }

    let mut frame = || {
        comp.render_scrolled(ctx, &pool, &layers, canvas, binding.as_ref())
            .unwrap();
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
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
    let Some((one_a, one_b)) = allocs_per_frame(1, false) else {
        return;
    };
    let Some((many_a, many_b)) = allocs_per_frame(32, false) else {
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

#[test]
fn yuv_layers_allocate_like_rgb_layers() {
    let Some((rgb_a, rgb_b)) = allocs_per_frame(8, false) else {
        return;
    };
    let Some((yuv_a, yuv_b)) = allocs_per_frame(8, true) else {
        return;
    };
    eprintln!(
        "allocations per frame, 8 layers: RGB {rgb_a:.1} / {rgb_b:.1}, 4:2:0 {yuv_a:.1} / {yuv_b:.1}"
    );
    assert!(yuv_b <= yuv_a * 1.1 + 1.0, "4:2:0: {yuv_a} then {yuv_b}");
    // The colour records cost one more buffer write per frame (wgpu stages it: 8
    // allocations on lavapipe), far below the decode pass and submit per camera picture
    // they replace (about 56). Nothing per layer in RBS code.
    assert!(yuv_b <= rgb_b + 12.0, "4:2:0 {yuv_b} against RGB {rgb_b}");
    let Some((_, many)) = allocs_per_frame(32, true) else {
        return;
    };
    let per_layer = (many - yuv_b) / 24.0;
    eprintln!("extra allocations per 4:2:0 layer: {per_layer:.2}");
    assert!(per_layer <= 4.0, "per-layer allocations {per_layer}");
}

#[test]
fn scrolling_layers_allocate_a_bounded_constant_more() {
    let Some((rgb_a, rgb_b)) = allocs_per_frame_of(8, false, false) else {
        return;
    };
    let Some((scroll_a, scroll_b)) = allocs_per_frame_of(8, false, true) else {
        return;
    };
    eprintln!(
        "allocations per frame, 8 layers: plain {rgb_a:.1} / {rgb_b:.1}, scrolling {scroll_a:.1} / {scroll_b:.1}"
    );
    assert!(
        scroll_b <= scroll_a * 1.1 + 1.0,
        "scrolling: {scroll_a} then {scroll_b}"
    );
    // One queue write and one bind group set per scrolling layer: a small constant.
    let per_layer = (scroll_b - rgb_b) / 8.0;
    assert!(per_layer <= 4.0, "per scrolling layer {per_layer}");
}
