//! Heap allocations on the file transport's `send` path (output-ports spec, hot-path
//! rules): none. `send` clones the chunk's `Bytes` (a reference count) into the
//! pre-allocated ring, and the I/O thread writes through its pre-allocated buffer.
//! The counter is global, so the I/O thread's allocations would count too.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{Bytes, ChunkKind, ContainerId, MuxChunk};
use rbs_core::time::Timestamp;
use rbs_transport::{
    QueueLimits, TransportConfig, TransportError, TransportFactory, TransportInput, TransportItem,
};
use rbs_transport_file::FileTransportFactory;

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
fn send_does_not_allocate() {
    let path = std::env::temp_dir().join(format!("rbs-file-alloc-{}", std::process::id()));
    let config = TransportConfig::new(
        format!("file://{}", path.display()),
        TransportInput::Container(ContainerId::MpegTs),
    )
    .with_queue(QueueLimits::new(usize::MAX, Duration::from_secs(3600)));
    let mut t = FileTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let chunks: Vec<MuxChunk> = (0..3000u64)
        .map(|i| {
            let data = Bytes::from(vec![(i % 251) as u8; 1316]);
            // Share once, so cloning in `send` is a reference-count increment.
            let _ = data.clone();
            MuxChunk::new(
                ChunkKind::Media {
                    keyframe: i % 30 == 0,
                    timestamp: Timestamp::from_nanos(1_000_000_000 + i * 1_000_000),
                },
                data,
            )
        })
        .collect();
    let (warm, measured) = chunks.split_at(500);
    let send_all = |t: &mut Box<dyn rbs_transport::Transport>, chunks: &[MuxChunk]| {
        for chunk in chunks {
            while let Err(TransportError::QueueFull) = t.send(TransportItem::Chunk(chunk)) {
                std::hint::spin_loop();
            }
        }
    };
    // Warm up both sides, including the I/O thread going idle and being woken (its
    // first park sets up thread-local state).
    for part in warm.chunks(100) {
        send_all(&mut t, part);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while t.stats().queued_bytes > 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    send_all(&mut t, measured);
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(allocs, 0, "allocations for {} sends", measured.len());
    t.finish(Duration::from_secs(10)).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 3000 * 1316);
    std::fs::remove_file(&path).unwrap();
}
