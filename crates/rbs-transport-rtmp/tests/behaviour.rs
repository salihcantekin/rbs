//! RTMP transport behaviour, checked against the local test server: the publish
//! sequence, message framing, headers, timestamps, control messages, rejections,
//! reconnects and shutdown (output-ports spec, RTMP transport and invariants 4-7).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, ContainerId, EncodedPacket,
    PixelFormat, Size, StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_media_format::amf0::Decoded;
use rbs_media_format::flv::FlvTracks;
use rbs_transport::{
    CreateError, QueueLimits, ReconnectPolicy, Secret, SendOutcome, Transport, TransportConfig,
    TransportError, TransportEvent, TransportFactory, TransportInput, TransportItem,
};
use rbs_transport_rtmp::RtmpTransportFactory;
use support::{Behaviour, PPS, SPS, Session, TestServer};

const KEY: &str = "live_9f8e7d6c5b4a";
const VIDEO: StreamIndex = StreamIndex(0);
const AUDIO: StreamIndex = StreamIndex(1);

fn video_stream() -> StreamDescriptor {
    let mut extradata = vec![0, 0, 0, 1];
    extradata.extend_from_slice(SPS);
    extradata.extend_from_slice(&[0, 0, 0, 1]);
    extradata.extend_from_slice(PPS);
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
    )
}

fn audio_stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        Bytes::from_static(&[0x11, 0x90]),
        StreamParams::audio(AudioFormat::STEREO_48K, 1024),
    )
}

fn streams() -> Vec<StreamDescriptor> {
    vec![video_stream(), audio_stream()]
}

fn ms(n: u64) -> Timestamp {
    Timestamp::from_nanos(n * 1_000_000)
}

/// An Annex B access unit: delimiter, then one slice of `len` payload bytes without
/// start-code emulation.
fn video(dts_ms: u64, key: bool, len: usize) -> EncodedPacket {
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
    data.extend((0..len).map(|i| ((i * 7 + dts_ms as usize) % 250) as u8 + 1));
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        ms(dts_ms + 33),
        ms(dts_ms),
        key,
        Bytes::from(data),
    )
    .unwrap()
}

fn audio(dts_ms: u64) -> EncodedPacket {
    let data: Vec<u8> = (0..200).map(|i| (i + dts_ms) as u8).collect();
    EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        ms(dts_ms),
        ms(dts_ms),
        true,
        Bytes::from(data),
    )
    .unwrap()
}

fn config(url: &str) -> TransportConfig {
    TransportConfig::new(url, TransportInput::Packets).with_secret(Secret::new(KEY))
}

