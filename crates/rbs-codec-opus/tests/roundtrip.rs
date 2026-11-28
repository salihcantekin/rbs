//! Behaviour of the Opus encoder and decoder sessions, without the RFC vectors.

#![allow(clippy::unwrap_used)]

use rbs_codec::{
    AudioDecoder, AudioDecoderConfig, AudioDecoderFactory, AudioEncoder, AudioEncoderConfig,
    AudioEncoderFactory, CreateError, DecodeError, EncodeError, EncoderChange, RateControl,
};
use rbs_codec_opus::{MAX_PACKET_FRAMES, OpusDecoderFactory, OpusEncoderFactory};
use rbs_core::media::{AudioBlock, AudioFormat, Bytes, ChannelLayout, CodecId, EncodedPacket};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

const ORIGIN: Timestamp = Timestamp::from_nanos(5_000_000_000);

fn encoder(options: &[(&str, &str)]) -> Box<dyn AudioEncoder> {
    let mut config = AudioEncoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K, 128_000);
    for (k, v) in options {
        config = config.with_adapter_option(*k, *v);
    }
    OpusEncoderFactory.create(&config).unwrap()
}

fn decoder() -> Box<dyn AudioDecoder> {
    OpusDecoderFactory
        .create(&AudioDecoderConfig::new(
            CodecId::Opus,
            AudioFormat::STEREO_48K,
        ))
        .unwrap()
}

fn tone(start: usize, frames: usize, hz: f32) -> Vec<f32> {
    (start..start + frames)
        .map(|n| (n as f32 * hz * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
        .collect()
}

fn encode(enc: &mut dyn AudioEncoder, seconds_frames: usize, at: Timestamp) -> Vec<EncodedPacket> {
    let mut packets = Vec::new();
    for block in 0..seconds_frames / 480 {
        let left = tone(block * 480, 480, 440.0);
        let right = tone(block * 480, 480, 660.0);
        let ts = at + SampleRate::HZ_48000.duration_of((block * 480) as u64);
        let audio = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &right]).unwrap();
        enc.submit(&audio).unwrap();
        enc.drain(&mut packets).unwrap();
    }
    enc.flush(&mut packets).unwrap();
    packets
}

fn pre_skip(enc: &dyn AudioEncoder) -> usize {
    let head = &enc.stream().extradata;
    usize::from(u16::from_le_bytes([head[10], head[11]]))
}

#[test]
fn a_second_of_stereo_round_trips_with_the_pre_skip_delay() {
    let mut enc = encoder(&[]);
    let skip = pre_skip(enc.as_ref());
    assert!((100..=1000).contains(&skip), "pre-skip {skip}");
    let packets = encode(enc.as_mut(), 48_000, ORIGIN);
    assert_eq!(packets.len(), 50);
    let mut dec = decoder();
    let mut out = Vec::with_capacity(dec.max_frames() * 2);
    let mut decoded = Vec::new();
    for p in &packets {
        assert_eq!(dec.decode(p, &mut out).unwrap(), 960);
        decoded.extend_from_slice(&out);
    }
    // After the pre-skip the decoded left channel is the input again.
    let want = tone(0, 48_000 - 2_000, 440.0);
    let got: Vec<f32> = decoded.chunks(2).skip(skip).map(|f| f[0]).collect();
    let (mut signal, mut noise) = (0.0_f64, 0.0_f64);
    for (w, g) in want.iter().zip(&got).skip(4_000) {
        signal += f64::from(*w * *w);
        noise += f64::from((w - g) * (w - g));
    }
    let snr = 10.0 * (signal / noise).log10();
    assert!(snr > 25.0, "round-trip SNR {snr:.1} dB");
}

#[test]
fn packets_carry_time_duration_and_a_valid_head() {
    let mut enc = encoder(&[("frame_ms", "10")]);
    assert_eq!(enc.frame_size(), 480);
    let head = enc.stream().extradata.clone();
    assert_eq!(&head[..8], b"OpusHead");
    assert_eq!((head[8], head[9]), (1, 2));
    assert_eq!(u32::from_le_bytes(head[12..16].try_into().unwrap()), 48_000);
    let packets = encode(enc.as_mut(), 4_800, ORIGIN);
    assert_eq!(packets.len(), 10);
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(
            p.pts(),
            ORIGIN + SampleRate::HZ_48000.duration_of(i as u64 * 480)
        );
        assert_eq!(p.duration(), Some(std::time::Duration::from_millis(10)));
        assert!(p.keyframe());
    }
}

#[test]
fn a_partial_frame_is_padded_on_flush_and_a_second_flush_is_empty() {
    let mut enc = encoder(&[]);
    let left = tone(0, 500, 440.0);
    let right = left.clone();
    let audio = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &right]).unwrap();
    enc.submit(&audio).unwrap();
    let mut out = Vec::new();
    enc.drain(&mut out).unwrap();
    assert_eq!(out.len(), 0);
    enc.flush(&mut out).unwrap();
    assert_eq!(out.len(), 1);
    enc.flush(&mut out).unwrap();
    assert_eq!(out.len(), 1);
}

