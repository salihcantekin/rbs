//! Adapter behaviour beyond the port suite, against the fake engine everywhere and the
//! Media Foundation encoder where present: extradata, packet counts, reuse after a
//! flush, interleaved input, discontinuities, configuration and input checks.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use rbs_codec::{
    AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError, EncoderChange,
    RateControl,
};
use rbs_core::media::{AudioBlock, AudioFormat, ChannelLayout, CodecId, EncodedPacket};
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;
use rbs_media_format::aac::AudioSpecificConfig;

const ORIGIN: Timestamp = Timestamp::from_nanos(2_000_000_000);
const STEREO: AudioFormat = AudioFormat::STEREO_48K;

fn session(factory: &dyn AudioEncoderFactory) -> Box<dyn AudioEncoder> {
    factory
        .create(&AudioEncoderConfig::new(CodecId::Aac, STEREO, 128_000))
        .unwrap()
}

fn tone(frames: usize, offset: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| ((i + offset) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3)
        .collect()
}

fn submit(s: &mut dyn AudioEncoder, ts: Timestamp, left: &[f32]) {
    s.submit(&AudioBlock::planar(STEREO, ts, &[left, left]).unwrap())
        .unwrap();
}

fn assert_monotonic(name: &str, out: &[EncodedPacket]) {
    for pair in out.windows(2) {
        assert!(
            pair[1].dts() >= pair[0].dts() + pair[0].duration().unwrap_or_default(),
            "{name}: packets overlap: {:?} then {:?}",
            pair[0].dts(),
            pair[1].dts()
        );
    }
}

#[test]
fn extradata_is_aac_lc_stereo_48k() {
    for (name, f) in common::factories() {
        let s = session(f.as_ref());
        let asc = AudioSpecificConfig::parse(&s.stream().extradata).unwrap();
        assert_eq!(asc.object_type, 2, "{name}");
        assert_eq!(asc.sample_rate(), 48_000, "{name}");
        assert_eq!(asc.channels, 2, "{name}");
        assert_eq!(
            &s.stream().extradata[..],
            &AudioSpecificConfig::lc(48_000, 2).unwrap().to_bytes()[..],
            "{name}: the encoder's own config matches the computed one"
        );
    }
}

#[test]
fn flush_emits_every_frame_and_a_flushed_session_encodes_like_a_new_one() {
    for (name, f) in common::factories() {
        let mut s = session(f.as_ref());
        let frames = 48_000; // 1 s
        let left = tone(frames, 0);
        let mut first = Vec::new();
        submit(s.as_mut(), ORIGIN, &left);
        s.flush(&mut first).unwrap();
        let frames_in = (frames as u64).div_ceil(1024);
        let delay = first.len() as u64 - frames_in.min(first.len() as u64);
        eprintln!(
            "{name}: {} packets for {frames_in} frames (delay {delay} frames)",
            first.len()
        );
        assert!(
            first.len() as u64 >= frames_in,
            "{name}: every frame comes out"
        );
        assert!(delay <= 4, "{name}: encoder delay of {delay} frames");
        assert!(first.iter().all(|p| p.keyframe() && !p.data().is_empty()));
        assert_eq!(first[0].pts(), ORIGIN, "{name}");
        assert_monotonic(name, &first);
        // A session can be reused after a flush, and starts from a clean state.
        let mut second = Vec::new();
        submit(s.as_mut(), ORIGIN, &left);
        s.flush(&mut second).unwrap();
        assert_eq!(first.len(), second.len(), "{name}");
        for (i, (a, b)) in first.iter().zip(&second).enumerate() {
            assert_eq!(a.pts(), b.pts(), "{name}: packet {i}");
            assert_eq!(a.data(), b.data(), "{name}: packet {i}");
        }
    }
}

#[test]
fn interleaved_input_matches_planar() {
    for (name, f) in common::factories() {
        let left = tone(4800, 0);
        let right = tone(4800, 100);
        let mut interleaved = Vec::with_capacity(9600);
        for (l, r) in left.iter().zip(&right) {
            interleaved.extend([*l, *r]);
        }
        let mut a = session(f.as_ref());
        let mut b = session(f.as_ref());
        let (mut pa, mut pb) = (Vec::new(), Vec::new());
        a.submit(&AudioBlock::planar(STEREO, ORIGIN, &[&left, &right]).unwrap())
            .unwrap();
        b.submit(&AudioBlock::interleaved(STEREO, ORIGIN, &interleaved).unwrap())
            .unwrap();
        a.flush(&mut pa).unwrap();
        b.flush(&mut pb).unwrap();
        assert_eq!(pa.len(), pb.len(), "{name}");
        for (x, y) in pa.iter().zip(&pb) {
            assert_eq!(x.data(), y.data(), "{name}");
        }
    }
}

#[test]
fn a_gap_restarts_the_timeline() {
    for (name, f) in common::factories() {
        let mut s = session(f.as_ref());
        let mut out = Vec::new();
        let left = tone(2048, 0);
        submit(s.as_mut(), ORIGIN, &left);
        // One second later instead of 2048 samples later.
        let later = ORIGIN + std::time::Duration::from_secs(1);
        submit(s.as_mut(), later, &left);
        s.flush(&mut out).unwrap();
        assert!(
            out.iter().any(|p| p.pts() == later),
            "{name}: a packet starts at the new timeline"
        );
        assert!(out.iter().any(|p| p.pts() < later), "{name}");
        assert_monotonic(name, &out);
    }
}