fn send(t: &mut Box<dyn Transport>, stream: StreamIndex, packet: &EncodedPacket) -> SendOutcome {
    t.send(TransportItem::Packet { stream, packet }).unwrap()
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

fn str_arg(values: &[Decoded], i: usize) -> Option<&str> {
    values.get(i).and_then(Decoded::as_str)
}

/// The FLV body the transport must send for `packet` (from the shared helpers).
fn expected_body(tracks: &FlvTracks, index: usize, packet: &EncodedPacket, cts: i64) -> Vec<u8> {
    let plan = tracks.plan(index, packet).unwrap();
    let mut out = Vec::new();
    tracks.write_body(plan, packet, cts, &mut out);
    out
}

#[test]
fn publishes_headers_then_packets() {
    let server = TestServer::start(Behaviour::default());
    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&streams()).unwrap();
    // Audio before the first keyframe is not sent: a connection starts at a keyframe.
    let early = audio(5000);
    let packets = [
        (VIDEO, video(5000, true, 9000)),
        (AUDIO, audio(5010)),
        (VIDEO, video(5033, false, 300)),
        (AUDIO, audio(5031)),
        (VIDEO, video(5066, false, 300)),
    ];
    assert_eq!(send(&mut t, AUDIO, &early), SendOutcome::Queued);
    for (stream, packet) in &packets {
        assert_eq!(send(&mut t, *stream, packet), SendOutcome::Queued);
    }
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    t.finish(Duration::from_secs(5)).unwrap();
    let stats = t.stats();
    assert_eq!(stats.items_dropped, 1, "the early audio packet");
    assert_eq!(stats.queued_bytes, 0);

    let sessions = server.finish();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert!(s.clean_eof, "the client closes the connection");
    assert_eq!(
        (s.messages[0].type_id, &s.messages[0].body[..]),
        (1, &4096_u32.to_be_bytes()[..]),
        "Set Chunk Size 4096 comes first"
    );
    assert_eq!(
        s.commands(),
        [
            "connect",
            "releaseStream",
            "FCPublish",
            "createStream",
            "publish",
            "FCUnpublish",
            "deleteStream"
        ]
    );
    let connect = s.messages[1].command().unwrap();
    let info = &connect[2];
    assert_eq!(info.get("app").and_then(Decoded::as_str), Some("live"));
    assert_eq!(
        info.get("tcUrl").and_then(Decoded::as_str),
        Some(server_url(&sessions, &connect).as_str())
    );
    assert_eq!(
        info.get("objectEncoding").and_then(Decoded::as_number),
        Some(0.0)
    );
    for name in ["releaseStream", "FCPublish", "publish"] {
        let m = s
            .messages
            .iter()
            .find(|m| m.command_name().as_deref() == Some(name))
            .unwrap();
        assert_eq!(str_arg(&m.command().unwrap(), 3), Some(KEY), "{name}");
    }
    let publish = s
        .messages
        .iter()
        .find(|m| m.command_name().as_deref() == Some("publish"))
        .unwrap();
    assert_eq!(publish.stream_id, 1, "publish goes on the created stream");
    assert_eq!(str_arg(&publish.command().unwrap(), 4), Some("live"));

    let media = s.media();
    let tracks = FlvTracks::new(&streams()).unwrap();
    // Headers, stamped with the first packet's time: origin is 1 s before it.
    assert_eq!(media.len(), 3 + packets.len());
    let data = rbs_media_format::amf0::decode_all(&media[0].body).unwrap();
    assert_eq!(media[0].type_id, 18);
    assert_eq!(str_arg(&data, 0), Some("@setDataFrame"));
    assert_eq!(str_arg(&data, 1), Some("onMetaData"));
    assert_eq!(
        data[2].get("width").and_then(Decoded::as_number),
        Some(320.0)
    );
    let mut header = Vec::new();
    tracks.video_sequence_header(&mut header);
    assert_eq!((media[1].type_id, &media[1].body), (9, &header));
    header.clear();
    tracks.audio_sequence_header(&mut header);
    assert_eq!((media[2].type_id, &media[2].body), (8, &header));
    for m in &media[..3] {
        assert_eq!(m.timestamp, 1000);
        assert_eq!(m.stream_id, 1);
    }
    for (m, (stream, packet)) in media[3..].iter().zip(&packets) {
        let dts = packet.dts().as_nanos() / 1_000_000 - 4000;
        let cts = if *stream == VIDEO { 33 } else { 0 };
        assert_eq!(m.timestamp, dts as u32);
        assert_eq!(m.type_id, if *stream == VIDEO { 9 } else { 8 });
        assert_eq!(
            m.body,
            expected_body(&tracks, usize::from(stream.0), packet, cts)
        );
    }
    assert_eq!(media[3].body[0], 0x17, "the first packet is a keyframe");
    assert!(events.contains(&TransportEvent::Connected), "{events:?}");
}

/// The tcUrl the client must send: the configured URL.
fn server_url(_sessions: &[Session], connect: &[Decoded]) -> String {
    let tc = connect[2].get("tcUrl").and_then(Decoded::as_str).unwrap();
    assert!(
        tc.starts_with("rtmp://127.0.0.1:") && tc.ends_with("/live"),
        "{tc}"
    );
    tc.to_owned()
}

