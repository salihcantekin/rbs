//! Muxes real H.264 and AAC produced by the `ffmpeg` command-line tool and checks that
//! `ffmpeg` decodes the result without errors. Ignored by default because CI has no
//! `ffmpeg`; run with `cargo test -p rbs-mux-matroska -- --ignored` where it is installed.
//! The tool is only used to make and check test data; the muxer does not use FFmpeg.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;
use std::process::Command;

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, ChunkKind, CodecId, ColorSpace, ContainerId,
    EncodedPacket, MuxChunk, PixelFormat, Size, StreamDescriptor, StreamIndex, StreamParams,
    VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate, SampleRate};
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use rbs_mux::{MuxerConfig, MuxerFactory};
use rbs_mux_matroska::MatroskaMuxerFactory;

fn ffmpeg(args: &[&str]) -> std::process::Output {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg is installed")
}

/// The byte stream without patches, as a live (non-seekable) output writes it.
fn sequential_bytes(chunks: &[MuxChunk]) -> Vec<u8> {
    chunks
        .iter()
        .filter(|c| c.is_sequential())
        .flat_map(|c| c.data.iter().copied())
        .collect()
}

/// The file a seekable output writes: the byte stream with the patches applied.
fn file_bytes(chunks: &[MuxChunk]) -> Vec<u8> {
    let mut bytes = sequential_bytes(chunks);
    for c in chunks {
        if let ChunkKind::Patch { offset } = c.kind {
            let at = usize::try_from(offset).unwrap();
            bytes[at..at + c.data.len()].copy_from_slice(&c.data);
        }
    }
    bytes
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
fn ffmpeg_decodes_annexb_input_without_errors() {
    run(false);
}

#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn ffmpeg_decodes_length_prefixed_input_without_errors() {
    run(true);
}

