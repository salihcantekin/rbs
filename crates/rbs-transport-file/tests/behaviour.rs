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
    assert_eq!(
        t.stats().bytes_sent,
        6 + 100 * 1000 + 2 + 3,
        "counters survive finish"
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
fn a_patch_beyond_the_end_fails_the_recording() {
    let path = temp("patch-beyond");
    let mut t = FileTransportFactory.create(&config(&path)).unwrap();
    t.start(&[]).unwrap();
    for chunk in [
        MuxChunk::new(ChunkKind::Header, Bytes::from_static(b"ABCD")),
        MuxChunk::new(ChunkKind::Patch { offset: 100 }, Bytes::from_static(b"ZZ")),
        MuxChunk::new(ChunkKind::Patch { offset: 3 }, Bytes::from_static(b"ZZ")),
        MuxChunk::new(ChunkKind::Trailer, Bytes::from_static(b"END")),
    ] {
        let _ = t.send(TransportItem::Chunk(&chunk));
    }
    assert!(matches!(
        t.finish(Duration::from_secs(5)),
        Err(TransportError::Io(_))
    ));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"ABCD",
        "nothing after the error"
    );
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
    assert!(
        matches!(
            t.send(TransportItem::Chunk(&media(1, 10))),
            Err(TransportError::Closed)
        ),
        "the error is returned once"
    );
    assert!(t.finish(Duration::from_secs(1)).is_err());
}

