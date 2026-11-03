//! Behaviour on real AMF hardware beyond the conformance suite: the bitstream is
//! Annex B with parameter sets in the extradata and on every IDR, a 10-bit HEVC
//! session takes P010, and dropping a session with frames in flight is clean.
//!
//! Skips without AMF; `RBS_REQUIRE_AMF=1` makes skipping the H.264 tests fail.

#![cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{EncodeError, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig};
use rbs_codec_amf::{AmfFactory, FRAMES_IN_FLIGHT};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    BitstreamFormat, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size, VideoFormat,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_media_format::nal;

fn encodes(codec: &str) -> bool {
    let caps = AmfFactory::new().probe().unwrap();
    let found = matches!(
        caps.get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == codec)
    );
    // `RBS_REQUIRE_AMF=1` makes only H.264 mandatory (as in the conformance suite's core
    // cases): some devices have no HEVC encoder (an AMD Ryzen 7000 iGPU refuses the
    // AMF HEVC component), and their HEVC tests skip.
    if !found {
        assert!(
            codec != "h264" || !std::env::var("RBS_REQUIRE_AMF").is_ok_and(|v| v == "1"),
            "RBS_REQUIRE_AMF=1 but AMF does not encode {codec}"
        );
        eprintln!("no AMF {codec} encoder; skipped");
    }
    found
}

fn config(codec: CodecId, pixel_format: PixelFormat) -> VideoEncoderConfig {
    VideoEncoderConfig::new(
        codec,
        VideoFormat::new(
            Size::new(1280, 720),
            pixel_format,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_60,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: 4_000_000,
        },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(30))
}

fn encode(session: &mut dyn VideoEncoder, format: &VideoFormat, frames: u64) -> Vec<EncodedPacket> {
    encode_range(session, format, 0..frames)
}

fn encode_range(
    session: &mut dyn VideoEncoder,
    format: &VideoFormat,
    frames: std::ops::Range<u64>,
) -> Vec<EncodedPacket> {
    let mut source = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    let mut packets = Vec::new();
    for i in frames {
        let frame = source.frame(format, i, cadence.time_of(i)).unwrap();
        match session.submit(&frame) {
            Err(EncodeError::Busy) => {
                session.drain(&mut packets).unwrap();
                session.submit(&frame).unwrap();
            }
            other => other.unwrap(),
        }
        session.drain(&mut packets).unwrap();
    }
    session.flush(&mut packets).unwrap();
    packets
}

#[test]
fn h264_is_annex_b_with_parameter_sets() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut session = AmfFactory::new().create_session(&config).unwrap();
    let stream = session.stream().clone();
    assert_eq!(stream.bitstream, BitstreamFormat::AnnexB);
    let types: Vec<u8> = nal::annexb_unit_headers(&stream.extradata)
        .map(|h| h & 0x1f)
        .collect();
    assert!(
        types.contains(&7) && types.contains(&8),
        "extradata NAL types {types:?}"
    );

    let packets = encode(&mut session, &config.format, 90);
    assert_eq!(packets.len(), 90);
    for (i, p) in packets.iter().enumerate() {
        let types: Vec<u8> = nal::annexb_unit_headers(p.data())
            .map(|h| h & 0x1f)
            .collect();
        assert_eq!(p.keyframe(), i % 30 == 0, "packet {i}: {types:?}");
        if p.keyframe() {
            assert!(
                types.contains(&7) && types.contains(&5),
                "IDR {i}: {types:?}"
            );
        } else {
            assert!(types.contains(&1), "P frame {i}: {types:?}");
        }
    }
    let stats = session.stats();
    assert_eq!((stats.submitted, stats.packets, stats.copied), (90, 90, 90));
}

/// The parameter sets of an Annex B buffer, in order (H.264 SPS and PPS, HEVC VPS, SPS
/// and PPS).
fn parameter_sets(data: &[u8], codec: CodecId) -> Vec<Vec<u8>> {
    nal::annexb_units(data)
        .filter(|u| match codec {
            CodecId::Hevc => matches!((u[0] >> 1) & 0x3f, 32..=34),
            _ => matches!(u[0] & 0x1f, 7 | 8),
        })
        .map(<[u8]>::to_vec)
        .collect()
}

/// Muxers compare in-band parameter sets with the extradata byte for byte (Matroska,
/// fMP4); AMF's `ExtraData` property once left out the VUI the in-band SPS carries.
#[test]
fn extradata_equals_the_in_band_parameter_sets() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut session = AmfFactory::new().create_session(&config).unwrap();
    let extradata = parameter_sets(&session.stream().extradata, CodecId::H264);
    let packets = encode(&mut session, &config.format, 31);
    for i in [0, 30] {
        assert!(packets[i].keyframe(), "packet {i}");
        assert_eq!(
            parameter_sets(packets[i].data(), CodecId::H264),
            extradata,
            "IDR {i}"
        );
    }
}

