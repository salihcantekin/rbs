//! SRT transport behaviour, checked against the local test listener: the handshake
//! and its extensions, packetisation, loss recovery, rejections, reconnects,
//! stalls and shutdown (output-ports spec, SRT transport and invariants 4-7).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rbs_core::media::{BitstreamFormat, CodecId};
use rbs_core::media::{Bytes, ChunkKind, ContainerId, EncodedPacket, MuxChunk, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_transport::{
    CreateError, QueueLimits, ReconnectPolicy, Secret, SendOutcome, Transport, TransportConfig,
    TransportError, TransportEvent, TransportFactory, TransportInput, TransportItem,
};
use rbs_transport_srt::SrtTransportFactory;
use support::{Behaviour, TestServer};

const STREAM_ID: &str = "#!::r=live/9f8e7d6c5b4a,m=publish";
const PAYLOAD: usize = 1316;
const MAGIC: u8 = 0xC7;

fn config(url: &str) -> TransportConfig {
    TransportConfig::new(url, TransportInput::Container(ContainerId::MpegTs))
        .with_secret(Secret::new(STREAM_ID))
}

fn header() -> MuxChunk {
    MuxChunk::new(ChunkKind::Header, Bytes::from(vec![0x47; 376]))
}

/// A media chunk that names itself: magic, index, length and keyframe flag, then
/// filler, so the delivered byte stream can be split back into chunks.
fn chunk(index: u32, key: bool, len: usize) -> MuxChunk {
    let mut data = vec![MAGIC];
    data.extend_from_slice(&index.to_be_bytes());
    data.extend_from_slice(&(len as u32).to_be_bytes());
    data.push(u8::from(key));
    data.resize(len.max(10), (index % 251) as u8);
    MuxChunk::new(
        ChunkKind::Media {
            keyframe: key,
            timestamp: Timestamp::from_nanos(1_000_000_000 + u64::from(index) * 33_333_333),
        },
        Bytes::from(data),
    )
}

/// Splits a delivered byte stream back into `(index, keyframe)` of its media
/// chunks, after `skip` header bytes.
fn delivered(bytes: &[u8], skip: usize) -> Vec<(u32, bool)> {
    let mut out = Vec::new();
    let mut rest = &bytes[skip..];
    while !rest.is_empty() {
        assert_eq!(rest[0], MAGIC, "chunk boundary expected");
        let index = u32::from_be_bytes(rest[1..5].try_into().unwrap());
        let len = u32::from_be_bytes(rest[5..9].try_into().unwrap()) as usize;
        out.push((index, rest[9] == 1));
        rest = &rest[len..];
    }
    out
}

fn send(t: &mut Box<dyn Transport>, chunk: &MuxChunk) -> SendOutcome {
    t.send(TransportItem::Chunk(chunk)).unwrap()
}

/// Polls events until `pred` matches one, or panics after 10 s.
fn wait_for(
    t: &mut Box<dyn Transport>,
    events: &mut Vec<TransportEvent>,
    pred: impl Fn(&TransportEvent) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let start = events.len();
        t.poll_events(events);
        if events[start..].iter().any(&pred) {
            return;
        }
        assert!(Instant::now() < deadline, "no matching event in {events:?}");
        std::thread::yield_now();
    }
}

#[test]
fn sends_the_handshake_then_whole_chunks_in_order() {
    let server = TestServer::start(Behaviour::default());
    let mut t = SrtTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&[]).unwrap();
    let mut chunks = vec![header()];
    for i in 0..30 {
        chunks.push(chunk(i, i % 10 == 0, 100 + (i as usize * 397) % 6000));
    }
    let mut expected = Vec::new();
    for c in &chunks {
        assert_eq!(send(&mut t, c), SendOutcome::Queued);
        expected.extend_from_slice(&c.data);
    }
    // Patches are accepted and ignored: a live stream cannot be rewritten.
    let patch = MuxChunk::new(ChunkKind::Patch { offset: 0 }, Bytes::from_static(b"PATCH"));
    assert_eq!(send(&mut t, &patch), SendOutcome::Queued);
    let trailer = MuxChunk::new(ChunkKind::Trailer, Bytes::from_static(b"END"));
    send(&mut t, &trailer);
    expected.extend_from_slice(b"END");
    chunks.push(trailer);
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    t.finish(Duration::from_secs(5)).unwrap();
    let stats = t.stats();
    assert_eq!(stats.items_dropped, 0);
    assert_eq!(stats.queued_bytes, 0);
    assert_eq!(
        stats.rtt,
        Some(Duration::from_millis(2)),
        "the listener's RTT"
    );
    assert!(stats.bytes_sent >= expected.len() as u64);

    let sessions = server.finish();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.stream_id.as_deref(), Some(STREAM_ID));
    assert_eq!(s.hsreq, Some([0x0001_0500, 0x3F, (120 << 16) | 120]));
    assert_eq!(s.payload, expected);
    assert_eq!(s.missing, 0);
    assert!(s.shutdown, "closed with a shutdown");
    assert!(s.ackacks > 0, "full ACKs are answered");
    // One solo message per packet, numbered from 1, never spanning two chunks.
    let sizes: Vec<usize> = chunks
        .iter()
        .flat_map(|c| c.data.chunks(PAYLOAD).map(<[u8]>::len))
        .collect();
    assert_eq!(s.packets.iter().map(|p| p.1).collect::<Vec<_>>(), sizes);
    for (i, (word, _)) in s.packets.iter().enumerate() {
        assert_eq!(word >> 30, 0b11, "solo message");
        assert_eq!((word >> 27) & 0b11, 0, "not encrypted");
        assert_eq!(word & 0x03FF_FFFF, i as u32 + 1, "message number");
    }
}

