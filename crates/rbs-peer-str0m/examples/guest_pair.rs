//! Two-computer pairing check for the WebRTC guest share (ADR-0139), without the studio.
//!
//! One computer runs `share`, the other `host`. Codes travel as files, so the person
//! (or the agent running the test) copies one file to the other computer:
//!
//! ```text
//! A$ cargo run --release -p rbs-peer-str0m --example guest_pair -- share --dir pair
//!    (writes pair/invite.txt, waits for pair/answer.txt)
//! B$ cargo run --release -p rbs-peer-str0m --example guest_pair -- host --dir pair
//!    (needs pair/invite.txt from A, writes pair/answer.txt)
//! ```
//!
//! Both sides print the check phrase and confirm it themselves; compare the two printed
//! phrases by eye. The share sends synthetic H.264 access units at 30 fps (a keyframe
//! every 2 s and on request) and synthetic Opus packets at 50 per second. The host counts
//! what arrives, asks for one keyframe halfway, and prints a summary line.
//!
//! Options: `--seconds N` (send time, default 20), `--local` (no STUN: same network
//! only). The code files are secrets while they live: delete them after the run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_peer::settings::{MAX_ANSWER_LIFETIME, MAX_INVITE_LIFETIME};
use rbs_peer::{AcceptSettings, InviteSettings, PeerFactory, PeerSession, PeerState, SideSettings};
use rbs_peer_str0m::media::Track;
use rbs_peer_str0m::{WebRtcMedia, WebRtcPeerFactory};
use rbs_transport::{
    TransportConfig, TransportEvent, TransportFactory, TransportInput, TransportItem,
};

const FRAME_NS: u64 = 1_000_000_000 / 30;
const AUDIO_NS: u64 = 20_000_000;
const START_NS: u64 = 1_000_000_000;

struct Options {
    share: bool,
    dir: PathBuf,
    seconds: u64,
    local: bool,
}

fn options() -> Options {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let share = match mode.as_str() {
        "share" => true,
        "host" => false,
        _ => {
            println!("usage: guest_pair <share|host> [--dir D] [--seconds N] [--local]");
            std::process::exit(2);
        }
    };
    let mut o = Options {
        share,
        dir: PathBuf::from("pair"),
        seconds: 20,
        local: false,
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => o.dir = PathBuf::from(args.next().expect("--dir value")),
            "--seconds" => o.seconds = args.next().expect("--seconds value").parse().unwrap(),
            "--local" => o.local = true,
            other => panic!("unknown option {other}"),
        }
    }
    o
}

fn side(local: bool) -> SideSettings {
    let stun = if local {
        Vec::new()
    } else {
        vec![
            "stun.l.google.com:19302".to_owned(),
            "stun.cloudflare.com:3478".to_owned(),
        ]
    };
    SideSettings::with_stun_servers(stun)
}

fn wait(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > limit {
            println!("FAILED: timed out waiting for {what}");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read_code(path: &Path, limit: Duration) -> String {
    println!("waiting for {}", path.display());
    let mut code = String::new();
    wait(&format!("{}", path.display()), limit, || {
        code = std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_owned();
        !code.is_empty()
    });
    code
}

fn wait_connected(session: &mut dyn PeerSession) {
    let start = Instant::now();
    // The other side may take its code minutes later (the answer lives up to 10 min).
    wait("connection", MAX_ANSWER_LIFETIME, || {
        while session.poll_event().is_some() {}
        match session.state() {
            PeerState::Failed(reason) => {
                println!("FAILED: pairing failed: {reason:?}");
                std::process::exit(1);
            }
            _ => session.check_phrase().is_some(),
        }
    });
    println!(
        "connected after {} ms, check phrase: {}",
        start.elapsed().as_millis(),
        session.check_phrase().unwrap()
    );
    session.confirm_phrase().unwrap();
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

/// About 2.5 Mbit/s: 8 KB delta frames, 40 KB keyframes.
fn access_unit(i: u64, key: bool) -> EncodedPacket {
    let mut au = Vec::new();
    if key {
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
    }
    au.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }]);
    au.extend(std::iter::repeat_n(
        (i % 250) as u8 + 1,
        if key { 40_000 } else { 8_000 },
    ));
    let pts = Timestamp::from_nanos(START_NS + i * FRAME_NS);
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        pts,
        pts,
        key,
        Bytes::from(au),
    )
    .unwrap()
}

fn opus_packet(i: u64) -> EncodedPacket {
    let pts = Timestamp::from_nanos(START_NS + i * AUDIO_NS);
    EncodedPacket::new(
        CodecId::Opus,
        BitstreamFormat::Raw,
        pts,
        pts,
        true,
        Bytes::from(vec![0xfc; 120]),
    )
    .unwrap()
}

