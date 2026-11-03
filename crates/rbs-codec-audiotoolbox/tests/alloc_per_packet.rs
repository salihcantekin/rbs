//! Heap allocations on the encode path: at most one per packet (the exact-size payload
//! `Vec`). Staging, the converter's input callback and `drain` allocate nothing in RBS
//! code once the output vector has room. AudioToolbox's own `malloc` calls bypass the
//! Rust allocator and are not counted.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_codec::{AudioEncoder, AudioEncoderConfig};
use rbs_codec_audiotoolbox::AacFactory;
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

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

#[test]
fn submit_and_drain_allocate_only_payloads() {
    let mut s = AacFactory::new()
        .create_session(&AudioEncoderConfig::new(
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
    let mut run = |blocks: u64, out: &mut Vec<_>| {
        let mut allocs = 0;
        for _ in 0..blocks {
            let ts = origin + rate.duration_of(block_index * 480);
            block_index += 1;
            let block = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
            let before = ALLOCS.load(Ordering::Relaxed);
            s.submit(&block).unwrap();
            s.drain(out).unwrap();
            allocs += ALLOCS.load(Ordering::Relaxed) - before;
        }
        allocs
    };
    run(50, &mut out); // warm-up
    let before = out.len();
    let allocs = run(500, &mut out); // 5 s
    let packets = (out.len() - before) as u64;
    eprintln!("{allocs} allocations for {packets} packets");
    assert!(packets > 200);
    assert!(
        allocs <= packets,
        "{allocs} allocations for {packets} packets"
    );
}