#[test]
#[ignore = "slow: waits out the 4 s SRT latency in real time; runs in the slow tier (ADR-0032)"]
fn slow_retransmits_what_the_listener_reports_lost() {
    let server = TestServer::start(Behaviour {
        lose_every: Some(7),
        ..Behaviour::default()
    });
    // A long latency keeps packets for retransmission even on a starved machine
    // (they are kept for the latency, at least 1 s).
    let config = config(&server.url()).with_adapter_option("latency_ms", "4000");
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut expected = Vec::new();
    // 56 chunks of three packets: the last packet (offset 167) is lost too, which
    // only the sender's retransmission timer recovers.
    for i in 0..56 {
        let c = chunk(i, i % 14 == 0, 3000);
        assert_eq!(send(&mut t, &c), SendOutcome::Queued);
        expected.extend_from_slice(&c.data);
    }
    t.finish(Duration::from_secs(10)).unwrap();
    let sessions = server.finish();
    let s = &sessions[0];
    assert_eq!(s.missing, 0);
    assert!(
        s.retransmitted >= 56 * 3 / 7,
        "{} retransmitted",
        s.retransmitted
    );
    assert_eq!(s.payload, expected, "lost packets were sent again");
}

#[test]
fn rejection_is_reported_once_without_the_stream_id_and_not_retried() {
    let server = TestServer::start(Behaviour {
        reject: Some(1403),
        ..Behaviour::default()
    });
    let config = config(&server.url()).with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(5),
        attempts: Some(5),
    });
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| {
        matches!(e, TransportEvent::Disconnected { .. })
    });
    let error = events
        .iter()
        .find_map(|e| match e {
            TransportEvent::Disconnected { error } => Some(error.clone()),
            _ => None,
        })
        .unwrap();
    let TransportError::Rejected(text) = &error else {
        panic!("{error:?}");
    };
    assert!(text.contains("1403"), "{text}");
    assert!(!format!("{events:?}").contains("9f8e7d6c5b4a"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TransportEvent::Reconnecting { .. })),
        "rejections are not retried: {events:?}"
    );
    let c = chunk(0, true, 100);
    assert_eq!(t.send(TransportItem::Chunk(&c)), Err(error.clone()));
    assert_eq!(
        t.send(TransportItem::Chunk(&c)),
        Err(TransportError::Closed)
    );
    assert_eq!(t.finish(Duration::from_secs(1)), Err(error));
    assert!(server.finish().is_empty());
}