#[test]
fn encodes_again_after_flush() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut session = AmfFactory::new().create_session(&config).unwrap();
    assert_eq!(encode_range(&mut session, &config.format, 0..10).len(), 10);
    // `flush` drained the encoder; the next frame restarts it with an IDR.
    let packets = encode_range(&mut session, &config.format, 10..20);
    assert_eq!(packets.len(), 10);
    let types: Vec<u8> = nal::annexb_unit_headers(packets[0].data())
        .map(|h| h & 0x1f)
        .collect();
    assert!(packets[0].keyframe(), "first packet after flush: {types:?}");
    assert!(types.contains(&7) && types.contains(&5), "{types:?}");
    assert!(packets[1..].iter().all(|p| !p.keyframe()));
}

#[test]
fn hevc_main10_from_p010() {
    if !encodes("hevc") {
        return;
    }
    let caps = AmfFactory::new().probe().unwrap();
    if !matches!(
        caps.get(&rbs_codec::caps::INPUT_PIXEL_FORMAT),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "p010")
    ) {
        eprintln!("no 10-bit HEVC; skipped");
        return;
    }
    let config = config(CodecId::Hevc, PixelFormat::P010);
    let mut session = AmfFactory::new().create_session(&config).unwrap();
    let types: Vec<u8> = nal::annexb_unit_headers(&session.stream().extradata)
        .map(|h| (h >> 1) & 0x3f)
        .collect();
    assert!(
        types.contains(&32) && types.contains(&33) && types.contains(&34),
        "VPS/SPS/PPS missing: {types:?}"
    );
    let packets = encode(&mut session, &config.format, 40);
    assert_eq!(packets.len(), 40);
    assert!(packets[0].keyframe() && packets[30].keyframe());
    // Parameter sets in band on every IDR, so each one starts a decodable stream.
    for i in [0, 30] {
        let types: Vec<u8> = nal::annexb_unit_headers(packets[i].data())
            .map(|h| (h >> 1) & 0x3f)
            .collect();
        assert!(
            types.contains(&32) && types.contains(&33) && types.contains(&34),
            "IDR {i}: {types:?}"
        );
    }
}

#[test]
fn drop_with_frames_in_flight() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut source = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    for _ in 0..3 {
        let mut session = AmfFactory::new().create_session(&config).unwrap();
        for i in 0..FRAMES_IN_FLIGHT as u64 {
            let frame = source.frame(&config.format, i, cadence.time_of(i)).unwrap();
            session.submit(&frame).unwrap();
        }
        drop(session);
    }
}

/// Bits per second of `frames` copies of one still picture (the case where CBR without
/// padding falls far below its target), and whether a filler NAL unit was seen.
fn still_rate(config: &VideoEncoderConfig, frames: u64) -> (f64, bool) {
    let mut session = AmfFactory::new().create_session(config).unwrap();
    let mut source = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    let mut packets = Vec::new();
    for i in 0..frames {
        let frame = source.frame(&config.format, 0, cadence.time_of(i)).unwrap();
        match session.submit(&frame) {
            Err(EncodeError::Busy) => {
                session.drain(&mut packets).unwrap();
                session.submit(&frame).unwrap();
            }
            other => other.unwrap(),
        }
        session.drain(&mut packets).unwrap();
    }
    session.flush(&mut packets).unwrap();
    let bytes: usize = packets.iter().map(|p| p.data().len()).sum();
    let filler = packets.iter().any(|p| {
        nal::annexb_unit_headers(p.data()).any(|h| match config.codec {
            CodecId::Hevc => (h >> 1) & 0x3f == 38,
            _ => h & 0x1f == 12,
        })
    });
    let seconds = frames as f64 / 60.0;
    (bytes as f64 * 8.0 / seconds, filler)
}

/// `cbr_padded` fills a still picture up to the target with filler data; plain CBR
/// does not (so the padding is real, not just accepted).
#[test]
fn cbr_padded_reaches_the_target_on_a_still_picture() {
    for (codec, name) in [(CodecId::H264, "h264"), (CodecId::Hevc, "hevc")] {
        if !encodes(name) {
            continue;
        }
        let target = 4_000_000u64;
        let plain = config(codec, PixelFormat::Nv12);
        let mut padded = plain.clone();
        padded.rate_control = RateControl::CbrPadded {
            bitrate_bps: target,
        };
        // 4 s at 60 fps: long enough for the rate controller to settle.
        let (plain_bps, plain_filler) = still_rate(&plain, 240);
        let (padded_bps, padded_filler) = still_rate(&padded, 240);
        eprintln!(
            "{name}: plain CBR {:.0} kbit/s, padded {:.0} kbit/s (target {} kbit/s)",
            plain_bps / 1000.0,
            padded_bps / 1000.0,
            target / 1000
        );
        assert!(
            padded_filler,
            "{name}: no filler NAL unit in the padded stream"
        );
        assert!(!plain_filler, "{name}: filler data without padding");
        assert!(
            padded_bps >= 0.9 * target as f64,
            "{name}: padded {padded_bps:.0} bit/s is below 90 % of {target}"
        );
    }
}