#[test]
fn a_gap_restarts_the_timeline_and_pads_the_partial_frame() {
    let mut enc = encoder(&[]);
    let left = tone(0, 1_200, 440.0);
    let first = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap();
    enc.submit(&first).unwrap();
    // 1200 frames: one packet encoded, 240 buffered. The next block is 1 s later.
    let later = ORIGIN + std::time::Duration::from_secs(1);
    let second = AudioBlock::planar(AudioFormat::STEREO_48K, later, &[&left, &left]).unwrap();
    enc.submit(&second).unwrap();
    let mut out = Vec::new();
    enc.flush(&mut out).unwrap();
    let times: Vec<_> = out.iter().map(|p| p.pts()).collect();
    assert_eq!(times[0], ORIGIN);
    assert_eq!(times[1], ORIGIN + SampleRate::HZ_48000.duration_of(960));
    assert_eq!(times[2], later);
    assert_eq!(out.len(), 4);
}

#[test]
fn wrong_input_and_bad_configs_are_typed_errors() {
    let mut enc = encoder(&[]);
    let mono = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Mono).unwrap();
    let samples = [0.0_f32; 480];
    let block = AudioBlock::planar(mono, ORIGIN, &[&samples]).unwrap();
    assert!(matches!(
        enc.submit(&block),
        Err(EncodeError::InvalidInput(_))
    ));

    let surround = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Surround5_1).unwrap();
    for config in [
        AudioEncoderConfig::new(CodecId::Opus, surround, 128_000),
        AudioEncoderConfig::new(CodecId::Aac, AudioFormat::STEREO_48K, 128_000),
        AudioEncoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K, 128_000)
            .with_adapter_option("frame_ms", "7"),
    ] {
        assert!(matches!(
            OpusEncoderFactory.create(&config),
            Err(CreateError::Unsupported { .. })
        ));
    }
    assert!(matches!(
        OpusEncoderFactory.create(&AudioEncoderConfig::new(
            CodecId::Opus,
            AudioFormat::STEREO_48K,
            0
        )),
        Err(CreateError::InvalidConfig(_))
    ));
}

#[test]
fn a_bitrate_change_takes_effect_and_other_changes_are_unsupported() {
    let mut low = encoder(&[("vbr", "off")]);
    let mut high = encoder(&[("vbr", "off")]);
    low.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
        bitrate_bps: 24_000,
    }))
    .unwrap();
    high.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
        bitrate_bps: 256_000,
    }))
    .unwrap();
    let a: usize = encode(low.as_mut(), 9_600, ORIGIN)
        .iter()
        .map(|p| p.data().len())
        .sum();
    let b: usize = encode(high.as_mut(), 9_600, ORIGIN)
        .iter()
        .map(|p| p.data().len())
        .sum();
    assert!(b > a * 5, "{a} bytes at 24 kbit/s, {b} at 256 kbit/s");
    assert!(matches!(
        high.reconfigure(&EncoderChange::RequestKeyframe),
        Err(EncodeError::Unsupported)
    ));
    assert!(matches!(
        high.reconfigure(&EncoderChange::Bitrate(RateControl::ConstantQuality {
            level: 20
        })),
        Err(EncodeError::Unsupported)
    ));
    assert!(matches!(
        high.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr { bitrate_bps: 1 })),
        Err(EncodeError::InvalidInput(_))
    ));
}

#[test]
fn non_finite_input_becomes_silence_and_never_fails() {
    let mut enc = encoder(&[]);
    let mut left = tone(0, 960, 440.0);
    left[3] = f32::NAN;
    left[4] = f32::INFINITY;
    let audio = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap();
    enc.submit(&audio).unwrap();
    let mut out = Vec::new();
    enc.drain(&mut out).unwrap();
    assert_eq!(out.len(), 1);
}

#[test]
fn the_decoder_refuses_bad_packets_and_survives_them() {
    let mut dec = decoder();
    let mut out = Vec::new();
    let wrong = EncodedPacket::new(
        CodecId::Aac,
        rbs_core::media::BitstreamFormat::Raw,
        ORIGIN,
        ORIGIN,
        true,
        Bytes::from_static(&[1, 2, 3]),
    )
    .unwrap();
    assert!(matches!(
        dec.decode(&wrong, &mut out),
        Err(DecodeError::InvalidInput(_))
    ));
    let empty = EncodedPacket::new(
        CodecId::Opus,
        rbs_core::media::BitstreamFormat::Raw,
        ORIGIN,
        ORIGIN,
        true,
        Bytes::new(),
    )
    .unwrap();
    assert!(matches!(
        dec.decode(&empty, &mut out),
        Err(DecodeError::InvalidInput(_))
    ));
    // Code 3 packet announcing 63 frames of 60 ms: longer than 120 ms in total.
    let long = EncodedPacket::new(
        CodecId::Opus,
        rbs_core::media::BitstreamFormat::Raw,
        ORIGIN,
        ORIGIN,
        true,
        Bytes::from_static(&[0x1B, 63]),
    )
    .unwrap();
    assert!(matches!(
        dec.decode(&long, &mut out),
        Err(DecodeError::Corrupt(_))
    ));
    assert!(out.is_empty());
    // The session still decodes a real packet afterwards.
    let packets = encode(encoder(&[]).as_mut(), 1_920, ORIGIN);
    assert_eq!(dec.decode(&packets[0], &mut out).unwrap(), 960);
    assert_eq!(out.len(), 1920);
}

