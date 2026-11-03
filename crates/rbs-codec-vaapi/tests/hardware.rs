//! Behaviour on real VA-API hardware beyond the conformance suite: the bitstream is
//! Annex B with parameter sets in the extradata and on every IDR (H.264 and HEVC),
//! HEVC Main 10 encodes from P010, a cropped Constrained Baseline session at
//! constant QP encodes, a bitrate change applies while running, and dropping a
//! session with frames in flight is clean.
//!
//! Skips without VA-API; `RBS_REQUIRE_VAAPI=1` makes skipping fail.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{
    EncodeError, EncoderChange, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig,
};
use rbs_codec_vaapi::{FRAMES_IN_FLIGHT, VaapiFactory};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    BitstreamFormat, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size, VideoFormat,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_media_format::nal;

fn encodes(codec: &str) -> bool {
    let caps = VaapiFactory::new().probe().unwrap();
    let found = matches!(
        caps.get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == codec)
    );
    if !found {
        // Every qualifying driver encodes H.264; HEVC is optional hardware.
        assert!(
            codec != "h264" || !std::env::var("RBS_REQUIRE_VAAPI").is_ok_and(|v| v == "1"),
            "RBS_REQUIRE_VAAPI=1 but VA-API does not encode {codec}"
        );
        eprintln!("no VA-API {codec} encoder; skipped");
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
    let mut session = VaapiFactory::new().create_session(&config).unwrap();
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
        assert!(
            !types.contains(&9),
            "packet {i} has an access unit delimiter"
        );
        if p.keyframe() {
            assert!(
                types.contains(&7) && types.contains(&5),
                "IDR {i}: {types:?}"
            );
            // The driver must pass the adapter's own SPS and PPS through unchanged,
            // once, so the slices agree with the extradata.
            let extradata = &stream.extradata[..];
            assert!(
                p.data().windows(extradata.len()).any(|w| w == extradata),
                "IDR {i} does not carry the extradata parameter sets"
            );
            assert_eq!(
                types.iter().filter(|t| **t == 7).count(),
                1,
                "IDR {i}: {types:?}"
            );
        } else {
            assert!(types.contains(&1), "P frame {i}: {types:?}");
        }
    }
    let stats = session.stats();
    assert_eq!((stats.submitted, stats.packets, stats.copied), (90, 90, 90));
}

/// HEVC NAL unit types of an Annex B stream.
fn hevc_types(data: &[u8]) -> Vec<u8> {
    nal::annexb_unit_headers(data)
        .map(|h| (h >> 1) & 0x3f)
        .collect()
}

fn hevc_annex_b_with_parameter_sets(pixel_format: PixelFormat) {
    let config = config(CodecId::Hevc, pixel_format);
    let mut session = VaapiFactory::new().create_session(&config).unwrap();
    let stream = session.stream().clone();
    assert_eq!(stream.bitstream, BitstreamFormat::AnnexB);
    assert_eq!(hevc_types(&stream.extradata), vec![32, 33, 34]);

    let packets = encode(&mut session, &config.format, 90);
    assert_eq!(packets.len(), 90);
    for (i, p) in packets.iter().enumerate() {
        let types = hevc_types(p.data());
        assert_eq!(p.keyframe(), i % 30 == 0, "packet {i}: {types:?}");
        assert!(
            !types.contains(&35),
            "packet {i} has an access unit delimiter"
        );
        if p.keyframe() {
            assert!(types.contains(&19), "IDR {i}: {types:?}");
            // The driver must pass the adapter's own VPS, SPS and PPS through
            // unchanged, once.
            let extradata = &stream.extradata[..];
            assert!(
                p.data().windows(extradata.len()).any(|w| w == extradata),
                "IDR {i} does not carry the extradata parameter sets"
            );
            assert_eq!(
                types.iter().filter(|t| **t == 33).count(),
                1,
                "IDR {i}: {types:?}"
            );
        } else {
            assert!(types.contains(&1), "P frame {i}: {types:?}");
        }
    }
    let stats = session.stats();
    assert_eq!((stats.submitted, stats.packets, stats.copied), (90, 90, 90));
}

#[test]
fn hevc_is_annex_b_with_parameter_sets() {
    if !encodes("hevc") {
        return;
    }
    hevc_annex_b_with_parameter_sets(PixelFormat::Nv12);
}

#[test]
fn hevc_main10_from_p010() {
    if !encodes("hevc") {
        return;
    }
    let caps = VaapiFactory::new().probe().unwrap();
    if !matches!(
        caps.get(&rbs_codec::caps::VIDEO_PROFILES),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "hevc.main10")
    ) {
        eprintln!("no HEVC Main 10; skipped");
        return;
    }
    hevc_annex_b_with_parameter_sets(PixelFormat::P010);
}

#[test]
fn encodes_again_after_flush() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut session = VaapiFactory::new().create_session(&config).unwrap();
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
fn cropped_baseline_at_constant_quality() {
    if !encodes("h264") {
        return;
    }
    let caps = VaapiFactory::new().probe().unwrap();
    if !matches!(
        caps.get(&rbs_codec::caps::VIDEO_RATE_CONTROL),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "constant_quality")
    ) {
        eprintln!("no constant QP; skipped");
        return;
    }
    let format = VideoFormat::new(
        Size::new(642, 362),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let config = VideoEncoderConfig::new(
        CodecId::H264,
        format,
        FrameRate::FPS_30,
        MemoryKind::System,
        RateControl::ConstantQuality { level: 30 },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(10))
    .with_profile("baseline");
    let Ok(mut session) = VaapiFactory::new().create_session(&config) else {
        eprintln!("no Constrained Baseline; skipped");
        return;
    };
    let packets = encode(&mut session, &format, 25);
    assert_eq!(packets.len(), 25);
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.keyframe(), i % 10 == 0, "packet {i}");
    }
}

#[test]
fn bitrate_change_while_running() {
    if !encodes("h264") {
        return;
    }
    let config = config(CodecId::H264, PixelFormat::Nv12);
    let mut session = VaapiFactory::new().create_session(&config).unwrap();
    let mut packets = encode_range(&mut session, &config.format, 0..20);
    session
        .reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
            bitrate_bps: 1_000_000,
        }))
        .unwrap();
    packets.extend(encode_range(&mut session, &config.format, 20..40));
    assert_eq!(packets.len(), 40);
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
        let mut session = VaapiFactory::new().create_session(&config).unwrap();
        for i in 0..FRAMES_IN_FLIGHT as u64 {
            let frame = source.frame(&config.format, i, cadence.time_of(i)).unwrap();
            session.submit(&frame).unwrap();
        }
        drop(session);
    }
}
