//! Heap allocations on the encode path: at most one per packet (the exact-size payload
//! `Vec`). Staging, 16-bit conversion and `drain` allocate nothing in RBS code once the
//! output vector has room. Media Foundation's own allocations bypass the Rust
//! allocator and are not counted.
//!
//! Only the test thread's allocations count: the adapter does all its work on the
//! calling thread, and the test harness's other threads (output capture, timers)
//! allocate whenever they like, which once added 4 to a CI run.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rbs_codec::{AudioEncoder, AudioEncoderConfig};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

struct Counting;

thread_local! {
    // Const-initialised `Cell`: reading it never allocates, so the allocator may use it.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: during thread teardown the slot may be gone; nothing to count then.
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

fn allocations() -> u64 {
    ALLOCS.with(Cell::get)
}

// SAFETY: forwards every call unchanged to the system allocator; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's, forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn submit_and_drain_allocate_only_payloads() {
    for (name, factory) in common::factories() {
        let mut s = factory
            .create(&AudioEncoderConfig::new(
                CodecId::Aac,
                AudioFormat::STEREO_48K,
                128_000,
            ))
            .unwrap();
        let left: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
        let rate = AudioFormat::STEREO_48K.sample_rate;
        let origin = Timestamp::from_nanos(1_000_000_000);
        let mut out = Vec::with_capacity(1024);
        let mut block_index = 0u64;
        let mut run = |blocks: u64, s: &mut dyn AudioEncoder, out: &mut Vec<_>| {
            let mut allocs = 0;
            for _ in 0..blocks {
                let ts = origin + rate.duration_of(block_index * 480);
                block_index += 1;
                let block =
                    AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
                let before = allocations();
                s.submit(&block).unwrap();
                s.drain(out).unwrap();
                allocs += allocations() - before;
            }
            allocs
        };
        run(50, s.as_mut(), &mut out); // warm-up
        let before = out.len();
        let allocs = run(500, s.as_mut(), &mut out); // 5 s
        let packets = (out.len() - before) as u64;
        eprintln!("{name}: {allocs} allocations for {packets} packets");
        assert!(packets > 200, "{name}");
        assert!(
            allocs <= packets,
            "{name}: {allocs} allocations for {packets} packets"
        );
    }
}