#[test]
fn concealment_continues_the_sound_and_validates_its_length() {
    let mut enc = encoder(&[]);
    let packets = encode(enc.as_mut(), 9_600, ORIGIN);
    let mut dec = decoder();
    let mut out = Vec::with_capacity(dec.max_frames() * 2);
    for p in &packets[..5] {
        dec.decode(p, &mut out).unwrap();
    }
    assert_eq!(dec.conceal(960, &mut out).unwrap(), 960);
    assert_eq!(out.len(), 1920);
    let energy: f32 = out.iter().map(|s| s * s).sum::<f32>() / 1920.0;
    assert!(
        energy > 0.001 && out.iter().all(|s| s.is_finite()),
        "energy {energy}"
    );
    for bad in [0, 100, MAX_PACKET_FRAMES + 120] {
        assert!(matches!(
            dec.conceal(bad, &mut out),
            Err(DecodeError::InvalidInput(_))
        ));
    }
    // A reset decoder starts from silence like a new one.
    dec.reset();
    let first = dec.decode(&packets[0], &mut out).unwrap();
    assert_eq!(first, 960);
}

#[test]
fn the_adapters_probe_what_they_do() {
    let enc = OpusEncoderFactory.probe().unwrap();
    let dec = OpusDecoderFactory.probe().unwrap();
    assert!(format!("{enc:?}").contains("opus") && format!("{dec:?}").contains("opus"));
    assert!(rbs_codec_opus::libopus_version().contains("1.5.2"));
}

#[test]
fn a_backward_jump_restarts_once_and_later_blocks_continue() {
    let mut enc = encoder(&[]);
    let mut out = Vec::new();
    let late = ORIGIN + std::time::Duration::from_secs(10);
    for (n, base) in [(0, late), (1, late)] {
        let _ = n;
        for block in 0..10 {
            let left = tone(block * 480, 480, 440.0);
            let ts = base + SampleRate::HZ_48000.duration_of((block * 480) as u64);
            let audio = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &left]).unwrap();
            enc.submit(&audio).unwrap();
        }
    }
    // The second run repeats the same times: a one-off overlap, then steady blocks.
    enc.flush(&mut out).unwrap();
    // 10 blocks = 4800 frames = 5 packets; the repeat adds 5 more, no chopping:
    // exactly 10 packets, each exactly one frame after the previous.
    assert_eq!(out.len(), 10);
    for (i, p) in out.iter().enumerate() {
        assert_eq!(
            p.pts(),
            late + SampleRate::HZ_48000.duration_of(i as u64 * 960)
        );
    }
}

#[test]
fn a_block_after_flush_starts_a_new_timeline_at_its_own_time() {
    let mut enc = encoder(&[]);
    let mut out = Vec::new();
    let left = tone(0, 960, 440.0);
    let a = AudioBlock::planar(AudioFormat::STEREO_48K, ORIGIN, &[&left, &left]).unwrap();
    enc.submit(&a).unwrap();
    enc.flush(&mut out).unwrap();
    let again = ORIGIN + std::time::Duration::from_millis(5);
    let b = AudioBlock::planar(AudioFormat::STEREO_48K, again, &[&left, &left]).unwrap();
    enc.submit(&b).unwrap();
    enc.flush(&mut out).unwrap();
    assert_eq!(out[1].pts(), again);
}

#[test]
fn a_cbr_change_switches_off_vbr_and_extradata_is_checked() {
    let mut enc = encoder(&[]);
    enc.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
        bitrate_bps: 64_000,
    }))
    .unwrap();
    let head = encoder(&[]).stream().extradata.clone();
    let ok = AudioDecoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K)
        .with_extradata(head.clone());
    assert!(OpusDecoderFactory.create(&ok).is_ok());
    let mut bad = head.to_vec();
    bad[18] = 1; // mapping family 1
    let cfg = AudioDecoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K)
        .with_extradata(Bytes::from(bad));
    assert!(matches!(
        OpusDecoderFactory.create(&cfg),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut mono = head.to_vec();
    mono[9] = 1; // one channel in the head, two in the session
    let cfg = AudioDecoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K)
        .with_extradata(Bytes::from(mono));
    assert!(matches!(
        OpusDecoderFactory.create(&cfg),
        Err(CreateError::InvalidConfig(_))
    ));
}
