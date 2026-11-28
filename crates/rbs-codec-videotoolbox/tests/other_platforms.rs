//! Off macOS the adapter compiles, probes to nothing and refuses sessions, so the
//! composition root can register it everywhere.

#![cfg(not(target_os = "macos"))]
#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_codec::{
    RateControl, VideoDecoderConfig, VideoDecoderFactory, VideoEncoderConfig, VideoEncoderFactory,
};
use rbs_codec_videotoolbox::{VideoToolboxDecoderFactory, VideoToolboxFactory};
use rbs_core::media::{CodecId, ColorSpace, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::AdapterFactory;
use rbs_core::timing::FrameRate;

#[test]
fn probes_empty_and_refuses_sessions() {
    for factory in [
        VideoToolboxFactory::new(),
        VideoToolboxFactory::with_software(),
    ] {
        assert!(factory.probe().unwrap().is_empty());
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
    assert_ne!(
        VideoToolboxFactory::new().id(),
        VideoToolboxFactory::with_software().id()
    );
}

#[test]
fn the_decoder_probes_empty_and_refuses_sessions() {
    let factory = VideoToolboxDecoderFactory::new();
    assert!(factory.probe().unwrap().is_empty());
    let config = VideoDecoderConfig::h264_annex_b(Size::new(640, 360));
    assert!(factory.create(&config).is_err());
    assert_ne!(factory.id(), VideoToolboxFactory::new().id());
}