#[test]
fn chunks_large_packets_with_extended_timestamps() {
    let server = TestServer::start(Behaviour::default());
    let config = config(&server.url()).with_queue(QueueLimits::new(
        64 * 1024 * 1024,
        Duration::from_secs(24 * 3600),
    ));
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&[video_stream()]).unwrap();
    // 5 hours later the RTMP time is past 0xFFFFFF ms: extended timestamps.
    let late = 1000 + 5 * 3600 * 1000;
    let packets = [
        video(1000, true, 100_000),
        video(1033, false, 4096),
        video(late, true, 50_000),
        video(late + 33, false, 10),
    ];
    for p in &packets {
        send(&mut t, VIDEO, p);
    }
    t.finish(Duration::from_secs(5)).unwrap();
    let sessions = server.finish();
    let media = sessions[0].media();
    let tracks = FlvTracks::new(&[video_stream()]).unwrap();
    assert_eq!(media.len(), 2 + packets.len());
    for (m, p) in media[2..].iter().zip(&packets) {
        let expected = (p.dts().as_nanos() / 1_000_000) as u32;
        assert_eq!(m.timestamp, expected);
        assert_eq!(m.body, expected_body(&tracks, 0, p, 33));
    }
    assert!(media[4].timestamp > 0x00ff_ffff);
}

#[test]
fn answers_pings_and_acknowledges_the_server_window() {
    let server = TestServer::start(Behaviour {
        ping: Some(0x0102_0304),
        window: Some(100),
        ..Behaviour::default()
    });
    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    for i in 0..10 {
        send(&mut t, VIDEO, &video(1000 + i * 33, i == 0, 100));
    }
    t.finish(Duration::from_secs(5)).unwrap();
    let sessions = server.finish();
    let messages = &sessions[0].messages;
    assert!(
        messages
            .iter()
            .any(|m| m.type_id == 4 && m.body == [0, 7, 1, 2, 3, 4]),
        "ping response"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.type_id == 5 && m.body == 2_500_000_u32.to_be_bytes()),
        "Window Acknowledgement Size answers Set Peer Bandwidth"
    );
    let ack = messages
        .iter()
        .find(|m| m.type_id == 3)
        .expect("an acknowledgement");
    let seq = u32::from_be_bytes(ack.body[..4].try_into().unwrap());
    assert!(
        seq >= 3073 + 100,
        "sequence number counts received bytes: {seq}"
    );
}

#[test]
fn rejection_is_reported_once_without_the_key_and_not_retried() {
    let server = TestServer::start(Behaviour {
        reject_publish: Some((
            "NetStream.Publish.BadName".into(),
            format!("stream {KEY} is not allowed"),
        )),
        ..Behaviour::default()
    });
    let config = config(&server.url()).with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(1),
        max: Duration::from_millis(1),
        attempts: None,
    });
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| {
        matches!(e, TransportEvent::Disconnected { .. })
    });
    let shown = format!("{events:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("BadName"), "{shown}");
    let p = video(1000, true, 10);
    let first = t.send(TransportItem::Packet {
        stream: VIDEO,
        packet: &p,
    });
    assert!(
        matches!(first, Err(TransportError::Rejected(_))),
        "{first:?}"
    );
    let again = t.send(TransportItem::Packet {
        stream: VIDEO,
        packet: &p,
    });
    assert_eq!(again, Err(TransportError::Closed));
    let finished = t.finish(Duration::from_secs(2));
    assert!(
        matches!(finished, Err(TransportError::Rejected(_))),
        "{finished:?}"
    );
    assert_eq!(server.finish().len(), 1, "a rejection is not retried");
}

#[test]
fn oversized_server_message_ends_the_connection() {
    let server = TestServer::start(Behaviour {
        oversized: Some(1_000_000),
        ..Behaviour::default()
    });
    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| {
        matches!(
            e,
            TransportEvent::Disconnected {
                error: TransportError::Connection(_)
            }
        )
    });
    assert!(
        format!("{events:?}").contains("larger than the limit"),
        "{events:?}"
    );
    let _ = t.finish(Duration::from_secs(1));
}