/// A device that fails every write with "no space left".
#[cfg(target_os = "linux")]
#[test]
fn write_errors_fail_the_recording() {
    let mut t = FileTransportFactory
        .create(&TransportConfig::new(
            "file:///dev/full",
            TransportInput::Container(ContainerId::MpegTs),
        ))
        .unwrap();
    t.start(&[]).unwrap();
    // Larger than the write buffer, so it is written at once.
    t.send(TransportItem::Chunk(&media(0, 512 * 1024))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|e| matches!(e, TransportEvent::Disconnected { .. }))
    {
        assert!(Instant::now() < deadline, "no error reported: {events:?}");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    assert!(matches!(
        t.send(TransportItem::Chunk(&media(33, 10))),
        Err(TransportError::Io(_))
    ));
    assert!(matches!(
        t.finish(Duration::from_secs(5)),
        Err(TransportError::Io(_))
    ));
}

#[test]
fn refuses_bad_urls_and_adapter_options() {
    let f = FileTransportFactory;
    let input = TransportInput::Container(ContainerId::MpegTs);
    for url in ["file://", "file://relative/x", "file:///a%2"] {
        assert!(
            f.create(&TransportConfig::new(url, input)).is_err(),
            "{url}"
        );
    }
    let mut with_option = TransportConfig::new("file:///tmp/x", input);
    with_option
        .adapter_options
        .push(("fsync".into(), "never".into()));
    assert!(f.create(&with_option).is_err());
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
    let path = mkfifo("fifo");
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
        slowest < rbs_transport::conformance::SEND_BUDGET,
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
fn mkfifo(name: &str) -> PathBuf {
    let path = temp(name);
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    path
}

/// Opens the reading end of a FIFO on a helper thread (opening blocks until the
/// writer opens it) and hands the open file back.
#[cfg(unix)]
fn open_reader(path: &std::path::Path) -> std::thread::JoinHandle<std::fs::File> {
    let path = path.to_owned();
    std::thread::spawn(move || std::fs::File::open(path).unwrap())
}

#[cfg(unix)]
#[test]
fn finish_and_drop_return_within_their_budget_on_a_stalled_file() {
    let path = mkfifo("fifo-timeout");
    let mut t = FileTransportFactory.create(&config(&path)).unwrap();
    t.start(&[]).unwrap();
    t.send(TransportItem::Chunk(&media(0, 100))).unwrap();
    let start = Instant::now();
    assert!(matches!(
        t.finish(Duration::from_millis(100)),
        Err(TransportError::Timeout)
    ));
    // The session waits at most 100 ms; the margin is for OS timer slack and
    // scheduling on loaded CI machines (the suite's own `send` margin).
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(100) + rbs_transport::conformance::SEND_BUDGET,
        "{took:?}"
    );
    // Release the detached thread, which is blocked opening the FIFO.
    drop(open_reader(&path).join().unwrap());
    drop(t);

    // Dropped while blocked in `open`: returns after its short grace period. Only then
    // is the reader opened, so the blocked case is always the one exercised.
    let mut dropped = FileTransportFactory.create(&config(&path)).unwrap();
    dropped.start(&[]).unwrap();
    let start = Instant::now();
    drop(dropped);
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(open_reader(&path).join().unwrap());
    std::fs::remove_file(&path).unwrap();
}

/// The reader opens the FIFO but never reads: the I/O thread is blocked in `write`
/// (the chunk is larger than the pipe and the write buffer), or holds unwritten
/// buffered bytes. `Drop` must return in both cases without flushing again.
#[cfg(unix)]
#[test]
fn drop_returns_while_a_write_is_blocked() {
    for (name, len) in [("fifo-buffered", 200 * 1024), ("fifo-write", 1024 * 1024)] {
        let path = mkfifo(name);
        let mut t = FileTransportFactory.create(&config(&path)).unwrap();
        t.start(&[]).unwrap();
        let reader = open_reader(&path).join().unwrap();
        t.send(TransportItem::Chunk(&media(0, len))).unwrap();
        // Wait until the chunk left the queue (buffered, or the write is blocked).
        let deadline = Instant::now() + Duration::from_secs(5);
        while t.stats().queued_bytes > 0 && len < 512 * 1024 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let start = Instant::now();
        drop(t);
        assert!(start.elapsed() < Duration::from_secs(1), "{name}");
        // Closing the reader fails the blocked write (EPIPE); the thread then exits.
        drop(reader);
        std::fs::remove_file(&path).unwrap();
    }
}

/// With the I/O thread stalled (blocked opening a FIFO), the duration limit binds:
/// chunks 33 ms apart, a 500 ms limit, so 0..=495 ms are queued and 528 ms is refused.
#[cfg(unix)]
#[test]
fn duration_limit_binds_while_nothing_is_written() {
    let path = mkfifo("fifo-duration");
    let limits = QueueLimits::new(usize::MAX, Duration::from_millis(500));
    let mut t = FileTransportFactory
        .create(&config(&path).with_queue(limits))
        .unwrap();
    t.start(&[]).unwrap();
    for i in 0..16 {
        assert_eq!(
            t.send(TransportItem::Chunk(&media(i * 33, 10))).unwrap(),
            SendOutcome::Queued,
            "chunk {i}"
        );
    }
    assert!(matches!(
        t.send(TransportItem::Chunk(&media(16 * 33, 10))),
        Err(TransportError::QueueFull)
    ));
    // Headers and patches carry no timestamp and are not held back by it.
    let trailer = MuxChunk::new(ChunkKind::Trailer, Bytes::from_static(b"END"));
    t.send(TransportItem::Chunk(&trailer)).unwrap();
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || std::fs::read(reader_path).unwrap());
    t.finish(Duration::from_secs(5)).unwrap();
    assert_eq!(reader.join().unwrap().len(), 16 * 10 + 3);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_healthy_file_takes_everything_within_the_duration_limit() {
    let path = temp("duration");
    let limits = QueueLimits::new(usize::MAX, Duration::from_millis(500));
    let mut t = FileTransportFactory
        .create(&config(&path).with_queue(limits))
        .unwrap();
    t.start(&[]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    for i in 0..40 {
        let chunk = media(i * 33, 10);
        while let Err(TransportError::QueueFull) = t.send(TransportItem::Chunk(&chunk)) {
            assert!(Instant::now() < deadline, "the writer never caught up");
            std::thread::yield_now();
        }
    }
    t.finish(Duration::from_secs(5)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len(), 400);
    std::fs::remove_file(&path).unwrap();
}
