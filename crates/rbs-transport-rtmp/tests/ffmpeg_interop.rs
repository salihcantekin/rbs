//! Publishes real H.264 and AAC (made by the `ffmpeg` command-line tool) to `ffmpeg`
//! acting as an RTMP server, then checks that `ffmpeg` decodes what it received
//! without errors and with every frame. Ignored by default because CI has no
//! `ffmpeg`; run with `cargo test -p rbs-transport-rtmp -- --ignored` where it is
//! installed. The tool only makes and checks test data; the adapter does not use
//! FFmpeg.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size,
    StreamDescriptor, StreamIndex, StreamParams, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate, SampleRate};
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use rbs_transport::{
    ReconnectPolicy, Secret, SendOutcome, TransportConfig, TransportFactory, TransportInput,
    TransportItem,
};
use rbs_transport_rtmp::RtmpTransportFactory;

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

#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn ffmpeg_receives_and_decodes_the_stream() {
    let dir = std::env::temp_dir().join(format!("rbs-rtmp-interop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |p: &Path| p.to_str().unwrap().to_owned();
    let video = dir.join("in.h264");
    let audio = dir.join("in.aac");
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30:duration=4",
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
        "sine=frequency=440:sample_rate=48000:duration=4",
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

    let units = access_units(&std::fs::read(&video).unwrap());
    let mut avcc = Vec::new();
    AvcConfig::from_annexb(&units[0].1, &mut avcc)
        .unwrap()
        .unwrap();
    let streams = [
        StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::from(avcc),
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
    let origin = Timestamp::from_nanos(5_000_000_000);
    let frames = Cadence::frames(origin, FrameRate::FPS_30);
    let rate = SampleRate::HZ_48000;
    let mut packets: Vec<(StreamIndex, EncodedPacket)> = Vec::new();
    let video_count = units.len();
    for (i, (key, au)) in units.into_iter().enumerate() {
        let t = frames.time_of(i as u64);
        let p = EncodedPacket::new(CodecId::H264, BitstreamFormat::AnnexB, t, t, key, au.into());
        packets.push((StreamIndex(0), p.unwrap()));
    }
    let audio_frames = adts_frames(&std::fs::read(&audio).unwrap());
    let audio_count = audio_frames.len();
    for (i, frame) in audio_frames.into_iter().enumerate() {
        let t = origin + rate.duration_of(i as u64 * 1024);
        let p = EncodedPacket::new(CodecId::Aac, BitstreamFormat::Raw, t, t, true, frame.into());
        packets.push((StreamIndex(1), p.unwrap()));
    }
    // Encoders deliver audio and video interleaved, roughly in decode order.
    packets.sort_by_key(|(s, p)| (p.dts(), *s));

    // ffmpeg as the RTMP server, writing what it receives to a file.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let key = "interop-key";
    let out = dir.join("out.flv");
    let server = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-listen",
            "1",
            "-f",
            "flv",
        ])
        .args(["-i", &format!("rtmp://127.0.0.1:{port}/live/{key}")])
        .args(["-c", "copy", &path(&out)])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // ffmpeg needs a moment to listen: the reconnect policy covers it.
    let config = TransportConfig::new(
        format!("rtmp://127.0.0.1:{port}/live"),
        TransportInput::Packets,
    )
    .with_secret(Secret::new(key))
    .with_reconnect(ReconnectPolicy::Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        attempts: Some(50),
    });
    let mut t = RtmpTransportFactory.create(&config).unwrap();
    t.start(&streams).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events.contains(&rbs_transport::TransportEvent::Connected) {
        assert!(Instant::now() < deadline, "not connected: {events:?}");
        t.poll_events(&mut events);
        std::thread::yield_now();
    }
    for (stream, packet) in &packets {
        let outcome = t.send(TransportItem::Packet {
            stream: *stream,
            packet,
        });
        assert_eq!(outcome, Ok(SendOutcome::Queued));
    }
    t.finish(Duration::from_secs(20)).unwrap();
    let server = server.wait_with_output().unwrap();
    assert!(
        server.status.success(),
        "{}",
        String::from_utf8_lossy(&server.stderr)
    );

    let decoded = ffmpeg(&["-i", &path(&out), "-f", "null", "-"]);
    assert!(decoded.status.success());
    assert!(
        decoded.stderr.is_empty(),
        "decode errors: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let count = |stream: &str| {
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", stream, "-count_packets"])
            .args(["-show_entries", "stream=nb_read_packets,codec_name"])
            .args(["-of", "csv=p=0", &path(&out)])
            .output()
            .unwrap();
        String::from_utf8(probe.stdout).unwrap().trim().to_owned()
    };
    assert_eq!(count("v:0"), format!("h264,{video_count}"));
    assert_eq!(count("a:0"), format!("aac,{audio_count}"));
    std::fs::remove_dir_all(&dir).unwrap();
}
