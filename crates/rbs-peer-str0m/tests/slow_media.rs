//! A share and a guest on this machine over real sockets: packets written to the
//! share's transport come out of the guest's receiver (ADR-0139 decisions 8 and 11).
//! Slow (real time, sockets, threads), so ignored by default (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_peer::{AcceptSettings, InviteSettings, PeerFactory, SideSettings};
use rbs_peer_str0m::media::Track;
use rbs_peer_str0m::{WebRtcMedia, WebRtcPeerFactory};
use rbs_transport::{
    TransportConfig, TransportEvent, TransportFactory, TransportInput, TransportItem,
};

const HANG_GUARD: Duration = Duration::from_secs(30);

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < HANG_GUARD, "hang guard: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn descriptors() -> Vec<StreamDescriptor> {
    let video = StreamParams::video(
        VideoFormat::new(
            Size::new(1280, 720),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_30,
    );
    vec![
        StreamDescriptor::new(CodecId::H264, BitstreamFormat::AnnexB, Bytes::new(), video),
        StreamDescriptor::new(
            CodecId::Opus,
            BitstreamFormat::Raw,
            Bytes::new(),
            StreamParams::audio(AudioFormat::STEREO_48K, 960),
        ),
    ]
}

fn access_unit(i: u64) -> EncodedPacket {
    let mut au = Vec::new();
    if i == 0 {
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
    }
    au.extend_from_slice(&[0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }]);
    au.extend(std::iter::repeat_n(i as u8 + 1, 4000));
    let pts = Timestamp::from_nanos(1_000_000_000 + i * 33_333_333);
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        pts,
        pts,
        i == 0,
        Bytes::from(au),
    )
    .unwrap()
}

#[test]
#[ignore = "slow: real sockets and threads"]
fn slow_share_reaches_the_guest_source() {
    let factory = WebRtcPeerFactory::new();
    let side = SideSettings::with_stun_servers(Vec::new());
    let mut invite = factory
        .create_invite(&InviteSettings::new(side.clone()))
        .unwrap();
    wait("invite", || invite.code().is_some());
    let code = invite.code().unwrap().expose().to_owned();
    let mut host = factory
        .accept_invite(&code, &AcceptSettings::new(side))
        .unwrap();
    wait("answer", || host.answer_code().is_some());
    let answer = host.answer_code().unwrap().expose().to_owned();
    let mut share = invite.accept_answer(&answer).unwrap();

    // Opened as the studio opens a guest share output: by its URL.
    let url = rbs_peer_str0m::share_url(share.session_id());
    let config = TransportConfig::new(url, TransportInput::Packets);
    let mut transport = factory
        .share_transports()
        .create(&config)
        .expect("transport");
    let mut receiver = WebRtcMedia::receiver(&host.media()).expect("receiver");
    transport.start(&descriptors()).unwrap();
    wait("connected", || {
        share.check_phrase().is_some() && host.check_phrase().is_some()
    });
    share.confirm_phrase().unwrap();
    host.confirm_phrase().unwrap();
    let mut events = Vec::new();
    wait("transport live", || {
        transport.poll_events(&mut events);
        events.contains(&TransportEvent::Connected)
    });

    for i in 0..5 {
        let packet = access_unit(i);
        transport
            .send(TransportItem::Packet {
                stream: StreamIndex(0),
                packet: &packet,
            })
            .unwrap();
    }
    let mut got = Vec::new();
    wait("five frames", || {
        while let Some(f) = receiver.poll() {
            got.push(f);
        }
        got.iter().filter(|f| f.track == Track::Video).count() == 5
    });
    assert!(got[0].keyframe);
    assert!(got[4].data.ends_with(&[5; 32]));
    let capture = got[4].capture_nanos.expect("capture time");
    assert!(capture.abs_diff(1_000_000_000 + 4 * 33_333_333) < 1_000);
    assert!(transport.stats().bytes_sent > 20_000);

    receiver.request_keyframe();
    wait("keyframe request", || {
        transport.poll_events(&mut events);
        events.contains(&TransportEvent::KeyframeRequested)
    });

    // The peer learns of a close by consent loss (30 s, covered by the simulated
    // tests); its own close ends its receiver at once.
    share.close();
    host.close();
    wait("receiver ends with its session", || receiver.ended());
}
