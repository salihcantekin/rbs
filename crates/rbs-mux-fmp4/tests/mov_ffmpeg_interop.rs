//! Muxes real H.264 and PCM into MOV and checks with the `ffmpeg` command-line tool
//! that the file is QuickTime, decodes without errors and gives back the PCM bytes
//! unchanged. Ignored by default because CI has no `ffmpeg`; run with
//! `cargo test -p rbs-mux-fmp4 --test mov_ffmpeg_interop -- --ignored` where it is
//! installed. The tool only makes and checks test data; the muxer does not use FFmpeg.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::{Path, PathBuf};
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
use rbs_mux_fmp4::Fmp4MuxerFactory;

/// PCM packet length in frames (10 ms at 48 kHz, as the audio graph delivers).
const BLOCK: usize = 480;

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg is installed");
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn probe(file: &Path, entries: &str, stream: Option<&str>) -> String {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error"]);
    if let Some(s) = stream {
        cmd.args(["-select_streams", s]);
    }
    let out = cmd
        .args(["-show_entries", entries, "-of", "default=nw=1:nk=1"])
        .arg(file)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn path(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-mux-mov-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Annex B H.264 with access unit delimiters, split into access units.
fn h264_units(dir: &Path) -> Vec<(bool, Vec<u8>)> {
    let file = dir.join("in.h264");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30:duration=3",
        "-c:v",
        "libx264",
        "-bf",
        "0",
        "-g",
        "30",
        "-bsf:v",
        "h264_metadata=aud=insert",
        &path(&file),
    ]);
    let mut units: Vec<(bool, Vec<u8>)> = Vec::new();
    for unit in nal::annexb_units(&std::fs::read(&file).unwrap()) {
        let kind = nal::h264_type(unit);
        if kind == Some(h264::AUD) || units.is_empty() {
            units.push((false, Vec::new()));
        }
        let au = units.last_mut().unwrap();
        au.0 |= kind == Some(h264::IDR);
        au.1.extend_from_slice(&[0, 0, 0, 1]);
        au.1.extend_from_slice(unit);
    }
    units
}

fn file_bytes(chunks: &[MuxChunk]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for chunk in chunks.iter().filter(|c| c.is_sequential()) {
        bytes.extend_from_slice(&chunk.data);
    }
    for chunk in chunks {
        if let ChunkKind::Patch { offset } = chunk.kind {
            let at = usize::try_from(offset).unwrap();
            bytes[at..at + chunk.data.len()].copy_from_slice(&chunk.data);
        }
    }
    bytes
}

/// `bits`: 16, 24 or 32 (float). `sample_fmt` is FFmpeg's raw format of that width.
fn run(bits: u8, sample_fmt: &str, codec_name: &str, with_video: bool) {
    let dir = scratch(&format!("{bits}-{with_video}"));
    let raw = dir.join("in.pcm");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=3",
        "-ac",
        "2",
        "-f",
        sample_fmt,
        &path(&raw),
    ]);
    let pcm = std::fs::read(&raw).unwrap();
    let frame = 2 * usize::from(bits / 8);

    let mut streams = Vec::new();
    let origin = Timestamp::from_nanos(5_000_000_000);
    let mut packets: Vec<(StreamIndex, EncodedPacket)> = Vec::new();
    if with_video {
        let units = h264_units(&dir);
        let mut avcc = Vec::new();
        AvcConfig::from_annexb(&units[0].1, &mut avcc)
            .unwrap()
            .unwrap();
        streams.push(StreamDescriptor::new(
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
        ));
        let frames = Cadence::frames(origin, FrameRate::FPS_30);
        for (i, (key, au)) in units.into_iter().enumerate() {
            let t = frames.time_of(i as u64);
            let p = EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                t,
                t,
                key,
                Bytes::from(au),
            );
            packets.push((StreamIndex(0), p.unwrap()));
        }
    }
    let audio_index = StreamIndex(streams.len() as u16);
    streams.push(StreamDescriptor::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        Bytes::copy_from_slice(&[bits]),
        StreamParams::audio(AudioFormat::STEREO_48K, BLOCK as u32),
    ));
    let rate = SampleRate::HZ_48000;
    for (i, block) in pcm.chunks(BLOCK * frame).enumerate() {
        let t = origin + rate.duration_of((i * BLOCK) as u64);
        let p = EncodedPacket::new(
            CodecId::Pcm,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::copy_from_slice(block),
        );
        packets.push((audio_index, p.unwrap()));
    }
    packets.sort_by_key(|(s, p)| (p.dts(), *s));

    let config = MuxerConfig::new(ContainerId::Mov, streams);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for (stream, packet) in &packets {
        muxer.write(*stream, packet, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    let out = dir.join("out.mov");
    std::fs::write(&out, file_bytes(&chunks)).unwrap();

    assert_eq!(probe(&out, "format_tags=major_brand", None), "qt");
    assert!(probe(&out, "format=format_name", None).contains("mov"));
    assert_eq!(probe(&out, "stream=codec_name", Some("a:0")), codec_name);
    assert_eq!(probe(&out, "stream=sample_rate", Some("a:0")), "48000");
    assert_eq!(probe(&out, "stream=channels", Some("a:0")), "2");
    if with_video {
        assert_eq!(probe(&out, "stream=codec_name", Some("v:0")), "h264");
    }
    // Duration from the patched movie header: three seconds.
    let duration: f64 = probe(&out, "format=duration", None).parse().unwrap();
    assert!((duration - 3.0).abs() < 0.05, "duration {duration}");

    // Decodes without errors.
    ffmpeg(&["-i", &path(&out), "-f", "null", "-"]);
    // The PCM comes back byte for byte.
    let back = dir.join("back.pcm");
    ffmpeg(&[
        "-i",
        &path(&out),
        "-map",
        "0:a",
        "-c:a",
        "copy",
        "-f",
        sample_fmt,
        &path(&back),
    ]);
    assert!(std::fs::read(&back).unwrap() == pcm, "PCM changed");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn mov_with_h264_and_pcm_16_decodes_and_keeps_the_samples() {
    run(16, "s16le", "pcm_s16le", true);
}

#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn mov_with_h264_and_pcm_24_decodes_and_keeps_the_samples() {
    run(24, "s24le", "pcm_s24le", true);
}

#[test]
#[ignore = "needs the ffmpeg command-line tool"]
fn mov_with_pcm_float_only_decodes_and_keeps_the_samples() {
    run(32, "f32le", "pcm_f32le", false);
}
