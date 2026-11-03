//! Micro-benchmarks for `Transport::send` of the SRT transport.
//! Run with `cargo bench -p rbs-transport-srt --bench send`.
//!
//! The destination is a local listener that only counts what it receives (and
//! acknowledges it), so the I/O thread keeps up and the numbers are the cost of
//! `send` (classification, staging and the hand-off), not of the network. Each
//! sample times a burst of sends of the same chunk, then waits (untimed) until the
//! I/O thread has taken the burst, so the queue never backs up and nothing is
//! dropped.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rbs_core::media::{Bytes, ChunkKind, ContainerId, MuxChunk};
use rbs_core::time::Timestamp;
use rbs_transport::{
    QueueLimits, Secret, TransportConfig, TransportEvent, TransportFactory, TransportInput,
    TransportItem,
};
use rbs_transport_srt::SrtTransportFactory;
use support::{Behaviour, TestServer};

/// Sends timed back to back before waiting for the I/O thread: a burst like a
/// keyframe's chunks with their audio.
const BURST: u64 = 16;

fn media(key: bool, len: usize) -> MuxChunk {
    let data = Bytes::from(vec![0x47; len]);
    // Shared once, as a muxer's chunk is by the time a transport clones it.
    let _ = data.clone();
    MuxChunk::new(
        ChunkKind::Media {
            keyframe: key,
            timestamp: Timestamp::from_nanos(1_000_000_000),
        },
        data,
    )
}

fn bench_send(c: &mut Criterion) {
    let server = TestServer::start(Behaviour {
        sink: true,
        ..Behaviour::default()
    });
    let config = TransportConfig::new(server.url(), TransportInput::Container(ContainerId::MpegTs))
        .with_secret(Secret::new("bench"))
        .with_queue(QueueLimits::new(
            256 * 1024 * 1024,
            Duration::from_secs(3600),
        ));
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events.contains(&TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    // The stream starts at a keyframe chunk.
    let first = media(true, 188 * 7);
    t.send(TransportItem::Chunk(&first)).unwrap();
    let cases = [
        ("ts_inter_3k", media(false, 188 * 16)),
        ("ts_key_150k", media(true, 188 * 800)),
        ("ts_audio_376", media(false, 188 * 2)),
    ];
    let mut group = c.benchmark_group("srt_send");
    for (name, chunk) in &cases {
        group.throughput(Throughput::Bytes(chunk.data.len() as u64));
        group.bench_function(*name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                let mut left = iters;
                while left > 0 {
                    let burst = left.min(BURST);
                    left -= burst;
                    let started = Instant::now();
                    for _ in 0..burst {
                        black_box(t.send(TransportItem::Chunk(black_box(chunk))).unwrap());
                    }
                    total += started.elapsed();
                    while t.stats().queued_bytes > 0 {
                        t.poll_events(&mut events);
                        std::hint::spin_loop();
                    }
                }
                total
            });
        });
    }
    group.finish();
    t.finish(Duration::from_secs(30)).unwrap();
    assert_eq!(t.stats().items_dropped, 0, "the I/O thread kept up");
    drop(server);
}

criterion_group!(benches, bench_send);
criterion_main!(benches);
