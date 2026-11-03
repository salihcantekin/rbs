//! Heap allocations on the SRT transport's send path (output-ports spec, hot-path
//! rules): none in steady state. `send` clones the chunk (a reference count) into
//! pre-allocated queues; the I/O thread cuts it into payload slices (reference
//! counts too), keeps them in a pre-allocated window and writes datagrams from a
//! reused buffer. The counter is global, so the I/O thread and the test listener
//! (which only counts in this test) are measured too.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_core::media::{Bytes, ChunkKind, ContainerId, MuxChunk};
use rbs_core::time::Timestamp;
use rbs_transport::{
    QueueLimits, Secret, SendOutcome, Transport, TransportConfig, TransportEvent, TransportFactory,
    TransportInput, TransportItem,
};
use rbs_transport_srt::SrtTransportFactory;
use support::{Behaviour, TestServer};

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

/// TS-sized chunks of a 30 fps stream with one keyframe chunk per second.
fn chunks(count: u64) -> Vec<MuxChunk> {
    (0..count)
        .map(|i| {
            let key = i % 30 == 0;
            let len = if key { 188 * 160 } else { 188 * 16 };
            let data = Bytes::from(vec![(i % 251) as u8; len]);
            // Share once, so cloning in `send` is a reference-count increment.
            let _ = data.clone();
            MuxChunk::new(
                ChunkKind::Media {
                    keyframe: key,
                    timestamp: Timestamp::from_nanos(1_000_000_000 + i * 33_333_333),
                },
                data,
            )
        })
        .collect()
}

fn send_all(t: &mut Box<dyn Transport>, chunks: &[MuxChunk]) {
    for chunk in chunks {
        assert_eq!(t.send(TransportItem::Chunk(chunk)), Ok(SendOutcome::Queued));
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
    let config = TransportConfig::new(server.url(), TransportInput::Container(ContainerId::MpegTs))
        .with_secret(Secret::new("alloc-test"))
        .with_queue(QueueLimits::new(usize::MAX, Duration::from_secs(3600)));
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let all = chunks(1200);
    let (warm, measured) = all.split_at(600);
    let mut events = Vec::with_capacity(64);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events.contains(&TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    // Warm up both sides: buffers reach their working size, ACKs and ACKACKs flow.
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
    assert_eq!(t.stats().items_dropped, 0);
    // Delivery itself is checked by the behaviour tests: on a loaded machine the
    // loopback can drop more than the sender keeps for retransmission.
    let sessions = server.finish();
    assert!(sessions[0].received_bytes > 0);
}
