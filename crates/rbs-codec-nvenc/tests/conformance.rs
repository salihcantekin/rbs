//! Runs the encoder port conformance suite against NVENC with system-memory frames.
//!
//! Machines without an NVIDIA driver and GPU probe to an empty set and the suite
//! skips. Set `RBS_REQUIRE_NVENC=1` on a machine with NVENC to make skipping fail
//! instead.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{self, SystemFrameSource};
use rbs_codec::{RateControl, VideoEncoderConfig, VideoEncoderFactory};
use rbs_codec_nvenc::NvencFactory;
use rbs_core::media::{CodecId, ColorSpace, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::AdapterFactory;
use rbs_core::timing::FrameRate;

const CORE_CASES: [&str; 4] = [
    "h264/cbr/packets",
    "h264/cbr/keyframe-interval",
    "h264/cbr/request-keyframe",
    "h264/invalid-size",
];

fn required() -> bool {
    std::env::var("RBS_REQUIRE_NVENC").is_ok_and(|v| v == "1")
}

#[test]
fn conformance_with_system_frames() {
    let report = conformance::video_encoder(&NvencFactory::new(), &mut SystemFrameSource::new());
    eprintln!("{report}");
    if required() {
        report.assert_ran(&CORE_CASES);
    } else {
        report.assert_passed();
    }
}

#[test]
fn without_nvenc_probes_empty_and_refuses_sessions() {
    let factory = NvencFactory::new();
    let caps = factory.probe().unwrap();
    if !caps.is_empty() {
        assert!(!required() || caps.get(&rbs_codec::caps::VIDEO_ENCODE).is_some());
        return;
    }
    assert!(
        !required(),
        "RBS_REQUIRE_NVENC=1 but NVENC probed to nothing"
    );
    let config = VideoEncoderConfig::new(
        CodecId::H264,
        VideoFormat::new(
            Size::new(640, 360),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_30,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: 1_000_000,
        },
    );
    assert!(factory.create(&config).is_err());
}
