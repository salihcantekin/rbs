//! `Transport::send` of the guest share (ADR-0139, P15). Run with
//! `cargo bench -p rbs-peer-str0m --bench send`.
//!
//! A real pairing over loopback: the owner thread drains the ring and writes to
//! str0m while the benchmark sends, so the numbers include the wake datagram when
//! the owner thread is waiting. Each sample times a burst of sends, then waits
//! (untimed) until the owner thread took part of the burst. The rate is far above
//! any real bitrate, so str0m may refuse some packets; that is not timed.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_peer::{AcceptSettings, InviteSettings, PeerFactory, PeerSession, SideSettings};
use rbs_peer_str0m::{WebRtcMedia, WebRtcPeerFactory, WebRtcTransport};
use rbs_transport::{Transport, TransportEvent, TransportItem};

fn wait(mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "hang guard");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn pair() -> (Box<dyn PeerSession>, Box<dyn PeerSession>, WebRtcTransport) {
    let factory = WebRtcPeerFactory;
    let side = SideSettings::with_stun_servers(Vec::new());
    let mut invite = factory
        .create_invite(&InviteSettings::new(side.clone()))
        .unwrap();
    wait(|| invite.code().is_some());
    let code = invite.code().unwrap().expose().to_owned();
    let mut host = factory
        .accept_invite(&code, &AcceptSettings::new(side))
        .unwrap();
    wait(|| host.answer_code().is_some());
    let answer = host.answer_code().unwrap().expose().to_owned();
    let mut share = invite.accept_answer(&answer).unwrap();
    let mut transport = WebRtcMedia::transport(&share.media()).unwrap();
    let video = StreamParams::video(
        VideoFormat::new(
            Size::new(1280, 720),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_30,
    );
    transport
        .start(&[
            StreamDescriptor::new(CodecId::H264, BitstreamFormat::AnnexB, Bytes::new(), video),
            StreamDescriptor::new(
                CodecId::Opus,
                BitstreamFormat::Raw,
                Bytes::new(),
                StreamParams::audio(AudioFormat::STEREO_48K, 960),
            ),
        ])
        .unwrap();
    wait(|| share.check_phrase().is_some() && host.check_phrase().is_some());
    share.confirm_phrase().unwrap();
    host.confirm_phrase().unwrap();
    let mut events = Vec::new();
    wait(|| {
        transport.poll_events(&mut events);
        events.contains(&TransportEvent::Connected)
    });
    (share, host, transport)
}

fn bench(c: &mut Criterion) {
    let (_share, _host, mut transport) = pair();
    let mut au = vec![0, 0, 0, 1, 0x41];
    au.extend(std::iter::repeat_n(7u8, 1200));
    let video = EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        Timestamp::ZERO,
        Timestamp::ZERO,
        false,
        Bytes::from(au),
    )
    .unwrap();
    let audio = EncodedPacket::new(
        CodecId::Opus,
        BitstreamFormat::Raw,
        Timestamp::ZERO,
        Timestamp::ZERO,
        false,
        Bytes::from_static(&[0xfc; 120]),
    )
    .unwrap();
    // The stream starts at a keyframe.
    let key = EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        Timestamp::ZERO,
        Timestamp::ZERO,
        true,
        Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
    )
    .unwrap();
    transport
        .send(TransportItem::Packet {
            stream: StreamIndex(0),
            packet: &key,
        })
        .unwrap();
    const BURST: u64 = 8;
    let mut group = c.benchmark_group("guest_share_send");
    for (name, packet, stream) in [("video_1200b", &video, 0), ("opus_120b", &audio, 1)] {
        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                let mut done = 0;
                while done < iters {
                    let n = BURST.min(iters - done);
                    // Untimed: a keyframe first, so a burst that str0m's queue refused
                    // (the bench sends far above any real bitrate) does not stop video.
                    transport
                        .send(TransportItem::Packet {
                            stream: StreamIndex(0),
                            packet: &key,
                        })
                        .unwrap();
                    let before = transport.stats().bytes_sent;
                    let start = Instant::now();
                    for _ in 0..n {
                        black_box(
                            transport
                                .send(TransportItem::Packet {
                                    stream: StreamIndex(stream),
                                    packet,
                                })
                                .unwrap(),
                        );
                    }
                    total += start.elapsed();
                    wait(|| transport.stats().bytes_sent > before);
                    done += n;
                }
                total
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
