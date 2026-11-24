//! Allocations per tick of the background removal node at a steady picture and a steady
//! stream of mattes: bounded and not growing (hot-path review, ADR-0179 decision 3). Own
//! test binary with one test, because it installs a counting global allocator. Skips
//! without a GPU adapter unless `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Rig, gpu};
use rbs_filters::conformance::Picture;

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

const FRAMES: u64 = 20;
const WARM_UP: u64 = 8;

/// Every tick has a new picture (a camera), so every tick draws small, reads back,
/// lends a frame, uploads a matte and composites. The count per tick must not grow.
#[test]
fn the_node_allocates_a_bounded_number_per_tick() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx.clone(), [64, 36]);
    let picture = Picture::solid(320, 180, [0.2, 0.6, 0.3, 1.0]);
    for _ in 0..WARM_UP {
        rig.set_input(picture.clone());
        rig.tick();
        ctx.wait_idle().unwrap();
    }
    let window = |rig: &mut Rig| {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..FRAMES {
            rig.set_input(picture.clone());
            rig.tick();
            ctx.wait_idle().unwrap();
        }
        (ALLOCS.load(Ordering::Relaxed) - before) as f64 / FRAMES as f64
    };
    let first = window(&mut rig);
    let second = window(&mut rig);
    eprintln!("background removal: {first:.1} then {second:.1} allocations per tick");
    assert!(second <= first * 1.1 + 1.0, "{first} then {second}");
    assert!(rig.stats.submitted() > 0 && rig.stats.mattes() > 0);
}
