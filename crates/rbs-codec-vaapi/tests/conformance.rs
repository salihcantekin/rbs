//! Runs the encoder port conformance suite against VA-API with system-memory frames.
//!
//! Machines without libva and an encoding VA driver probe to an empty set and the suite
//! skips. Set `RBS_REQUIRE_VAAPI=1` on a machine with VA-API to make skipping fail
//! instead.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{self, SystemFrameSource};
use rbs_codec::{RateControl, VideoEncoderConfig, VideoEncoderFactory};
use rbs_codec_vaapi::VaapiFactory;
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
    std::env::var("RBS_REQUIRE_VAAPI").is_ok_and(|v| v == "1")
}

#[test]
fn conformance_with_system_frames() {
    let report = conformance::video_encoder(&VaapiFactory::new(), &mut SystemFrameSource::new());
    eprintln!("{report}");
    if required() {
        report.assert_ran(&CORE_CASES);
    } else {
        report.assert_passed();
    }
}

#[test]
fn without_vaapi_probes_empty_and_refuses_sessions() {
    let factory = VaapiFactory::new();
    let caps = factory.probe().unwrap();
    if !caps.is_empty() {
        assert!(!required() || caps.get(&rbs_codec::caps::VIDEO_ENCODE).is_some());
        return;
    }
    assert!(
        !required(),
        "RBS_REQUIRE_VAAPI=1 but VA-API probed to nothing"
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