#[test]
fn reconnects_resending_headers_and_resuming_at_a_keyframe() {
    // The listener closes the first connection after five packets: the header, a
    // keyframe chunk and three inter chunks.
    let server = TestServer::start(Behaviour {
        close_after: Some(5),
        ..Behaviour::default()
    });
    let config = config(&server.url()).with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        attempts: Some(10),
    });
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut events = Vec::new();
    let h = header();
    send(&mut t, &h);
    for i in 0..4 {
        send(&mut t, &chunk(i, i == 0, 500));
    }
    wait_for(&mut t, &mut events, |e| {
        matches!(e, TransportEvent::Reconnecting { attempt: 1 })
    });
    // Inter chunks continue the lost GOP and are not sent; the keyframe resumes.
    for (i, key) in [(4, false), (5, false), (6, true), (7, false), (8, false)] {
        send(&mut t, &chunk(i, key, 500));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.stats().reconnects == 0 {
        assert!(Instant::now() < deadline, "no reconnect");
        std::thread::yield_now();
    }
    t.poll_events(&mut events);
    t.finish(Duration::from_secs(5)).unwrap();
    assert!(
        events.contains(&TransportEvent::KeyframeRequested),
        "{events:?}"
    );
    let reconnecting = events
        .iter()
        .position(|e| matches!(e, TransportEvent::Reconnecting { .. }))
        .unwrap();
    assert!(
        events[reconnecting..].contains(&TransportEvent::Connected),
        "Connected after Reconnecting: {events:?}"
    );
    assert_eq!(t.stats().reconnects, 1);
    let sessions = server.finish();
    assert_eq!(sessions.len(), 2);
    let second = &sessions[1];
    assert_eq!(second.stream_id.as_deref(), Some(STREAM_ID));
    assert_eq!(second.payload[..h.data.len()], h.data[..], "header resent");
    assert_eq!(
        delivered(&second.payload, h.data.len()),
        [(6, true), (7, false), (8, false)],
        "resumes at the keyframe"
    );
}

#[test]
fn after_a_stall_media_resumes_at_a_keyframe_with_intact_gops() {
    let gate = Arc::new(AtomicBool::new(true));
    let server = TestServer::start(Behaviour {
        flow_window: 8,
        gate: Some(Arc::clone(&gate)),
        ..Behaviour::default()
    });
    let config =
        config(&server.url()).with_queue(QueueLimits::new(64 * 1024, Duration::from_secs(10)));
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    let h = header();
    send(&mut t, &h);
    let mut refused = 0;
    for i in 0..120 {
        if let SendOutcome::Dropped { .. } = send(&mut t, &chunk(i, i % 10 == 0, 4000)) {
            refused += 1;
        }
    }
    t.poll_events(&mut events);
    assert!(refused > 0, "a stalled listener must make the queue drop");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TransportEvent::Dropped { .. }))
    );
    assert!(events.contains(&TransportEvent::KeyframeRequested));
    gate.store(false, Ordering::SeqCst);
    t.finish(Duration::from_secs(5)).unwrap();
    assert!(t.stats().items_dropped > 0);
    let sessions = server.finish();
    let s = &sessions[0];
    assert_eq!(s.missing, 0);
    assert_eq!(s.payload[..h.data.len()], h.data[..], "header kept");
    let got = delivered(&s.payload, h.data.len());
    // GOPs of 40 KB do not fit next to the committed chunks in a 64 KB queue, so
    // every GOP after the first is cut; the last keyframe chunk always survives.
    assert!(got.len() < 120, "{got:?}");
    assert!(got.last().is_some_and(|(i, _)| *i >= 110), "{got:?}");
    assert!(got[0].1, "starts at a keyframe");
    for pair in got.windows(2) {
        let ((a, _), (b, key)) = (pair[0], pair[1]);
        assert!(
            key || b == a + 1,
            "chunk {b} continues a GOP that lost {a}..{b}"
        );
    }
}

/// Extra time a loaded CI runner may take to wake a thread after a timed wait. The
/// code keeps the budget exactly; the bound below only catches a call that ignores its
/// budget, so it is a hang guard with room for any scheduler delay, not a precision
/// check (ADR-0032; a 50 ms margin failed on macOS CI with 390 ms against 300 ms).
const SCHEDULING_SLACK: Duration = Duration::from_secs(1);

