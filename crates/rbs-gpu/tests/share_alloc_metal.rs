//! Heap allocations per preview frame (spec `gpu-compositor.md`, "Preview export
//! (cross-process)"). Runs on macOS and Windows (the file name predates the Windows
//! backend, ADR-0035).
//!
//! RBS code in `PreviewPublisher::publish` and `tick` does not allocate. The test runs a
//! steady loop (publish, wait, tick sends the frame, the subscriber receives and drops it,
//! the next tick takes the release) and compares it with an NV12 conversion into pool
//! planes, which records more GPU work: the preview may add only a small constant (wgpu's
//! poll bookkeeping), and nothing may grow over time.
#![cfg(any(target_vendor = "apple", windows))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use std::thread;
use std::time::Duration;

use common::solid;
use rbs_core::time::Timestamp;
use rbs_gpu::share::{PreviewDesc, PreviewPublisher, PreviewSubscriber};
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
fn preview_allocations_are_bounded() {
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

    let mut publisher = match PreviewPublisher::new(
        ctx,
        PreviewDesc {
            width: W / 2,
            height: H / 2,
            surfaces: 3,
        },
    ) {
        Ok(p) => p,
        Err(rbs_gpu::GpuError::Unsupported(why)) => {
            eprintln!("SKIPPED: {why}");
            return;
        }
        Err(e) => panic!("publisher failed: {e}"),
    };
    let endpoint = publisher.endpoint().clone();
    let handle =
        thread::spawn(move || PreviewSubscriber::connect(&endpoint, Duration::from_secs(5)));
    while !handle.is_finished() {
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    let mut sub = handle.join().unwrap().unwrap();

    let mut preview = |i| {
        publisher
            .publish(ctx, &pool, canvas, Timestamp::from_nanos(i), i)
            .unwrap();
        ctx.wait_idle().unwrap();
        // Sends the frame and takes the previous frame's release.
        publisher.tick(ctx).unwrap();
        let frame = sub.recv(Duration::ZERO).unwrap().expect("frame");
        drop(frame);
    };
    window(50, &mut preview);
    let preview_a = window(FRAMES, &mut preview);
    let preview_b = window(FRAMES, &mut preview);
    eprintln!(
        "allocations per frame: NV12 convert + wait {plain_a:.1}; preview publish + wait + tick + receive {preview_a:.1} / {preview_b:.1}"
    );
    assert_eq!(publisher.stats().dropped, 0);
    assert!(
        preview_b <= preview_a * 1.1 + 1.0,
        "growth: {preview_a} then {preview_b}"
    );
    assert!(
        preview_a <= plain_a + 4.0,
        "preview adds {} allocations per frame",
        preview_a - plain_a
    );
}
