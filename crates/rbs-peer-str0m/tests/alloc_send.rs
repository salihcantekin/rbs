//! `WebRtcTransport::send` allocates nothing (hot-path checklist, ADR-0139 decision
//! 11): it clones the packet (its bytes are shared) into a pre-allocated ring. Only
//! this thread's allocations are counted.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_peer_str0m::WebRtcMedia;
use rbs_transport::{SendOutcome, Transport, TransportItem};

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
fn send_allocates_nothing() {
    let video = StreamParams::video(
        VideoFormat::new(
            Size::new(1280, 720),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_30,
    );
    let mut transport = WebRtcMedia::transport(&WebRtcMedia::detached_live()).unwrap();
    transport
        .start(&[
            StreamDescriptor::new(CodecId::H264, BitstreamFormat::AnnexB, Bytes::new(), video),
            StreamDescriptor::new(
                CodecId::Opus,
                BitstreamFormat::Raw,
                Bytes::new(),
                StreamParams::audio(AudioFormat::STEREO_48K, 960),
            ),
        ])
        .unwrap();
    let packet = EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        Timestamp::ZERO,
        Timestamp::ZERO,
        true,
        Bytes::from(vec![0, 0, 0, 1, 0x65, 9, 9, 9]),
    )
    .unwrap();
    let audio = EncodedPacket::new(
        CodecId::Opus,
        BitstreamFormat::Raw,
        Timestamp::ZERO,
        Timestamp::ZERO,
        false,
        Bytes::from(vec![0xfc; 60]),
    )
    .unwrap();
    // A `Bytes` built from a `Vec` allocates its shared header on its first clone (as
    // in the RTMP test); packets from an encoder queue have been cloned already.
    let _ = (packet.clone(), audio.clone());
    let before = allocations();
    for i in 0..200u16 {
        let (stream, packet) = if i % 2 == 0 {
            (0, &packet)
        } else {
            (1, &audio)
        };
        let outcome = transport
            .send(TransportItem::Packet {
                stream: StreamIndex(stream),
                packet,
            })
            .unwrap();
        assert_eq!(outcome, SendOutcome::Queued);
    }
    assert_eq!(allocations() - before, 0, "send allocated");
}
