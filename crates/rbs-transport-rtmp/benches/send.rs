//! Micro-benchmarks for `Transport::send` of the RTMP transport.
//! Run with `cargo bench -p rbs-transport-rtmp --bench send`.
//!
//! The destination is a local server that discards what it reads, so the I/O thread
//! keeps up and the numbers are the cost of `send` (validation, timing, staging and
//! the hand-off), not of the network. Each sample times a burst of sends of the same
//! packet, then waits (untimed) until the I/O thread has sent the burst, so the queue
//! never backs up and nothing is dropped.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_transport::{
    QueueLimits, Secret, TransportConfig, TransportEvent, TransportFactory, TransportInput,
    TransportItem,
};
use rbs_transport_rtmp::RtmpTransportFactory;
use support::{Behaviour, PPS, SPS, TestServer};

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
                    Size::new(1920, 1080),
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

/// Slice-like payload: pseudo-random bytes with emulation prevention (no 00 00 0x).
fn slice(key: bool, len: usize) -> Bytes {
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
    let mut seed = 0x1234_5678_u32;
    let mut zeros = 0;
    for _ in 0..len {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut b = (seed >> 24) as u8;
        if zeros >= 2 && b <= 3 {
            data.push(3);
            zeros = 0;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        if zeros > 2 {
            b = 1;
            zeros = 0;
        }
        data.push(b);
    }
    Bytes::from(data)
}

/// Sends timed back to back before waiting for the I/O thread: a burst like a
/// keyframe with its audio.
const BURST: u64 = 16;

fn bench_send(c: &mut Criterion) {
    let server = TestServer::start(Behaviour {
        sink: true,
        ..Behaviour::default()
    });
    let config = TransportConfig::new(server.url(), TransportInput::Packets)
        .with_secret(Secret::new("bench"))
        .with_queue(QueueLimits::new(
            256 * 1024 * 1024,
            Duration::from_secs(3600),
        ));
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams()).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events.contains(&TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    let t0 = Timestamp::from_nanos(1_000_000_000);
    let key = EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        t0,
        t0,
        true,
        slice(true, 150_000),
    )
    .unwrap();
    // The stream starts at a keyframe.
    t.send(TransportItem::Packet {
        stream: StreamIndex(0),
        packet: &key,
    })
    .unwrap();
    let cases = [
        (
            "video_inter_20k",
            StreamIndex(0),
            CodecId::H264,
            slice(false, 20_000),
        ),
        (
            "video_key_150k",
            StreamIndex(0),
            CodecId::H264,
            slice(true, 150_000),
        ),
        (
            "audio_400",
            StreamIndex(1),
            CodecId::Aac,
            Bytes::from(vec![0x21; 400]),
        ),
    ];
    let mut group = c.benchmark_group("rtmp_send");
    for (name, stream, codec, data) in cases {
        let (format, keyframe) = match codec {
            CodecId::H264 => (BitstreamFormat::AnnexB, name.contains("key")),
            _ => (BitstreamFormat::Raw, true),
        };
        let _shared = data.clone();
        group.throughput(Throughput::Bytes(data.len() as u64));
        let packet = EncodedPacket::new(codec, format, t0, t0, keyframe, data).unwrap();
        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                let mut left = iters;
                while left > 0 {
                    let burst = left.min(BURST);
                    left -= burst;
                    let started = Instant::now();
                    for _ in 0..burst {
                        black_box(
                            t.send(TransportItem::Packet {
                                stream,
                                packet: black_box(&packet),
                            })
                            .unwrap(),
                        );
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
