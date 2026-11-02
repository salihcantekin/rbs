//! Micro-benchmarks for `Transport::send` of the file transport.
//! Run with `cargo bench -p rbs-transport-file --bench send`.
//!
//! The target is `/dev/null` (a temporary file elsewhere), so the I/O thread keeps up
//! and the numbers are the cost of `send`, not of the disk. A `QueueFull` is retried
//! and its cost is part of the measurement.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rbs_core::media::{Bytes, ChunkKind, ContainerId, MuxChunk};
use rbs_core::time::Timestamp;
use rbs_transport::{
    QueueLimits, TransportConfig, TransportError, TransportFactory, TransportInput, TransportItem,
};
use rbs_transport_file::FileTransportFactory;

fn bench_send(c: &mut Criterion) {
    let path = if cfg!(unix) {
        std::path::PathBuf::from("/dev/null")
    } else {
        std::env::temp_dir().join(format!("rbs-file-bench-{}", std::process::id()))
    };
    let mut group = c.benchmark_group("file_send");
    // One TS packet group (7 x 188 bytes) and one 150 KB keyframe fragment.
    for (name, len) in [("chunk_1316", 1316usize), ("chunk_150k", 150_000)] {
        let config = TransportConfig::new(
            format!("file://{}", path.display()),
            TransportInput::Container(ContainerId::MpegTs),
        )
        .with_queue(QueueLimits::new(
            64 * 1024 * 1024,
            Duration::from_secs(3600),
        ));
        let mut t = FileTransportFactory.create(&config).unwrap();
        t.start(&[]).unwrap();
        let data = Bytes::from(vec![0x47; len]);
        let _shared = data.clone();
        let chunk = MuxChunk::new(
            ChunkKind::Media {
                keyframe: true,
                timestamp: Timestamp::from_nanos(1_000_000_000),
            },
            data,
        );
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                while let Err(TransportError::QueueFull) =
                    t.send(TransportItem::Chunk(black_box(&chunk)))
                {
                    std::hint::spin_loop();
                }
            });
        });
        t.finish(Duration::from_secs(30)).unwrap();
    }
    group.finish();
    if !cfg!(unix) {
        let _ = std::fs::remove_file(&path);
    }
}

criterion_group!(benches, bench_send);
criterion_main!(benches);
