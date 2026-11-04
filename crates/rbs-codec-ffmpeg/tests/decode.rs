//! The real encoder's output decodes: a 440 Hz tone, encoded and written as ADTS, is
//! decoded by the `ffmpeg` command-line tool back to a 440 Hz tone at the input level.
//! Ignored by default (it runs an external tool, ADR-0032): `cargo test -p
//! rbs-codec-ffmpeg --test decode -- --ignored`. Skipped without the encoder or without `ffmpeg` on `PATH` (unless
//! `RBS_REQUIRE_FFMPEG_AAC=1`, which requires both).

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::process::Command;

use rbs_codec::{AudioEncoder, AudioEncoderConfig};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;

#[test]
#[ignore = "slow: runs the ffmpeg tool (about 2 s)"]
fn slow_a_tone_decodes_back_to_the_same_tone() {
    let Some(factory) = common::real() else {
        return;
    };
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        assert!(
            !common::required(),
            "RBS_REQUIRE_FFMPEG_AAC=1 needs ffmpeg on PATH"
        );
        eprintln!("no ffmpeg tool: decode check skipped");
        return;
    }
    let format = AudioFormat::STEREO_48K;
    let mut s = factory
        .create_session(&AudioEncoderConfig::new(CodecId::Aac, format, 160_000))
        .unwrap();
    let asc = AudioSpecificConfig::parse(&s.stream().extradata).unwrap();
    let tone: Vec<f32> = (0..48_000 * 2)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
        .collect();
    let mut packets = Vec::new();
    for (i, block) in tone.chunks(480).enumerate() {
        let ts = Timestamp::from_nanos(1_000_000_000 + i as u64 * 10_000_000);
        s.submit(&AudioBlock::planar(format, ts, &[block, block]).unwrap())
            .unwrap();
    }
    s.flush(&mut packets).unwrap();
    let mut adts = Vec::new();
    for p in &packets {
        adts.extend(asc.adts_header(p.data().len()).unwrap());
        adts.extend_from_slice(p.data());
    }
    let dir = std::env::temp_dir().join(format!("rbs-ffmpeg-aac-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tone.aac");
    std::fs::write(&path, &adts).unwrap();
    let decoded = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "f32le", "-"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let samples: Vec<f32> = decoded
        .stdout
        .chunks_exact(4)
        .step_by(2) // the left channel of interleaved stereo
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    // Two seconds plus at most the encoder delay and the padded last frame.
    assert!(
        samples.len() >= 96_000 && samples.len() <= 96_000 + 3 * 1024,
        "{}",
        samples.len()
    );
    // Skip the start (priming) and the end; measure level and zero crossings.
    let body = &samples[4_800..samples.len() - 4_800];
    let rms = (body.iter().map(|v| v * v).sum::<f32>() / body.len() as f32).sqrt();
    let crossings = body
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    let hz = crossings as f32 / 2.0 / (body.len() as f32 / 48_000.0);
    eprintln!(
        "decoded {} samples, rms {rms:.4}, {hz:.1} Hz",
        samples.len()
    );
    assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
    assert!((hz - 440.0).abs() < 2.0, "{hz} Hz");
}
