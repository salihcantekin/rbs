//! Heap allocations per packet on the mux path (spec `docs/specs/output-ports.md`,
//! allocation rule: at most one payload allocation per produced chunk).
//!
//! After warm-up, `write` must allocate exactly once per packet: the chunk's buffer.
//! The PES scratch buffer is reused and the chunk is sized exactly, so `Bytes::from`
//! neither reallocates nor adds a shared header.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::{BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{MuxerConfig, MuxerFactory};
use rbs_mux_mpegts::MpegTsMuxerFactory;

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

fn packets(count: u64) -> Vec<(StreamIndex, EncodedPacket)> {
    let mut out = Vec::new();
    for i in 0..count {
        let t = Timestamp::from_nanos(1_000_000_000 + i * 33_333_333);
        let nal_type = if i % 30 == 0 { 0x65 } else { 0x41 };
        let size = if i % 30 == 0 { 40_000 } else { 6_000 };
        let mut data = (size as u32 - 4).to_be_bytes().to_vec();
        data.push(nal_type);
        data.resize(size, 0xab);
        let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
        let video = EncodedPacket::new(CodecId::H264, lp, t, t, i % 30 == 0, Bytes::from(data));
        out.push((StreamIndex(0), video.unwrap()));
        let audio = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(vec![0x21; 380]),
        );
        out.push((StreamIndex(1), audio.unwrap()));
    }
    out
}

#[test]
fn one_allocation_per_packet_after_warm_up() {
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![h264_stream(), aac_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let input = packets(300);
    let mut chunks = Vec::with_capacity(input.len() + 1);
    muxer.begin(&mut chunks).unwrap();
    // Warm-up: the first keyframe grows the PES scratch buffer to its working size.
    let (warm, measured) = input.split_at(60);
    for (stream, packet) in warm {
        muxer.write(*stream, packet, &mut chunks).unwrap();
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    for (stream, packet) in measured {
        muxer.write(*stream, packet, &mut chunks).unwrap();
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocs,
        measured.len() as u64,
        "allocations for {} packets",
        measured.len()
    );
}
