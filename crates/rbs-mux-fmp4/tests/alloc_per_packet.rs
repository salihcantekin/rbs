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

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamDescriptor,
    StreamIndex, StreamParams,
};
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
    // One test, run case by case: the counter is global, and tests in one binary run
    // in parallel.
    fragmented_mp4_with_h264_and_aac();
    mov_with_h264_and_pcm();
}

fn fragmented_mp4_with_h264_and_aac() {
    let config = MuxerConfig::new(
        ContainerId::Mp4Fragmented,
        vec![h264_stream(), aac_stream()],
    );
    let mut input = Vec::new();
    for i in 0..2000u64 {
        let t = Timestamp::from_nanos(1_000_000_000 + i * 33_333_333);
        input.push((StreamIndex(0), h264(i, t)));
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
    measure(&config, &input);
}

/// MOV with 24-bit stereo PCM in 10 ms packets: a PCM run has no per-sample rows, so
/// the fragment stays one allocation however many frames it holds.
fn mov_with_h264_and_pcm() {
    let pcm = StreamDescriptor::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        Bytes::from_static(&[24]),
        StreamParams::audio(AudioFormat::STEREO_48K, 480),
    );
    let config = MuxerConfig::new(ContainerId::Mov, vec![h264_stream(), pcm]);
    let mut input = Vec::new();
    let mut next_audio = 0u64;
    for i in 0..2000u64 {
        let ms = i * 1000 / 30;
        while next_audio <= ms {
            let t = Timestamp::from_nanos(1_000_000_000 + next_audio * 1_000_000);
            let a = EncodedPacket::new(
                CodecId::Pcm,
                BitstreamFormat::Raw,
                t,
                t,
                true,
                Bytes::from(vec![0x21; 480 * 6]),
            );
            input.push((StreamIndex(1), a.unwrap()));
            next_audio += 10;
        }
        let t = Timestamp::from_nanos(1_000_000_000 + ms * 1_000_000);
        input.push((StreamIndex(0), h264(i, t)));
    }
    measure(&config, &input);
}

fn h264(i: u64, t: Timestamp) -> EncodedPacket {
    let lp = BitstreamFormat::LengthPrefixed { length_size: 4 };
    let mut data = 5996u32.to_be_bytes().to_vec();
    data.push(if i.is_multiple_of(30) { 0x65 } else { 0x41 });
    data.resize(6000, 0xab);
    EncodedPacket::new(
        CodecId::H264,
        lp,
        t,
        t,
        i.is_multiple_of(30),
        Bytes::from(data),
    )
    .unwrap()
}

fn measure(config: &MuxerConfig, input: &[(StreamIndex, EncodedPacket)]) {
    let mut muxer = Fmp4MuxerFactory.create(config).unwrap();
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
