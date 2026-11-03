//! Heap allocations on the fragmented MP4 mux path (spec `docs/specs/output-ports.md`,
//! allocation rule).
//!
//! After warm-up the muxer allocates exactly once per fragment: the `moof` + `mdat`
//! chunk, sized exactly so `Bytes::from` neither reallocates nor adds a shared header.
//! Collecting a sample only clones the packet's `Bytes` (a reference count; the input
//! here is cloned once beforehand so `Bytes` made from a `Vec` is already shared).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::{BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{MuxerConfig, MuxerFactory};
use rbs_mux_fmp4::Fmp4MuxerFactory;

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
fn one_allocation_per_fragment_after_warm_up() {
    let config = MuxerConfig::new(
        ContainerId::Mp4Fragmented,
        vec![h264_stream(), aac_stream()],
    );
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
    let mut input = Vec::new();
    for i in 0..2000u64 {
        let t = Timestamp::from_nanos(1_000_000_000 + i * 33_333_333);
        let mut data = 5996u32.to_be_bytes().to_vec();
        data.push(if i % 30 == 0 { 0x65 } else { 0x41 });
        data.resize(6000, 0xab);
        let v = EncodedPacket::new(CodecId::H264, lp, t, t, i % 30 == 0, Bytes::from(data));
        input.push((StreamIndex(0), v.unwrap()));
        let a = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(vec![0x21; 380]),
        );
        input.push((StreamIndex(1), a.unwrap()));
    }
    // Share every payload once, as an encoder's pooled or already-cloned output is.
    let shared: Vec<Bytes> = input.iter().map(|(_, p)| p.data().clone()).collect();
    let mut chunks = Vec::with_capacity(input.len() + 1);
    muxer.begin(&mut chunks).unwrap();
    // Warm up over several fragments so the per-track sample lists reach their size.
    let (warm, measured) = input.split_at(400);
    for (s, p) in warm {
        muxer.write(*s, p, &mut chunks).unwrap();
    }
    let fragments_before = chunks.len();
    let before = ALLOCS.load(Ordering::Relaxed);
    for (s, p) in measured {
        muxer.write(*s, p, &mut chunks).unwrap();
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    let fragments = (chunks.len() - fragments_before) as u64;
    assert!(fragments > 50);
    assert_eq!(allocs, fragments, "allocations for {fragments} fragments");
    drop(shared);
}
