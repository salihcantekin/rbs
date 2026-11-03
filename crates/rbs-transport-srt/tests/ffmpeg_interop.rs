//! Muxes real H.264 and AAC (made by the `ffmpeg` command-line tool) into MPEG-TS
//! chunks with `rbs-mux-mpegts`, sends them to `ffmpeg` acting as an SRT listener
//! (libsrt), and checks that every packet arrived intact: the per-packet checksums
//! of what `ffmpeg` received equal those of the muxer's output, and it decodes
//! without errors. A second run makes the listener lose nothing but reorder nothing
//! either; loss recovery is covered by the behaviour tests. Ignored by default
//! because CI has no `ffmpeg`; run with `cargo test -p rbs-transport-srt -- --ignored`
//! where it is installed. The tool only makes and checks test data; the adapter
//! does not use FFmpeg or libsrt.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, ContainerId, EncodedPacket, MuxChunk,
    PixelFormat, Size, StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate, SampleRate};
use rbs_media_format::nal::{self, h264};
use rbs_mux::{MuxerConfig, MuxerFactory};
use rbs_mux_mpegts::MpegTsMuxerFactory;
use rbs_transport::{
    QueueLimits, ReconnectPolicy, Secret, SendOutcome, TransportConfig, TransportEvent,
    TransportFactory, TransportInput, TransportItem,
};
use rbs_transport_srt::SrtTransportFactory;

fn ffmpeg(args: &[&str]) -> std::process::Output {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg is installed")
}

/// Splits an Annex B stream with access unit delimiters into access units.
fn access_units(stream: &[u8]) -> Vec<(bool, Vec<u8>)> {
    let mut units: Vec<(bool, Vec<u8>)> = Vec::new();
    for nal in nal::annexb_units(stream) {
        let kind = nal::h264_type(nal);
        if kind == Some(h264::AUD) || units.is_empty() {
            units.push((false, Vec::new()));
        }
        let au = units.last_mut().unwrap();
        au.0 |= kind == Some(h264::IDR);
        au.1.extend_from_slice(&[0, 0, 0, 1]);
        au.1.extend_from_slice(nal);
    }
    units
}

/// Splits an ADTS stream into raw AAC frames.
fn adts_frames(mut stream: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    while stream.len() >= 7 {
        assert_eq!(stream[0], 0xff);
        let len = (usize::from(stream[3] & 0x3) << 11)
            | (usize::from(stream[4]) << 3)
            | usize::from(stream[5] >> 5);
        let header = if stream[1] & 1 == 0 { 9 } else { 7 };
        frames.push(stream[header..len].to_vec());
        stream = &stream[len..];
    }
    frames
}

/// Six seconds of 320x240 H.264 at 30 fps (one GOP per second) and AAC, muxed.
fn muxed_chunks(dir: &Path) -> Vec<MuxChunk> {
    let path = |p: &Path| p.to_str().unwrap().to_owned();
    let video = dir.join("in.h264");
    let audio = dir.join("in.aac");
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30:duration=6",
        "-c:v",
        "libx264",
        "-bf",
        "0",
        "-g",
        "30",
        "-bsf:v",
        "h264_metadata=aud=insert",
        &path(&video),
    ]);
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=6",
        "-ac",
        "2",
        "-c:a",
        "aac",
        &path(&audio),
    ]);
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let streams = vec![
        StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::new(),
            StreamParams::video(
                VideoFormat::new(
                    Size::new(320, 240),
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
    ];
    let config = MuxerConfig::new(ContainerId::MpegTs, streams);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let origin = Timestamp::from_nanos(5_000_000_000);
    let frames = Cadence::frames(origin, FrameRate::FPS_30);
    let rate = SampleRate::HZ_48000;
    let mut packets: Vec<(StreamIndex, EncodedPacket)> = Vec::new();
    for (i, (key, au)) in access_units(&std::fs::read(&video).unwrap())
        .into_iter()
        .enumerate()
    {
        let t = frames.time_of(i as u64);
        let p = EncodedPacket::new(CodecId::H264, BitstreamFormat::AnnexB, t, t, key, au.into());
        packets.push((StreamIndex(0), p.unwrap()));
    }
    for (i, frame) in adts_frames(&std::fs::read(&audio).unwrap())
        .into_iter()
        .enumerate()
    {
        let t = origin + rate.duration_of(i as u64 * 1024);
        let p = EncodedPacket::new(CodecId::Aac, BitstreamFormat::Raw, t, t, true, frame.into());
        packets.push((StreamIndex(1), p.unwrap()));
    }
    packets.sort_by_key(|(s, p)| (p.dts(), *s));
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for (stream, packet) in &packets {
        muxer.write(*stream, packet, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks
}

/// Per-packet checksums of a TS input (`framemd5`), without the header comments.
fn checksums(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "needs the ffmpeg command-line tool built with libsrt"]
fn libsrt_receives_every_packet_intact() {
    let dir = std::env::temp_dir().join(format!("rbs-srt-interop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |p: &Path| p.to_str().unwrap().to_owned();
    let chunks = muxed_chunks(&dir);
    let local = dir.join("sent.ts");
    let bytes: Vec<u8> = chunks
        .iter()
        .filter(|c| c.is_sequential())
        .flat_map(|c| c.data.iter().copied())
        .collect();
    std::fs::write(&local, &bytes).unwrap();

    // ffmpeg as the SRT listener, writing per-packet checksums of what it receives.
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let received = dir.join("received.md5");
    let listener = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-i",
            &format!("srt://127.0.0.1:{port}?mode=listener&latency=200000"),
        ])
        .args([
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "framemd5",
            &path(&received),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // ffmpeg needs a moment to listen: the reconnect policy covers it.
    let config = TransportConfig::new(
        format!("srt://127.0.0.1:{port}"),
        TransportInput::Container(ContainerId::MpegTs),
    )
    .with_secret(Secret::new("#!::r=live/interop,m=publish"))
    // The whole stream is sent at once, so the queue must hold six seconds of it.
    .with_queue(QueueLimits::new(
        64 * 1024 * 1024,
        Duration::from_secs(3600),
    ))
    .with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        attempts: Some(50),
    });
    let mut t = SrtTransportFactory.create(&config).unwrap();
    t.start(&[]).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events.contains(&TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected: {events:?}");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    for chunk in &chunks {
        let outcome = t.send(TransportItem::Chunk(chunk));
        assert_eq!(outcome, Ok(SendOutcome::Queued));
        t.poll_events(&mut events);
    }
    t.finish(Duration::from_secs(20)).unwrap();
    assert!(t.stats().rtt.is_some(), "libsrt reports a round-trip time");
    assert_eq!(t.stats().items_dropped, 0);
    let listener = listener.wait_with_output().unwrap();
    assert!(
        listener.status.success(),
        "{}",
        String::from_utf8_lossy(&listener.stderr)
    );

    let expected = ffmpeg(&[
        "-i",
        &path(&local),
        "-map",
        "0",
        "-c",
        "copy",
        "-f",
        "framemd5",
        "-",
    ]);
    let expected = checksums(&String::from_utf8(expected.stdout).unwrap());
    let got = checksums(&std::fs::read_to_string(&received).unwrap());
    assert!(expected.len() > 180 + 280, "{} packets", expected.len());
    assert_eq!(got.len(), expected.len(), "every packet arrived");
    assert_eq!(got, expected, "every packet intact");
    let decoded = ffmpeg(&["-i", &path(&local), "-f", "null", "-"]);
    assert!(decoded.stderr.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}