#[test]
fn small_jitter_is_continuous() {
    for (name, f) in common::factories() {
        let mut s = session(f.as_ref());
        let mut out = Vec::new();
        let left = tone(480, 0);
        for i in 0..100u64 {
            // 0.5 ms of jitter, well under one codec frame (21 ms).
            let jitter = if i % 2 == 0 { 0 } else { 500_000 };
            let ts = Timestamp::from_nanos(ORIGIN.as_nanos() + i * 10_000_000 + jitter);
            submit(s.as_mut(), ts, &left);
        }
        s.flush(&mut out).unwrap();
        for (i, p) in out.iter().enumerate() {
            let expected = ORIGIN.as_nanos() + (i as u64 * 1024 * 1_000_000_000) / 48_000;
            assert!(
                p.pts().as_nanos().abs_diff(expected) < 1_000,
                "{name}: packet {i}"
            );
        }
    }
}

#[test]
fn a_short_gap_or_an_overlap_never_moves_time_backwards() {
    let rate = STEREO.sample_rate;
    let left = tone(2048, 0);
    for (name, f) in common::factories() {
        // 1.5 frames of gap: the old tail (with the encoder delay) reaches past the
        // block. And an overlap of two frames.
        for next in [ORIGIN + rate.duration_of(2048 + 1536), ORIGIN] {
            let mut s = session(f.as_ref());
            let mut out = Vec::new();
            submit(s.as_mut(), ORIGIN, &left);
            submit(s.as_mut(), next, &left);
            // The timeline continues from the new block: no further restart.
            submit(s.as_mut(), next + rate.duration_of(2048), &left);
            s.flush(&mut out).unwrap();
            assert!(out.len() >= 6, "{name}");
            assert_monotonic(name, &out);
            let frame = rate.duration_of(1024).as_nanos() as u64;
            let jumps = out
                .windows(2)
                .filter(|w| (w[1].pts().as_nanos() - w[0].pts().as_nanos()).abs_diff(frame) > 1_000)
                .count();
            assert!(jumps <= 1, "{name}: {jumps} restarts");
        }
    }
}

#[test]
fn checks_configuration_input_and_changes() {
    let mono = AudioFormat::new(SampleRate::new(48_000).unwrap(), ChannelLayout::Mono).unwrap();
    let mono_44 = AudioFormat::new(SampleRate::new(44_100).unwrap(), ChannelLayout::Mono).unwrap();
    let stereo_32 =
        AudioFormat::new(SampleRate::new(32_000).unwrap(), ChannelLayout::Stereo).unwrap();
    for (name, f) in common::factories() {
        let mut s = session(f.as_ref());
        let left = tone(480, 0);
        let wrong = AudioBlock::planar(mono, ORIGIN, &[&left]).unwrap();
        assert!(
            matches!(s.submit(&wrong), Err(EncodeError::InvalidInput(_))),
            "{name}"
        );
        assert!(s.reconfigure(&EncoderChange::RequestKeyframe).is_ok());
        assert!(matches!(
            s.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
                bitrate_bps: 96_000
            })),
            Err(EncodeError::Unsupported)
        ));
        for bps in rbs_codec_mf::BITRATES {
            for format in [STEREO, mono, mono_44] {
                let config = AudioEncoderConfig::new(CodecId::Aac, format, bps);
                let mut s = f.create(&config).unwrap();
                let mut out = Vec::new();
                let block = vec![0.25f32; 4096];
                let planes: Vec<&[f32]> = (0..format.channels.channels())
                    .map(|_| &block[..])
                    .collect();
                s.submit(&AudioBlock::planar(format, ORIGIN, &planes).unwrap())
                    .unwrap();
                s.flush(&mut out).unwrap();
                assert!(out.len() >= 4, "{name}: {bps} bit/s {format:?}");
            }
        }
        for bps in [64_000, 1_000, 320_000] {
            let config = AudioEncoderConfig::new(CodecId::Aac, STEREO, bps);
            assert!(
                matches!(f.create(&config), Err(CreateError::InvalidConfig(_))),
                "{name}: {bps}"
            );
        }
        let config = AudioEncoderConfig::new(CodecId::Aac, stereo_32, 128_000);
        assert!(matches!(
            f.create(&config),
            Err(CreateError::Unsupported { .. })
        ));
        let config = AudioEncoderConfig::new(CodecId::Opus, STEREO, 128_000);
        assert!(matches!(
            f.create(&config),
            Err(CreateError::Unsupported { .. })
        ));
    }
}

#[test]
fn probe_declares_keyframe_changes_only() {
    use rbs_core::capability::CapabilityValue;
    for (name, f) in common::factories() {
        let caps = f.probe().unwrap();
        let Some(CapabilityValue::Set(set)) = caps.get(&rbs_codec::caps::RECONFIGURE) else {
            panic!("{name}: codec.reconfigure missing");
        };
        assert_eq!(set, &[rbs_codec::caps::RECONFIGURE_KEYFRAME.to_owned()]);
    }
}

#[cfg(windows)]
#[test]
fn media_foundation_copies_its_input() {
    let Some(f) = common::real() else {
        return;
    };
    let mut s = f
        .create_session(&AudioEncoderConfig::new(CodecId::Aac, STEREO, 128_000))
        .unwrap();
    let left = tone(48_000, 0);
    let mut out = Vec::new();
    submit(&mut s, ORIGIN, &left);
    s.flush(&mut out).unwrap();
    assert_eq!(s.input_samples_replaced(), 0);
}