#[test]
fn reconnects_resending_headers_and_resuming_at_a_keyframe() {
    // The server drops the first connection after 3 headers and 4 packets.
    let server = TestServer::start(Behaviour {
        drop_after_media: Some(7),
        ..Behaviour::default()
    });
    let config = config(&server.url()).with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        attempts: Some(10),
    });
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    let mut n = 0;
    let mut next = |key: bool| {
        let p = video(1000 + n * 33, key, 200);
        n += 1;
        p
    };
    for i in 0..8 {
        let p = next(i == 0);
        send(&mut t, VIDEO, &p);
        send(&mut t, AUDIO, &audio(1000 + i * 33));
    }
    wait_for(&mut t, &mut events, |e| {
        matches!(e, TransportEvent::Reconnecting { attempt: 1 })
    });
    // Inter frames continue the lost GOP and are not sent; the keyframe resumes.
    let tail: Vec<(bool, EncodedPacket)> = [false, false, true, false, false]
        .into_iter()
        .map(|key| (key, next(key)))
        .collect();
    for (_, p) in &tail {
        send(&mut t, VIDEO, p);
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
    assert_eq!(second.commands()[..5], sessions[0].commands()[..5]);
    let media = second.media();
    assert_eq!(
        media.iter().map(|m| m.type_id).take(3).collect::<Vec<_>>(),
        [18, 9, 8],
        "headers are resent"
    );
    assert_eq!(media[1].body[..2], [0x17, 0], "AVC sequence header");
    assert_eq!(media[3].body[0], 0x17, "resumes at a keyframe");
    let resumed = &tail[2].1;
    assert_eq!(
        media[3].timestamp as u64,
        resumed.dts().as_nanos() / 1_000_000
    );
    assert_eq!(media.len(), 3 + 3, "keyframe and the two frames after it");
}

/// Extra time a loaded CI runner may take to wake a thread after a timed wait.
/// The code keeps the budget exactly; this only absorbs scheduler latency (a
/// 50 ms margin failed on macOS CI with 390 ms against a 300 ms budget).
const SCHEDULING_SLACK: Duration = Duration::from_millis(200);

#[test]
fn finish_and_drop_respect_their_bounds_against_a_silent_server() {
    let server = TestServer::start(Behaviour {
        silent: true,
        ..Behaviour::default()
    });
    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&streams()).unwrap();
    for i in 0..30 {
        send(&mut t, VIDEO, &video(1000 + i * 33, i == 0, 1000));
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

    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&streams()).unwrap();
    send(&mut t, VIDEO, &video(1000, true, 1000));
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
    let ok = config("rtmp://127.0.0.1/live");
    assert!(RtmpTransportFactory.create(&ok).is_ok());
    let cases = [
        config("rtmps://127.0.0.1/live"),
        config("srt://127.0.0.1:9000"),
        TransportConfig::new(
            "rtmp://127.0.0.1/live",
            TransportInput::Container(ContainerId::Flv),
        )
        .with_secret(Secret::new(KEY)),
        TransportConfig::new("rtmp://127.0.0.1/live", TransportInput::Packets),
        config("rtmp://127.0.0.1/live").with_adapter_option("chunk_size", "8192"),
        config("rtmp://127.0.0.1"),
        config("rtmp://127.0.0.1/live").with_queue(QueueLimits::new(0, Duration::from_secs(1))),
        config("rtmp://127.0.0.1/live").with_queue(QueueLimits::new(1024, Duration::ZERO)),
        config("rtmp://127.0.0.1/live").with_reconnect(ReconnectPolicy::Backoff {
            initial: Duration::ZERO,
            max: Duration::from_secs(1),
            attempts: None,
        }),
    ];
    for c in cases {
        let error = RtmpTransportFactory.create(&c).err().unwrap();
        assert!(
            matches!(
                error,
                CreateError::Unsupported { .. } | CreateError::InvalidConfig(_)
            ),
            "{c:?}"
        );
        assert!(!error.to_string().contains(KEY));
    }
}

