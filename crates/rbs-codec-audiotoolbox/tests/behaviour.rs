//! Adapter behaviour beyond the port suite: extradata, packet count after a flush,
//! interleaved input, discontinuities and input checks.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::{AudioEncoder, AudioEncoderConfig, EncodeError, EncoderChange, RateControl};
use rbs_codec_audiotoolbox::{AacFactory, AacSession};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId, EncodedPacket};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;

const ORIGIN: Timestamp = Timestamp::from_nanos(2_000_000_000);

fn session() -> AacSession {
    AacFactory::new()
        .create_session(&AudioEncoderConfig::new(
            CodecId::Aac,
            AudioFormat::STEREO_48K,
            128_000,
        ))
        .unwrap()
}

fn tone(frames: usize, offset: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| ((i + offset) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3)
        .collect()
}

#[test]
fn extradata_is_aac_lc_stereo_48k() {
    let s = session();
    let asc = AudioSpecificConfig::parse(&s.stream().extradata).unwrap();
    assert_eq!(asc.object_type, 2);
    assert_eq!(asc.sample_rate(), 48_000);
    assert_eq!(asc.channels, 2);
    assert!(
        s.priming() > 0 && s.priming() < 4096,
        "priming {}",
        s.priming()
    );
}

#[test]
fn flush_emits_every_frame_plus_the_priming_delay() {
    let mut s = session();
    let mut out: Vec<EncodedPacket> = Vec::new();
    let frames = 48_000; // 1 s
    let left = tone(frames, 0);
    let block = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap();
    s.submit(&block).unwrap();
    s.flush(&mut out).unwrap();
    let total = frames as u64 + u64::from(s.priming());
    let expected = total.div_ceil(1024);
    assert!(
        (out.len() as u64).abs_diff(expected) <= 1,
        "{} packets, about {expected} expected",
        out.len()
    );
    assert!(out.iter().all(|p| p.keyframe() && !p.data().is_empty()));
    assert_eq!(out[0].pts(), ORIGIN);
    // A session can be reused after a flush.
    let block = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap();
    s.submit(&block).unwrap();
    out.clear();
    s.flush(&mut out).unwrap();
    assert!(!out.is_empty());
    assert_eq!(out[0].pts(), ORIGIN);
}

#[test]
fn interleaved_input_matches_planar() {
    let left = tone(4800, 0);
    let mut interleaved = Vec::with_capacity(9600);
    for v in &left {
        interleaved.extend([*v, *v]);
    }
    let mut a = session();
    let mut b = session();
    let mut pa = Vec::new();
    let mut pb = Vec::new();
    a.submit(&AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap())
        .unwrap();
    b.submit(&AudioBlock::interleaved(AudioFormat::STEREO_48K, ORIGIN, &interleaved).unwrap())
        .unwrap();
    a.flush(&mut pa).unwrap();
    b.flush(&mut pb).unwrap();
    assert_eq!(pa.len(), pb.len());
    for (x, y) in pa.iter().zip(&pb) {
        assert_eq!(x.data(), y.data());
    }
}

#[test]
fn a_gap_restarts_the_timeline() {
    let mut s = session();
    let mut out = Vec::new();
    let left = tone(2048, 0);
    s.submit(&AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap())
        .unwrap();
    // One second later instead of 2048 samples later.
    let later = ORIGIN + std::time::Duration::from_secs(1);
    s.submit(&AudioBlock::planar(AudioFormat::STEREO_48K, later, &[&left, &left]).unwrap())
        .unwrap();
    s.flush(&mut out).unwrap();
    assert!(
        out.iter().any(|p| p.pts() == later),
        "a packet starts at the new timeline"
    );
    let before: Vec<_> = out.iter().filter(|p| p.pts() < later).collect();
    assert!(!before.is_empty());
}

#[test]
fn small_jitter_is_continuous() {
    let mut s = session();
    let mut out = Vec::new();
    let left = tone(480, 0);
    for i in 0..100u64 {
        // 0.5 ms of jitter, well under one codec frame (21 ms).
        let jitter = if i % 2 == 0 { 0 } else { 500_000 };
        let ts = Timestamp::from_nanos(ORIGIN.as_nanos() + i * 10_000_000 + jitter);
        s.submit(&AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap())
            .unwrap();
    }
    s.flush(&mut out).unwrap();
    for (i, p) in out.iter().enumerate() {
        let expected = ORIGIN.as_nanos() + (i as u64 * 1024 * 1_000_000_000) / 48_000;
        assert!(p.pts().as_nanos().abs_diff(expected) < 1_000, "packet {i}");
    }
}

#[test]
fn checks_input_and_changes() {
    let mut s = session();
    let mono = AudioFormat::new(
        rbs_core::timing::SampleRate::new(48_000).unwrap(),
        rbs_core::media::ChannelLayout::Mono,
    )
    .unwrap();
    let left = tone(480, 0);
    let wrong = AudioBlock::planar(mono, ORIGIN, &[&left]).unwrap();
    assert!(matches!(
        s.submit(&wrong),
        Err(EncodeError::InvalidInput(_))
    ));
    assert!(s.reconfigure(&EncoderChange::RequestKeyframe).is_ok());
    assert!(
        s.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
            bitrate_bps: 96_000
        }))
        .is_ok()
    );
    assert!(
        AacFactory::new()
            .create_session(&AudioEncoderConfig::new(
                CodecId::Aac,
                AudioFormat::STEREO_48K,
                1_000
            ))
            .is_err()
    );
}