fn run(length_prefixed: bool) {
    let dir = std::env::temp_dir().join(format!(
        "rbs-mux-matroska-{}-{length_prefixed}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let video = dir.join("in.h264");
    let audio = dir.join("in.aac");
    let path = |p: &Path| p.to_str().unwrap().to_owned();
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
    let format = if length_prefixed {
        BitstreamFormat::LengthPrefixed { length_size: 4 }
    } else {
        BitstreamFormat::AnnexB
    };
    let video_stream = StreamDescriptor::new(
        CodecId::H264,
        format,
        Bytes::from(avcc),
        StreamParams::video(
            VideoFormat::new(
                Size::new(320, 240),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ),
            FrameRate::FPS_30,
        ),
    );
    let audio_stream = StreamDescriptor::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        Bytes::from_static(&[0x11, 0x90]),
        StreamParams::audio(AudioFormat::STEREO_48K, 1024),
    );
    let config = MuxerConfig::new(ContainerId::Matroska, vec![video_stream, audio_stream]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();

    let origin = Timestamp::from_nanos(5_000_000_000);
    let frames = Cadence::frames(origin, FrameRate::FPS_30);
    let rate = SampleRate::HZ_48000;
    let mut packets: Vec<(StreamIndex, EncodedPacket)> = Vec::new();
    for (i, (key, au)) in units.into_iter().enumerate() {
        let t = frames.time_of(i as u64);
        let au = if length_prefixed {
            let mut lp = Vec::new();
            nal::annexb_to_length_prefixed(&au, &mut lp).unwrap();
            lp
        } else {
            au
        };
        let p = EncodedPacket::new(CodecId::H264, format, t, t, key, Bytes::from(au));
        packets.push((StreamIndex(0), p.unwrap()));
    }
    let video_count = packets.len();
    for (i, frame) in adts_frames(&std::fs::read(&audio).unwrap())
        .into_iter()
        .enumerate()
    {
        let t = origin + rate.duration_of(i as u64 * 1024);
        let p = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(frame),
        );
        packets.push((StreamIndex(1), p.unwrap()));
    }
    packets.sort_by_key(|(s, p)| (p.dts(), *s));

    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for (stream, packet) in &packets {
        muxer.write(*stream, packet, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let keyframes = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Media { keyframe: true, .. }))
        .count();
    assert_eq!(keyframes, 4, "one cluster per GOP");

    // A live output (no patches applied) decodes as well.
    let live = dir.join("live.mkv");
    std::fs::write(&live, sequential_bytes(&chunks)).unwrap();
    let decoded = ffmpeg(&["-i", &path(&live), "-f", "null", "-"]);
    assert!(decoded.status.success());
    assert!(decoded.stderr.is_empty());

    let out = dir.join("out.mkv");
    std::fs::write(&out, file_bytes(&chunks)).unwrap();
    // The patched Duration: 4 s from the first frame at 1 s, plus the last AAC frame's
    // overhang past the last video frame (at most one 21 ms frame).
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(&out)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&probe.stdout);
    let duration: f64 = text.trim().parse().unwrap();
    assert!((4.999..5.05).contains(&duration), "duration {duration}");
    let decoded = ffmpeg(&["-i", &path(&out), "-f", "null", "-"]);
    assert!(decoded.status.success());
    assert!(
        decoded.stderr.is_empty(),
        "decoder errors: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=nb_read_frames,start_time",
            "-of",
            "csv=p=0",
        ])
        .arg(&out)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&probe.stdout);
    let fields: Vec<&str> = text.lines().next().unwrap_or_default().split(',').collect();
    assert_eq!(fields[1].parse::<usize>().unwrap(), video_count, "{text}");
    // Time 0 is one interleaving bound (1 s) before the first dts (Matroska has no
    // edit list), so playback starts at 1 s.
    let start: f64 = fields[0].parse().unwrap();
    assert!((start - 1.0).abs() < 1e-3, "start time {start}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Packets of the first video stream of `path` with their times, read with ffprobe.
fn probe_packets(path: &Path) -> Vec<(f64, f64, bool, Vec<u8>)> {
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_packets",
            "-show_data",
        ])
        .args(["-show_entries", "packet=pts_time,dts_time,flags,data"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8(probe.stdout).unwrap();
    let mut packets = Vec::new();
    for section in text.split("[PACKET]").skip(1) {
        let field = |name: &str| {
            section
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .unwrap()
                .to_owned()
        };
        let mut data = Vec::new();
        for line in section
            .lines()
            .filter(|l| l.len() > 10 && &l[8..10] == ": ")
        {
            let hex = &line[10..line.len().min(49)];
            for group in hex.split_whitespace() {
                for i in (0..group.len()).step_by(2) {
                    data.push(u8::from_str_radix(&group[i..i + 2], 16).unwrap());
                }
            }
        }
        packets.push((
            field("pts_time=").parse().unwrap(),
            // Matroska has no dts; N/A reads as NaN.
            field("dts_time=").parse().unwrap_or(f64::NAN),
            field("flags=").starts_with('K'),
            data,
        ));
    }
    packets
}

/// B-frames: blocks carry presentation times in decode order. Real x264 output with
/// two B-frames, read back by ffmpeg, must decode without errors and present every
/// frame once, evenly spaced.
#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn ffmpeg_decodes_b_frames_in_presentation_order() {
    let dir = std::env::temp_dir().join(format!("rbs-mux-matroska-bf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("in.ts");
    let path = |p: &Path| p.to_str().unwrap().to_owned();
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30:duration=3",
        "-c:v",
        "libx264",
        "-bf",
        "2",
        "-g",
        "30",
        "-bsf:v",
        "h264_metadata=aud=insert",
        &path(&source),
    ]);
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let packets = probe_packets(&source);
    assert!(packets.len() >= 90);
    assert!(
        packets.iter().any(|(pts, dts, ..)| pts > dts),
        "has reordering"
    );
    let mut avcc = Vec::new();
    AvcConfig::from_annexb(&packets[0].3, &mut avcc)
        .unwrap()
        .unwrap();
    let stream = StreamDescriptor::new(
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
    );
    let config = MuxerConfig::new(ContainerId::Matroska, vec![stream]);
    let mut muxer = MatroskaMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let t = |s: f64| Timestamp::from_nanos(5_000_000_000 + (s * 1e9).round() as u64);
    for (pts, dts, key, data) in &packets {
        let p = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            t(*pts),
            t(*dts),
            *key,
            Bytes::from(data.clone()),
        )
        .unwrap();
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let out = dir.join("out.mkv");
    std::fs::write(&out, file_bytes(&chunks)).unwrap();

    let decoded = ffmpeg(&["-i", &path(&out), "-f", "null", "-"]);
    assert!(decoded.status.success());
    assert!(
        decoded.stderr.is_empty(),
        "decoder errors: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let frames = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .args(["frame=pts_time", "-of", "csv=p=0"])
        .arg(&out)
        .output()
        .unwrap();
    let times: Vec<f64> = String::from_utf8_lossy(&frames.stdout)
        .lines()
        .map(|l| l.trim_end_matches(',').parse().unwrap())
        .collect();
    assert_eq!(times.len(), packets.len(), "every frame presented once");
    for pair in times.windows(2) {
        let step = pair[1] - pair[0];
        assert!((step - 1.0 / 30.0).abs() < 0.002, "frame step {step}");
    }
    // Block timestamps are the presentation times: source pts relative to the first
    // dts, plus the 1 s lead.
    let first_dts = packets[0].1;
    let muxed = probe_packets(&out);
    assert_eq!(muxed.len(), packets.len());
    for ((pts, ..), (muxed_pts, ..)) in packets.iter().zip(&muxed) {
        let expected = pts - first_dts + 1.0;
        assert!(
            (muxed_pts - expected).abs() < 0.0015,
            "pts {muxed_pts} vs {expected}"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