fn run_share(o: &Options) {
    let factory = WebRtcPeerFactory::new();
    let mut settings = InviteSettings::new(side(o.local));
    settings.lifetime = MAX_INVITE_LIFETIME;
    let mut invite = factory.create_invite(&settings).unwrap();
    wait("invite code", Duration::from_secs(30), || {
        invite.code().is_some()
    });
    let invite_path = o.dir.join("invite.txt");
    let answer_path = o.dir.join("answer.txt");
    let _ = std::fs::remove_file(&answer_path);
    std::fs::write(&invite_path, invite.code().unwrap().expose()).unwrap();
    println!(
        "invite written to {} ({} chars)",
        invite_path.display(),
        invite.code().unwrap().expose().len()
    );
    let answer = read_code(&answer_path, MAX_INVITE_LIFETIME);
    let mut share = invite.accept_answer(&answer).unwrap();
    let url = rbs_peer_str0m::share_url(share.session_id());
    let mut transport = factory
        .share_transports()
        .create(&TransportConfig::new(url, TransportInput::Packets))
        .unwrap();
    transport.start(&descriptors()).unwrap();
    wait_connected(share.as_mut());
    let mut events = Vec::new();
    wait("transport live", Duration::from_secs(30), || {
        transport.poll_events(&mut events);
        events.contains(&TransportEvent::Connected)
    });
    events.clear();

    let start = Instant::now();
    let end = Duration::from_secs(o.seconds);
    let (mut video, mut audio, mut requests, mut forced) = (0u64, 0u64, 0u64, 0u64);
    while start.elapsed() < end {
        transport.poll_events(&mut events);
        for e in events.drain(..) {
            match e {
                TransportEvent::KeyframeRequested => requests += 1,
                TransportEvent::Disconnected { error } => {
                    println!("FAILED: share disconnected: {error:?}");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        let now = start.elapsed().as_nanos() as u64;
        while audio * AUDIO_NS <= now {
            let p = opus_packet(audio);
            transport
                .send(TransportItem::Packet {
                    stream: StreamIndex(1),
                    packet: &p,
                })
                .unwrap();
            audio += 1;
        }
        while video * FRAME_NS <= now {
            let key = video.is_multiple_of(60) || requests > forced;
            if key && !video.is_multiple_of(60) {
                forced = requests;
            }
            let p = access_unit(video, key);
            transport
                .send(TransportItem::Packet {
                    stream: StreamIndex(0),
                    packet: &p,
                })
                .unwrap();
            video += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let stats = transport.stats();
    println!(
        "SHARE RESULT: sent video {video}, audio {audio}, keyframe requests {requests}, bytes {}, dropped {}",
        stats.bytes_sent, stats.items_dropped
    );
    // Give the last packets time to leave before closing.
    std::thread::sleep(Duration::from_secs(2));
    share.close();
    let _ = std::fs::remove_file(&invite_path);
}

fn run_host(o: &Options) {
    let factory = WebRtcPeerFactory::new();
    let invite_path = o.dir.join("invite.txt");
    let answer_path = o.dir.join("answer.txt");
    let code = read_code(&invite_path, MAX_INVITE_LIFETIME);
    let mut settings = AcceptSettings::new(side(o.local));
    settings.lifetime = MAX_ANSWER_LIFETIME;
    let mut host = factory.accept_invite(&code, &settings).unwrap();
    wait("answer code", Duration::from_secs(30), || {
        host.answer_code().is_some()
    });
    std::fs::write(&answer_path, host.answer_code().unwrap().expose()).unwrap();
    println!(
        "answer written to {} ({} chars)",
        answer_path.display(),
        host.answer_code().unwrap().expose().len()
    );
    let mut receiver = WebRtcMedia::receiver(&host.media()).unwrap();
    wait_connected(host.as_mut());

    let (mut video, mut audio, mut keys, mut gaps, mut bytes) = (0u64, 0u64, 0u64, 0u64, 0u64);
    // Arrival minus capture, relative to the first frame: the network's jitter.
    let mut first: Option<(Instant, u64)> = None;
    let mut spread: Vec<i64> = Vec::new();
    let mut first_frame: Option<Instant> = None;
    let mut asked = false;
    let mut keys_after_ask = 0u64;
    let limit = Duration::from_secs(o.seconds + 30);
    let start = Instant::now();
    while !receiver.ended() && start.elapsed() < limit {
        while let Some(f) = receiver.poll() {
            first_frame.get_or_insert(f.arrived);
            bytes += f.data.len() as u64;
            if !f.contiguous {
                gaps += 1;
            }
            match f.track {
                Track::Video => {
                    video += 1;
                    if f.keyframe {
                        keys += 1;
                        if asked {
                            keys_after_ask += 1;
                        }
                    }
                    if let Some(c) = f.capture_nanos {
                        let (a0, c0) = *first.get_or_insert((f.arrived, c));
                        let arrival = f.arrived.duration_since(a0).as_nanos() as i64;
                        spread.push(arrival - (c as i64 - c0 as i64));
                    }
                }
                Track::Audio => audio += 1,
            }
        }
        if let Some(t) = first_frame
            && !asked
            && t.elapsed() > Duration::from_secs(o.seconds / 2)
        {
            receiver.request_keyframe();
            asked = true;
            println!("keyframe requested");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    spread.sort_unstable();
    let ms = |q: f64| -> f64 {
        if spread.is_empty() {
            return 0.0;
        }
        let base = spread[0];
        let i = ((spread.len() - 1) as f64 * q) as usize;
        (spread[i] - base) as f64 / 1e6
    };
    let expected_video = o.seconds * 30;
    let expected_audio = o.seconds * 50;
    println!(
        "HOST RESULT: video {video}/{expected_video}, audio {audio}/{expected_audio}, keyframes {keys} ({keys_after_ask} after request), gaps {gaps}, overflow {}, bytes {bytes}, jitter p50 {:.1} ms p99 {:.1} ms max {:.1} ms, ended {}",
        receiver.overflow(),
        ms(0.5),
        ms(0.99),
        ms(1.0),
        receiver.ended()
    );
    host.close();
    let _ = std::fs::remove_file(&answer_path);
}

fn main() {
    let o = options();
    std::fs::create_dir_all(&o.dir).unwrap();
    if o.share {
        run_share(&o);
    } else {
        run_host(&o);
    }
}