#[test]
fn finish_and_drop_respect_their_bounds_against_a_silent_listener() {
    let server = TestServer::start(Behaviour {
        silent: true,
        ..Behaviour::default()
    });
    let mut t = SrtTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&[]).unwrap();
    for i in 0..30 {
        send(&mut t, &chunk(i, i == 0, 1000));
    }
    let started = Instant::now();
    let result = t.finish(Duration::from_millis(300));
    assert!(
        started.elapsed() < Duration::from_millis(300) + SCHEDULING_SLACK,
        "{:?}",
        started.elapsed()
    );
    assert!(result.is_err(), "nothing was delivered: {result:?}");
    assert_eq!(t.stats().queued_bytes, 0);
    assert_eq!(t.stats().items_dropped, 30);

    let mut t = SrtTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&[]).unwrap();
    send(&mut t, &chunk(0, true, 1000));
    let started = Instant::now();
    drop(t);
    assert!(
        started.elapsed() < Duration::from_millis(200) + SCHEDULING_SLACK,
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn refuses_bad_configurations() {
    let ok = config("srt://127.0.0.1:9000").with_adapter_option("latency_ms", "400");
    assert!(SrtTransportFactory.create(&ok).is_ok());
    let no_id = TransportConfig::new(
        "srt://127.0.0.1:9000",
        TransportInput::Container(ContainerId::MpegTs),
    );
    assert!(
        SrtTransportFactory.create(&no_id).is_ok(),
        "the stream id is optional"
    );
    let cases = [
        config("rtmp://127.0.0.1/live"),
        config("srt://127.0.0.1"),
        config("srt://127.0.0.1:9000?streamid=x"),
        TransportConfig::new("srt://127.0.0.1:9000", TransportInput::Packets),
        TransportConfig::new(
            "srt://127.0.0.1:9000",
            TransportInput::Container(ContainerId::Flv),
        ),
        config("srt://127.0.0.1:9000").with_adapter_option("latency_ms", "70000"),
        config("srt://127.0.0.1:9000").with_adapter_option("passphrase", "x"),
        config("srt://127.0.0.1:9000").with_secret(Secret::new("x".repeat(513))),
        config("srt://127.0.0.1:9000").with_queue(QueueLimits::new(0, Duration::from_secs(1))),
        config("srt://127.0.0.1:9000").with_reconnect(ReconnectPolicy::Backoff {
            initial: Duration::ZERO,
            max: Duration::from_secs(1),
            attempts: None,
        }),
    ];
    for c in cases {
        match SrtTransportFactory.create(&c) {
            Err(CreateError::Unsupported { what }) => assert!(!what.contains("9f8e7d")),
            Err(CreateError::InvalidConfig(e)) => assert!(!e.contains("9f8e7d")),
            other => panic!("{c:?} created: {:?}", other.map(|_| ())),
        }
    }
}

#[test]
fn send_checks_the_session_state_and_item_kind() {
    let server = TestServer::start(Behaviour::default());
    let mut t = SrtTransportFactory.create(&config(&server.url())).unwrap();
    let c = chunk(0, true, 100);
    assert_eq!(
        t.send(TransportItem::Chunk(&c)),
        Err(TransportError::NotStarted)
    );
    t.start(&[]).unwrap();
    assert!(t.start(&[]).is_err(), "started twice");
    let t0 = Timestamp::from_nanos(0);
    let packet = EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        t0,
        t0,
        true,
        Bytes::from_static(&[0, 0, 0, 1, 0x65]),
    )
    .unwrap();
    assert!(matches!(
        t.send(TransportItem::Packet {
            stream: StreamIndex(0),
            packet: &packet
        }),
        Err(TransportError::InvalidInput(_))
    ));
    t.finish(Duration::from_secs(5)).unwrap();
    assert_eq!(
        t.send(TransportItem::Chunk(&c)),
        Err(TransportError::Closed)
    );
}

#[test]
fn a_late_listener_gets_recent_media_not_what_waited_in_the_hand_off() {
    // The listener answers only after 6 s of media (a keyframe every second) was sent:
    // the first chunks were committed to the hand-off long before the connection.
    let gate = Arc::new(std::sync::Barrier::new(2));
    let server = TestServer::start(Behaviour {
        accept_gate: Some(Arc::clone(&gate)),
        ..Behaviour::default()
    });
    let config = config(&server.url())
        .with_queue(QueueLimits::new(64 * 1024 * 1024, Duration::from_secs(3)));
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let head = header();
    send(&mut t, &head);
    let mut i = 0;
    let mut send_chunk = |t: &mut Box<dyn Transport>| {
        send(t, &chunk(i, i % 30 == 0, 500));
        i += 1;
    };
    for _ in 0..180 {
        send_chunk(&mut t);
    }
    gate.wait();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    // What was queued goes out before live media goes on, as it would in real time
    // (a frame every 33 ms); polling moves staged items to the I/O thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.stats().queued_bytes > 0 {
        assert!(Instant::now() < deadline, "the queue never drained");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    for _ in 0..60 {
        send_chunk(&mut t);
    }
    t.finish(Duration::from_secs(5)).unwrap();
    let sessions = server.finish();
    assert_eq!(sessions.len(), 1);
    let got = delivered(&sessions[0].payload, head.data.len());
    // The newest chunk before the connection is 179: nothing older than the 3 s
    // queue limit before it (90 chunks at 30 per second) goes out, and the stream
    // starts at a keyframe and is continuous from there.
    assert!(
        got.first().is_some_and(|c| c.1),
        "starts at a keyframe: {got:?}"
    );
    assert!(got[0].0 + 90 >= 179, "stale first chunk {}", got[0].0);
    for pair in got.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "{got:?}");
    }
    assert_eq!(got.last().map(|c| c.0), Some(239));
}
