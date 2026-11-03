//! Behaviour on real QSV hardware beyond the conformance suite: the bitstream is
//! Annex B with parameter sets in the extradata and on every IDR, a 10-bit HEVC
//! session takes P010, and dropping a session with frames in flight is clean.
//!
//! Skips without QSV; `RBS_REQUIRE_QSV=1` makes skipping fail.

#![cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{EncodeError, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig};
use rbs_codec_qsv::{FRAMES_IN_FLIGHT, QsvFactory};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    BitstreamFormat, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size, VideoFormat,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_media_format::nal;

fn encodes(codec: &str) -> bool {
    let caps = QsvFactory::new().probe().unwrap();
    let found = matches!(
        caps.get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == codec)
    );
    if !found {
        assert!(
            !std::env::var("RBS_REQUIRE_QSV").is_ok_and(|v| v == "1"),
            "RBS_REQUIRE_QSV=1 but QSV does not encode {codec}"
        );
        eprintln!("no QSV {codec} encoder; skipped");
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
    let mut source = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    let mut packets = Vec::new();
    for i in 0..frames {
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
    let mut session = QsvFactory::new().create_session(&config).unwrap();
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

#[test]
fn hevc_main10_from_p010() {
    if !encodes("hevc") {
        return;
    }
    let caps = QsvFactory::new().probe().unwrap();
    if !matches!(
        caps.get(&rbs_codec::caps::INPUT_PIXEL_FORMAT),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "p010")
    ) {
        eprintln!("no 10-bit HEVC; skipped");
        return;
    }
    let config = config(CodecId::Hevc, PixelFormat::P010);
    let mut session = QsvFactory::new().create_session(&config).unwrap();
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
        let mut session = QsvFactory::new().create_session(&config).unwrap();
        for i in 0..FRAMES_IN_FLIGHT as u64 {
            let frame = source.frame(&config.format, i, cadence.time_of(i)).unwrap();
            session.submit(&frame).unwrap();
        }
        drop(session);
    }
}
