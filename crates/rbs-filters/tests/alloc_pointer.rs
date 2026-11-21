//! Allocations per tick of a pointer filter type (spec `video-filters.md`, "Host
//! service: pointer" and conformance item 7), idle and drawing: bounded and not
//! growing. Own test binary with one test, because it installs a counting global
//! allocator whose counter a test running beside it would add to. Skips without a GPU
//! adapter unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{FilterType, Values};

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
/// Ticks per measured window, after `WARM_UP` ticks (see `alloc_per_frame.rs`).
const FRAMES: u64 = 60;
const WARM_UP: u64 = 20;

/// A pointer type (spec "Host service: pointer"): idle on an unchanged input it does
/// what the skip path does; drawing a live ring every tick it costs what a render
/// costs, and neither grows.
#[test]
fn a_pointer_type_allocates_like_any_filter_idle_and_drawing() {
    use common::pointer::{RingType, Scripted, mapping};
    use rbs_capture::{Cover, MappingCell};
    use rbs_filters::PointerFeed;
    use rbs_input::InputMonitor;

    let Some(ctx) = common::gpu() else { return };
    let script = Scripted::default();
    let monitor = InputMonitor::new(Box::new(script.clone()));
    let cell = MappingCell::new();
    cell.publish(Some(mapping(Cover::Clear)));
    // Rings that never die, so every drawing tick draws.
    let ty: Arc<dyn FilterType> = Arc::new(RingType::living(u64::MAX / 2));
    let values = Values::defaults(ty.as_ref());
    let feed = PointerFeed::new(monitor, cell);
    let mut rig = Rig::with_pointer(Arc::clone(&ctx), ty, values, Some([W, H]), feed).unwrap();
    rig.set_input(Picture::solid(W, H, [0.2, 0.6, 0.3, 1.0]));
    let window = |rig: &mut Rig| {
        for _ in 0..WARM_UP {
            rig.tick();
        }
        let mut avg = || {
            let before = ALLOCS.load(Ordering::Relaxed);
            for _ in 0..FRAMES {
                rig.tick();
            }
            (ALLOCS.load(Ordering::Relaxed) - before) as f64 / FRAMES as f64
        };
        (avg(), avg())
    };
    let (idle_a, idle_b) = window(&mut rig);
    script.click(1, 102.0, 53.0);
    let (draw_a, draw_b) = window(&mut rig);
    ctx.wait_idle().unwrap();
    eprintln!("pointer: idle {idle_a:.1} / {idle_b:.1}, drawing {draw_a:.1} / {draw_b:.1}");
    assert!(idle_b <= idle_a * 1.1 + 1.0, "{idle_a} then {idle_b}");
    assert!(draw_b <= draw_a * 1.1 + 1.0, "{draw_a} then {draw_b}");
    assert!(idle_b <= 8.0, "{idle_b} idle");
    assert!(draw_b - idle_b <= 128.0, "{draw_b} drawing");
}
