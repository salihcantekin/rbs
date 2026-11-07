//! Runs the virtual camera conformance suite against the test adapter.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use bytes::Bytes;
use rbs_core::media::{
    ColorSpace, MemoryKind, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_vcam::conformance::{self, Options};
use rbs_vcam::{CameraFormat, VirtualCameraConfig, VirtualCameraFactory};
use rbs_vcam_test::TestVcamFactory;

const W: u32 = 64;
const H: u32 = 36;

fn frame(n: u64) -> VideoFrame {
    let format = VideoFormat::new(
        Size {
            width: W,
            height: H,
        },
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let w = W as usize;
    let planes = [
        PlaneLayout {
            offset: 0,
            stride: w,
        },
        PlaneLayout {
            offset: w * H as usize,
            stride: w,
        },
    ];
    let data = Bytes::from(vec![0u8; w * H as usize * 3 / 2]);
    let f = SystemFrame::new(format, data, &planes).unwrap();
    VideoFrame::system(Timestamp::from_nanos(n), f, None).with_sequence(n)
}

fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

fn thread_ids() -> Option<Vec<u64>> {
    let entries = std::fs::read_dir("/proc/self/task").ok()?;
    Some(
        entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect(),
    )
}

#[test]
fn the_test_adapter_conforms() {
    let factory = TestVcamFactory::new();
    factory.install().unwrap();
    let options = Options {
        config: VirtualCameraConfig::new(
            CameraFormat::new(W, H, FrameRate::FPS_30, PixelFormat::Nv12),
            MemoryKind::System,
        ),
        other_format: CameraFormat::new(1280, 720, FrameRate::FPS_30, PixelFormat::Nv12),
        frame: &frame,
        measure_allocations: Some(measure),
        // This binary runs a single test, so the process's threads are its own.
        process_threads: cfg!(target_os = "linux")
            .then_some(thread_ids as fn() -> Option<Vec<u64>>),
    };
    let failures = conformance::run(&factory, factory.consumer().as_ref(), &options);
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_format_change_waits_while_an_app_reads() {
    let factory = TestVcamFactory::new();
    let f = CameraFormat::new(640, 360, FrameRate::FPS_30, PixelFormat::Nv12);
    factory.consumer().set_reading(true);
    assert!(!factory.set_format(&f).unwrap());
    factory.consumer().set_reading(false);
    assert!(factory.set_format(&f).unwrap());
    assert_eq!(factory.format(), Some(f));
}
