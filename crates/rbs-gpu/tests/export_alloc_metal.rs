//! Heap allocations per exported frame (spec `gpu-compositor.md`, "Native export").
//!
//! RBS code in `SurfaceExporter::export` and `take_ready` does not allocate. The test
//! compares a steady export loop (convert into an IOSurface, wait, take the frame, let the
//! "encoder" drop it) with the same conversion into plain pool planes: the export may add
//! only a small constant (wgpu's own poll bookkeeping), and nothing may grow over time.
#![cfg(target_vendor = "apple")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use common::solid;
use rbs_core::time::Timestamp;
use rbs_gpu::export::SurfaceExporter;
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

fn window(n: u64, mut frame: impl FnMut(u64)) -> f64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    for i in 0..n {
        frame(i);
    }
    (ALLOCS.load(Ordering::Relaxed) - before) as f64 / n as f64
}

#[test]
fn export_allocations_are_bounded() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let src = solid(ctx, &mut pool, 16, 16, [0.2, 0.4, 0.6, 1.0]);
    let layers = [Layer::new(src, 0.0, 0.0, W as f32, H as f32)];
    comp.render(ctx, &pool, &layers, canvas).unwrap();

    // Reference: conversion into pool planes, waiting for the GPU each frame.
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let mut plain = |_| {
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
        ctx.wait_idle().unwrap();
    };
    window(50, &mut plain);
    let plain_a = window(FRAMES, &mut plain);

    let mut exporter = SurfaceExporter::new(ctx, &mut pool, YuvFormat::Nv12, W, H, 3).unwrap();
    let mut export = |i| {
        exporter
            .export(ctx, &pool, canvas, Timestamp::from_nanos(i), i)
            .unwrap();
        ctx.wait_idle().unwrap();
        let frame = exporter.take_ready(ctx, &pool).unwrap().expect("ready");
        drop(frame);
    };
    window(50, &mut export);
    let export_a = window(FRAMES, &mut export);
    let export_b = window(FRAMES, &mut export);
    eprintln!(
        "allocations per frame: convert + wait {plain_a:.1}; export + wait + take {export_a:.1} / {export_b:.1}"
    );
    assert!(
        export_b <= export_a * 1.1 + 1.0,
        "growth: {export_a} then {export_b}"
    );
    assert!(
        export_a <= plain_a + 4.0,
        "export adds {} allocations per frame",
        export_a - plain_a
    );
    exporter.close(&mut pool);
}
