//! Runs the encoder port conformance suite against both VideoToolbox adapters, with
//! system-memory frames and with IOSurface (`metal`) frames.
//!
//! Machines without a usable encoder (other platforms, virtual machines without a media
//! engine for the hardware adapter) probe to an empty set and the suite skips. Set
//! `RBS_REQUIRE_VIDEOTOOLBOX=1` (software adapter) or `RBS_REQUIRE_VIDEOTOOLBOX_HW=1`
//! (hardware adapter) to make skipping fail instead.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_codec::conformance::{self, FrameSource, Report, SystemFrameSource};
use rbs_codec_videotoolbox::VideoToolboxFactory;
use support::IoSurfaceSource;

const CORE_CASES: [&str; 4] = [
    "h264/cbr/packets",
    "h264/cbr/keyframe-interval",
    "h264/cbr/request-keyframe",
    "h264/invalid-size",
];

fn required(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| v == "1")
}

fn check(report: &Report, require: bool) {
    eprintln!("{report}");
    if require {
        report.assert_ran(&CORE_CASES);
    } else {
        report.assert_passed();
    }
}

fn run(factory: &VideoToolboxFactory, frames: &mut dyn FrameSource, require: bool) {
    check(&conformance::video_encoder(factory, frames), require);
}

#[test]
fn software_adapter_with_system_frames() {
    run(
        &VideoToolboxFactory::with_software(),
        &mut SystemFrameSource::new(),
        required("RBS_REQUIRE_VIDEOTOOLBOX"),
    );
}

#[test]
fn software_adapter_with_io_surfaces() {
    let mut frames = IoSurfaceSource::new();
    run(
        &VideoToolboxFactory::with_software(),
        &mut frames,
        required("RBS_REQUIRE_VIDEOTOOLBOX"),
    );
    assert_eq!(frames.held(), 0, "the encoder kept surface leases");
}

#[test]
fn hardware_adapter_with_system_frames() {
    run(
        &VideoToolboxFactory::new(),
        &mut SystemFrameSource::new(),
        required("RBS_REQUIRE_VIDEOTOOLBOX_HW"),
    );
}

#[test]
fn hardware_adapter_with_io_surfaces() {
    let mut frames = IoSurfaceSource::new();
    run(
        &VideoToolboxFactory::new(),
        &mut frames,
        required("RBS_REQUIRE_VIDEOTOOLBOX_HW"),
    );
    assert_eq!(frames.held(), 0, "the encoder kept surface leases");
}
