//! Allocations per frame of a filter at a steady size (spec `video-filters.md`,
//! "Performance requirements" and conformance item 7): bounded and not growing. The
//! node itself allocates nothing; what is counted is the wgpu command recording and
//! submit it shares with the compositor, plus the rig's executor tick. Own test binary because
//! it installs a counting global allocator. Skips without a GPU adapter unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{FieldKind, FilterType, Value, Values, catalogue};

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

/// Two value sets that differ in the first number field, so every tick renders.
fn two_values(ty: &dyn FilterType) -> (Values, Values) {
    let a = Values::defaults(ty);
    let field = ty
        .fields()
        .iter()
        .find_map(|f| match f.kind {
            FieldKind::Number { min, max, .. } => Some((f.key, (min + max) / 2.0)),
            _ => None,
        })
        .unwrap();
    let b = a.with(ty, [(field.0, Value::Number(field.1))]).unwrap();
    (a, b)
}

/// Average allocations per tick over two windows, rendering every tick when
/// `render`, else on an unchanged input (the skip path).
fn per_tick(rig: &mut Rig, values: (Values, Values), render: bool) -> (f64, f64) {
    let mut n = 0u64;
    let mut tick = |rig: &mut Rig| {
        if render {
            let v = if n.is_multiple_of(2) {
                values.0
            } else {
                values.1
            };
            rig.set_values(0, v).unwrap();
        }
        n += 1;
        rig.tick();
    };
    for _ in 0..50 {
        tick(rig);
    }
    let mut window = |rig: &mut Rig| {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..FRAMES {
            tick(rig);
        }
        (ALLOCS.load(Ordering::Relaxed) - before) as f64 / FRAMES as f64
    };
    let first = window(rig);
    let second = window(rig);
    (first, second)
}

#[test]
fn allocations_per_frame_are_bounded_and_do_not_grow() {
    let Some(ctx) = common::gpu() else { return };
    for ty in catalogue() {
        let values = two_values(*ty);
        let mut rig = Rig::new(Arc::clone(&ctx), Arc::new(*ty), values.0, Some([W, H])).unwrap();
        rig.set_input(Picture::solid(W, H, [0.2, 0.6, 0.3, 1.0]));
        let (skip_a, skip_b) = per_tick(&mut rig, values, false);
        let (draw_a, draw_b) = per_tick(&mut rig, values, true);
        ctx.wait_idle().unwrap();
        let render = draw_b - skip_b;
        eprintln!(
            "{}: unchanged input {skip_a:.1} / {skip_b:.1}, rendering {draw_a:.1} / {draw_b:.1}, render {render:.1}",
            ty.name()
        );
        // No growth over time.
        assert!(
            skip_b <= skip_a * 1.1 + 1.0,
            "{}: {skip_a} then {skip_b}",
            ty.name()
        );
        assert!(
            draw_b <= draw_a * 1.1 + 1.0,
            "{}: {draw_a} then {draw_b}",
            ty.name()
        );
        // The render costs a bounded number of allocations: wgpu's command encoder,
        // passes and submit, one submit per filter (rbs-gpu's compositor frame, two
        // submits, counts about 130 with the same wgpu).
        assert!(render <= 128.0, "{}: {render} for a render", ty.name());
        // An unchanged input renders nothing: a few allocations of the rig's tick.
        assert!(
            skip_b <= 8.0,
            "{}: {skip_b} on an unchanged input",
            ty.name()
        );
    }
}