#[test]
fn start_and_send_check_streams_and_packets() {
    let mut t = RtmpTransportFactory
        .create(&config("rtmp://127.0.0.1:9/live"))
        .unwrap();
    let hevc = StreamDescriptor::new(
        CodecId::Hevc,
        BitstreamFormat::AnnexB,
        Bytes::new(),
        video_stream().params,
    );
    assert!(matches!(
        t.start(&[hevc]),
        Err(TransportError::InvalidInput(_))
    ));
    t.start(&streams()).unwrap();
    let wrong_stream = t.send(TransportItem::Packet {
        stream: StreamIndex(5),
        packet: &audio(1000),
    });
    assert!(matches!(wrong_stream, Err(TransportError::InvalidInput(_))));
    let wrong_codec = t.send(TransportItem::Packet {
        stream: VIDEO,
        packet: &audio(1000),
    });
    assert!(matches!(wrong_codec, Err(TransportError::InvalidInput(_))));
    send(&mut t, VIDEO, &video(5000, true, 10));
    // More than the one-second origin lead before the first packet.
    let early = t.send(TransportItem::Packet {
        stream: AUDIO,
        packet: &audio(3000),
    });
    assert!(matches!(early, Err(TransportError::InvalidInput(_))));
    assert_eq!(send(&mut t, AUDIO, &audio(4200)), SendOutcome::Queued);
}

#[test]
fn audio_only_output_needs_no_keyframe() {
    let server = TestServer::start(Behaviour::default());
    let mut t = RtmpTransportFactory.create(&config(&server.url())).unwrap();
    t.start(&[audio_stream()]).unwrap();
    for i in 0..5 {
        send(&mut t, StreamIndex(0), &audio(1000 + i * 21));
    }
    t.finish(Duration::from_secs(5)).unwrap();
    let media = server.finish()[0]
        .media()
        .iter()
        .map(|m| m.type_id)
        .collect::<Vec<_>>();
    assert_eq!(media, [18, 8, 8, 8, 8, 8, 8]);
}

#[test]
fn after_a_stall_video_resumes_at_a_keyframe_with_intact_gops() {
    // The server stops reading right after publish; the sender keeps going.
    let pause = std::sync::Arc::new(std::sync::Barrier::new(2));
    let server = TestServer::start(Behaviour {
        pause: Some(std::sync::Arc::clone(&pause)),
        ..Behaviour::default()
    });
    let config =
        config(&server.url()).with_queue(QueueLimits::new(512 * 1024, Duration::from_secs(3)));
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    wait_for(&mut t, &mut events, |e| *e == TransportEvent::Connected);
    // 600 frames of 64 KiB (38 MiB): far more than socket buffers and the queue.
    let mut refused = 0;
    for i in 0..600 {
        let outcome = send(&mut t, VIDEO, &video(1000 + i * 33, i % 30 == 0, 64 * 1024));
        refused += u32::from(matches!(outcome, SendOutcome::Dropped { .. }));
        send(&mut t, AUDIO, &audio(1000 + i * 33));
    }
    t.poll_events(&mut events);
    assert!(refused > 0, "the queue must have overflowed");
    assert!(
        events.contains(&TransportEvent::KeyframeRequested),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TransportEvent::Dropped { .. })),
        "{events:?}"
    );
    pause.wait();
    t.finish(Duration::from_secs(20)).unwrap();
    let sessions = server.finish();
    let media = sessions[0].media();
    assert_eq!(
        media.iter().map(|m| m.type_id).take(3).collect::<Vec<_>>(),
        [18, 9, 8]
    );
    // Every delivered inter frame directly follows the previous delivered video
    // frame: no frame of its GOP was lost.
    let video: Vec<_> = media[3..].iter().filter(|m| m.type_id == 9).collect();
    // How much survives depends on the OS socket buffers (macOS keeps far less in
    // flight than Linux); what matters is that frames were dropped and the rest
    // is decodable.
    assert!(
        !video.is_empty() && video.len() < 600,
        "{} frames",
        video.len()
    );
    assert_eq!(video[0].body[0], 0x17);
    for pair in video.windows(2) {
        if pair[1].body[0] == 0x27 {
            assert_eq!(
                pair[1].timestamp - pair[0].timestamp,
                33,
                "gap before an inter frame"
            );
        }
    }
}
