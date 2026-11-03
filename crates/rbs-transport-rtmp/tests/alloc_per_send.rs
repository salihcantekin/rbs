//! Heap allocations on the RTMP transport's send path (output-ports spec, hot-path
//! rules): none in steady state. `send` validates the packet without allocating and
//! clones it (a reference count) into pre-allocated queues; the I/O thread builds
//! bodies and chunks in buffers it reuses. The counter is global, so the I/O thread
//! (and the test server, which only discards in this test) count too.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_transport::{
    QueueLimits, Secret, SendOutcome, Transport, TransportConfig, TransportEvent, TransportFactory,
    TransportInput, TransportItem,
};
use rbs_transport_rtmp::RtmpTransportFactory;
use support::{Behaviour, PPS, SPS, TestServer};

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

fn streams() -> Vec<StreamDescriptor> {
    let mut extradata = vec![0, 0, 0, 1];
    extradata.extend_from_slice(SPS);
    extradata.extend_from_slice(&[0, 0, 0, 1]);
    extradata.extend_from_slice(PPS);
    vec![
        StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::from(extradata),
            StreamParams::video(
                VideoFormat::new(
                    Size::new(320, 240),
                    PixelFormat::Nv12,
                    ColorSpace::BT709_LIMITED,
                ),
                FrameRate::FPS_30,
            ),
        ),
        StreamDescriptor::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Bytes::from_static(&[0x11, 0x90]),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        ),
    ]
}

/// Video and audio packets in decode order, 30 fps with one keyframe per second.
fn packets(count: u64) -> Vec<(StreamIndex, EncodedPacket)> {
    let mut out = Vec::new();
    for i in 0..count {
        let t = Timestamp::from_nanos(1_000_000_000 + i * 33_333_333);
        let key = i % 30 == 0;
        let len = if key { 30_000 } else { 3_000 };
        let mut data = vec![
            0,
            0,
            0,
            1,
            0x09,
            0xf0,
            0,
            0,
            0,
            1,
            if key { 0x65 } else { 0x41 },
        ];
        data.extend((0..len).map(|j| ((i as usize + j * 13) % 250) as u8 + 1));
        let data = Bytes::from(data);
        // Share once, so cloning in `send` is a reference-count increment.
        let _ = data.clone();
        let p = EncodedPacket::new(CodecId::H264, BitstreamFormat::AnnexB, t, t, key, data);
        out.push((StreamIndex(0), p.unwrap()));
        let a = Bytes::from(vec![(i % 251) as u8; 300]);
        let _ = a.clone();
        let p = EncodedPacket::new(CodecId::Aac, BitstreamFormat::Raw, t, t, true, a);
        out.push((StreamIndex(1), p.unwrap()));
    }
    out
}

fn send_all(t: &mut Box<dyn Transport>, packets: &[(StreamIndex, EncodedPacket)]) {
    for (stream, packet) in packets {
        let outcome = t.send(TransportItem::Packet {
            stream: *stream,
            packet,
        });
        assert_eq!(outcome, Ok(SendOutcome::Queued));
    }
}

fn drain(t: &mut Box<dyn Transport>, events: &mut Vec<TransportEvent>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.stats().queued_bytes > 0 {
        assert!(Instant::now() < deadline, "the I/O thread does not drain");
        t.poll_events(events);
        std::thread::yield_now();
    }
}

#[test]
fn send_and_delivery_do_not_allocate() {
    let server = TestServer::start(Behaviour {
        sink: true,
        ..Behaviour::default()
    });
    let config = TransportConfig::new(server.url(), TransportInput::Packets)
        .with_secret(Secret::new("alloc-test"))
        .with_queue(QueueLimits::new(usize::MAX, Duration::from_secs(3600)));
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams()).unwrap();
    let all = packets(1200);
    let (warm, measured) = all.split_at(600);
    let mut events = Vec::with_capacity(64);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events.contains(&TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    // Warm up both sides: buffers reach their working size, the I/O thread parks,
    // is woken and reads the socket.
    for part in warm.chunks(100) {
        send_all(&mut t, part);
        drain(&mut t, &mut events);
    }
    events.clear();
    let before = ALLOCS.load(Ordering::Relaxed);
    for part in measured.chunks(100) {
        send_all(&mut t, part);
        drain(&mut t, &mut events);
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(allocs, 0, "allocations for {} sends", measured.len());
    assert!(events.is_empty(), "{events:?}");
    t.finish(Duration::from_secs(10)).unwrap();
    let sessions = server.finish();
    assert!(sessions[0].discarded_bytes > 600 * 3_000);
}
