//! Heap allocations per frame of the YUV source decode, as the importer runs it: acquire a
//! decoded slot, decode, hand the slot to a frame and release it (spec
//! docs/specs/gpu-compositor.md, "YUV source decode"). RBS code does not allocate; wgpu
//! allocates while recording and submitting. The count must not grow over time and must
//! stay within a small constant of one NV12 conversion (which records two passes).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::ColorSpace;
use rbs_gpu::yuv_decode::decoded_desc;
use rbs_gpu::{TextureDesc, TexturePool, YuvConverter, YuvDecoder, YuvFormat, YuvSource, yuv};

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

fn per_frame(mut frame: impl FnMut()) -> (f64, f64) {
    for _ in 0..50 {
        frame();
    }
    let mut window = |n: u64| {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..n {
            frame();
        }
        (ALLOCS.load(Ordering::Relaxed) - before) as f64 / n as f64
    };
    let first = window(FRAMES);
    (first, window(FRAMES))
}

#[test]
fn decode_allocations_are_bounded() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let mut conv = YuvConverter::new(ctx, YuvFormat::Nv12, W, H).unwrap();
    let mut dec = YuvDecoder::new(ctx);

    let (conv_a, conv_b) = per_frame(|| {
        conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
        ctx.poll();
    });
    let (dec_a, dec_b) = per_frame(|| {
        let target = pool.acquire(ctx, &decoded_desc(W, H)).unwrap();
        dec.decode(
            ctx,
            &pool,
            YuvSource::Planes { luma, chroma },
            ColorSpace::BT709_LIMITED,
            target,
        )
        .unwrap();
        let frame = pool
            .frame(
                target,
                rbs_core::media::VideoFormat::new(
                    rbs_core::media::Size::new(W, H),
                    rbs_core::media::PixelFormat::Rgba16Float,
                    ColorSpace::LINEAR_BT709,
                ),
                rbs_core::time::Timestamp::ZERO,
            )
            .unwrap();
        pool.release(target).unwrap();
        drop(frame);
        ctx.poll();
    });
    ctx.wait_idle().unwrap();
    eprintln!(
        "allocations per frame: NV12 convert {conv_a:.1} / {conv_b:.1}, decode {dec_a:.1} / {dec_b:.1}"
    );
    assert!(dec_b <= dec_a * 1.1 + 1.0, "decode: {dec_a} then {dec_b}");
    assert!(
        dec_b <= conv_b + 4.0,
        "decode {dec_b} vs convert {conv_b} allocations per frame"
    );
    assert_eq!(dec.cached_sources(), 1);
}
