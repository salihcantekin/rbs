//! Heap use on the hot calls. `submit`/`drain` may allocate the packet payload (one
//! exact-size `Bytes` per packet) and nothing else; `decode` and `conceal` allocate
//! nothing once the output vector has its capacity. libopus keeps its scratch on the
//! stack (`USE_ALLOCA`); its only `malloc` calls are in state creation (`opus_alloc` in
//! `opus_encoder.c`, `opus_decoder.c`, `celt/modes.c` for custom modes, which are not
//! built), as `grep opus_alloc vendor/libopus` shows. Only the test thread's Rust
//! allocations are counted, which is where the packet payload comes from.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rbs_codec::{AudioDecoderConfig, AudioDecoderFactory, AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_opus::{OpusDecoderFactory, OpusEncoderFactory};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCS.with(Cell::get)
}

// SAFETY: forwards every call unchanged to the system allocator; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: same contract as the caller's, forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: `ptr` came from `System.alloc` with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn hot_calls_allocate_only_the_packet_payload() {
    let left: Vec<f32> = (0..960).map(|n| (n as f32 * 0.05).sin() * 0.4).collect();
    let config = AudioEncoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K, 128_000);
    let mut encoder = OpusEncoderFactory.create(&config).unwrap();
    let mut decoder = OpusDecoderFactory
        .create(&AudioDecoderConfig::new(
            CodecId::Opus,
            AudioFormat::STEREO_48K,
        ))
        .unwrap();
    let mut packets = Vec::with_capacity(64);
    let mut out = Vec::with_capacity(decoder.max_frames() * 2);
    let block = |n: u64| {
        AudioBlock::planar(
            AudioFormat::STEREO_48K,
            Timestamp::from_nanos(n * 20_000_000),
            &[&left, &left],
        )
        .unwrap()
    };
    // Warm up: first calls may touch lazily initialised tables.
    for n in 0..4 {
        encoder.submit(&block(n)).unwrap();
        encoder.drain(&mut packets).unwrap();
    }
    for p in &packets {
        decoder.decode(p, &mut out).unwrap();
    }
    decoder.conceal(960, &mut out).unwrap();
    packets.clear();

    let rust_before = allocations();
    for n in 4..44 {
        encoder.submit(&block(n)).unwrap();
        encoder.drain(&mut packets).unwrap();
    }
    let encode_allocs = allocations() - rust_before;
    assert_eq!(packets.len(), 40);
    assert!(
        encode_allocs <= 40,
        "{encode_allocs} allocations for 40 packets"
    );

    let rust_before = allocations();
    for p in &packets {
        decoder.decode(p, &mut out).unwrap();
        decoder.conceal(960, &mut out).unwrap();
    }
    assert_eq!(
        allocations() - rust_before,
        0,
        "decode and conceal allocated"
    );
}
