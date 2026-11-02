//! File transport behaviour the suite cannot check: never blocking on a stalled file,
//! QueueFull, patches, errors and thread shutdown (output-ports spec, invariants 2 and
//! 4 and "QueueFull").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rbs_core::media::{Bytes, ChunkKind, ContainerId, MuxChunk};
use rbs_core::time::Timestamp;
use rbs_transport::{
    QueueLimits, SendOutcome, TransportConfig, TransportError, TransportEvent, TransportFactory,
    TransportInput, TransportItem,
};
use rbs_transport_file::FileTransportFactory;

fn temp(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rbs-file-transport-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn config(path: &std::path::Path) -> TransportConfig {
    TransportConfig::new(
        format!("file://{}", path.display()),
        TransportInput::Container(ContainerId::MpegTs),
    )
}

fn media(ms: u64, len: usize) -> MuxChunk {
    MuxChunk::new(
        ChunkKind::Media {
            keyframe: ms == 0,
            timestamp: Timestamp::from_nanos(1_000_000_000 + ms * 1_000_000),
        },
        Bytes::from(vec![(ms % 251) as u8; len]),
    )
}

#[test]
fn writes_in_order_and_applies_patches() {
    let path = temp("patch");
    let mut t = FileTransportFactory.create(&config(&path)).unwrap();
    t.start(&[]).unwrap();
    let header = MuxChunk::new(ChunkKind::Header, Bytes::from_static(b"HEADER"));
    t.send(TransportItem::Chunk(&header)).unwrap();
    for i in 0..100 {
        assert_eq!(
            t.send(TransportItem::Chunk(&media(i, 1000))).unwrap(),
            SendOutcome::Queued
        );
    }
    let patch = MuxChunk::new(ChunkKind::Patch { offset: 1 }, Bytes::from_static(b"XY"));
    t.send(TransportItem::Chunk(&patch)).unwrap();
    let tail = MuxChunk::new(ChunkKind::Trailer, Bytes::from_static(b"END"));
    t.send(TransportItem::Chunk(&tail)).unwrap();
    t.finish(Duration::from_secs(5)).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes.len(),
        6 + 100 * 1000 + 3,
        "patch overwrites, does not append"
    );
    assert_eq!(&bytes[..6], b"HXYDER");
    assert_eq!(
        &bytes[bytes.len() - 3..],
        b"END",
        "writes after a patch go to the end"
    );
    let mut events = Vec::new();
    t.poll_events(&mut events);
    assert!(matches!(
        t.send(TransportItem::Chunk(&header)),
        Err(TransportError::Closed)
    ));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn open_errors_are_reported_once_and_returned() {
    // A directory cannot be opened as a file.
    let dir = std::env::temp_dir();
    let mut t = FileTransportFactory
        .create(&TransportConfig::new(
            format!("file://{}", dir.display()),
            TransportInput::Container(ContainerId::Flv),
        ))
        .unwrap();
    t.start(&[]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while events.is_empty() && Instant::now() < deadline {
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    assert!(
        matches!(
            events.as_slice(),
            [TransportEvent::Disconnected {
                error: TransportError::Io(_)
            }]
        ),
        "{events:?}"
    );
    let mut again = Vec::new();
    t.poll_events(&mut again);
    assert!(again.is_empty(), "reported once");
    assert!(matches!(
        t.send(TransportItem::Chunk(&media(0, 10))),
        Err(TransportError::Io(_))
    ));
    assert!(t.finish(Duration::from_secs(1)).is_err());
}

#[test]
fn rejects_packet_input_and_other_schemes() {
    let f = FileTransportFactory;
    assert!(
        f.create(&TransportConfig::new(
            "file:///tmp/x",
            TransportInput::Packets
        ))
        .is_err()
    );
    assert!(
        f.create(&TransportConfig::new(
            "rtmp://host/app",
            TransportInput::Container(ContainerId::Flv)
        ))
        .is_err()
    );
}

/// A FIFO with no reader blocks the I/O thread inside `open`, the worst stall a file
/// can have. `send` must still return at once and report `QueueFull` at the limit.
#[cfg(unix)]
#[test]
fn never_blocks_on_a_stalled_file_and_reports_queue_full() {
    let path = temp("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    let limits = QueueLimits::new(64 * 1024, Duration::from_secs(10));
    let mut t = FileTransportFactory
        .create(&config(&path).with_queue(limits))
        .unwrap();
    t.start(&[]).unwrap();
    let mut slowest = Duration::ZERO;
    let mut queued = 0;
    let mut full = 0;
    for i in 0..200 {
        let chunk = media(i * 33, 4096);
        let start = Instant::now();
        let result = t.send(TransportItem::Chunk(&chunk));
        slowest = slowest.max(start.elapsed());
        match result {
            Ok(SendOutcome::Queued) => queued += 1,
            Err(e @ TransportError::QueueFull) => {
                assert!(e.is_recoverable());
                full += 1;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        slowest < Duration::from_millis(50),
        "slowest send took {slowest:?}"
    );
    assert_eq!(queued, 16, "64 KiB of 4 KiB chunks");
    assert_eq!(full, 184);
    assert_eq!(t.stats().queued_bytes, 64 * 1024);

    // Unblock the thread by opening the reading end, then finish: everything queued
    // arrives.
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || std::fs::read(reader_path).unwrap());
    t.finish(Duration::from_secs(5)).unwrap();
    let received = reader.join().unwrap();
    assert_eq!(received.len(), 16 * 4096);
    std::fs::remove_file(&path).unwrap();
}

#[cfg(unix)]
#[test]
fn finish_times_out_and_drop_returns_on_a_stalled_file() {
    let path = temp("fifo-timeout");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let mut t = FileTransportFactory.create(&config(&path)).unwrap();
    t.start(&[]).unwrap();
    t.send(TransportItem::Chunk(&media(0, 100))).unwrap();
    let start = Instant::now();
    assert!(matches!(
        t.finish(Duration::from_millis(100)),
        Err(TransportError::Timeout)
    ));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "bounded by timeout plus grace"
    );
    // Release the detached thread.
    let _ = std::fs::read(&path);
    drop(t);

    let mut dropped = FileTransportFactory.create(&config(&path)).unwrap();
    dropped.start(&[]).unwrap();
    let start = Instant::now();
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::read(reader_path);
    });
    drop(dropped);
    assert!(start.elapsed() < Duration::from_secs(3));
    reader.join().unwrap();
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn duration_limit_applies_while_nothing_is_written() {
    // Before the thread has written anything, the queued duration is newest minus
    // first queued timestamp.
    let path = temp("duration");
    let limits = QueueLimits::new(usize::MAX, Duration::from_millis(500));
    let mut t = FileTransportFactory
        .create(&config(&path).with_queue(limits))
        .unwrap();
    t.start(&[]).unwrap();
    // Whether the thread keeps up decides how many fit; the limit must never let more
    // than 500 ms queue up, and a healthy file eventually takes everything.
    for i in 0..40 {
        let chunk = media(i * 33, 10);
        while let Err(TransportError::QueueFull) = t.send(TransportItem::Chunk(&chunk)) {
            std::thread::yield_now();
        }
    }
    t.finish(Duration::from_secs(5)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len(), 400);
    std::fs::remove_file(&path).unwrap();
}
