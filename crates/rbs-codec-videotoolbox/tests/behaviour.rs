//! Adapter behaviour beyond the port suite: bitstream shape, lease release, the
//! IOSurface wrapper cache, the system-memory copy path and input checks.
//!
//! Each test uses the software-capable adapter and skips (passes with a note) when the
//! machine has no VideoToolbox encoder at all, unless `RBS_REQUIRE_VIDEOTOOLBOX=1`.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{
    EncodeError, EncoderChange, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig,
};
use rbs_codec_videotoolbox::{
    FRAMES_IN_FLIGHT, SURFACE_CACHE, VideoToolboxFactory, VideoToolboxSession,
};
use rbs_core::media::{
    BitstreamFormat, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size,
    SurfaceSync, VideoFormat,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_media_format::avc::AvcConfig;
use rbs_media_format::nal::{self, h264};
use support::IoSurfaceSource;

const ORIGIN: Timestamp = Timestamp::from_nanos(3_000_000_000);

fn format(pixel_format: PixelFormat) -> VideoFormat {
    VideoFormat::new(Size::new(640, 360), pixel_format, ColorSpace::BT709_LIMITED)
}

fn config(codec: CodecId, pixel_format: PixelFormat, memory: MemoryKind) -> VideoEncoderConfig {
    VideoEncoderConfig::new(
        codec,
        format(pixel_format),
        FrameRate::FPS_30,
        memory,
        RateControl::Cbr {
            bitrate_bps: 2_000_000,
        },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(30))
}

/// A session, or `None` (with a note) when this machine cannot encode `codec`.
fn session(config: &VideoEncoderConfig) -> Option<VideoToolboxSession> {
    let factory = VideoToolboxFactory::with_software();
    let probed = factory.probe().unwrap();
    let declared = matches!(
        probed.get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(rbs_core::capability::CapabilityValue::Set(s)) if s.iter().any(|c| c == config.codec.as_str())
    );
    if !declared {
        assert!(
            !std::env::var("RBS_REQUIRE_VIDEOTOOLBOX").is_ok_and(|v| v == "1"),
            "no VideoToolbox {} encoder",
            config.codec
        );
        eprintln!("skipped: no VideoToolbox {} encoder", config.codec);
        return None;
    }
    Some(factory.create_session(config).expect("create"))
}

fn encode(
    session: &mut VideoToolboxSession,
    frames: &mut dyn FrameSource,
    count: u64,
) -> Vec<EncodedPacket> {
    let cadence = Cadence::frames(ORIGIN, FrameRate::FPS_30);
    let format = session.stream().params;
    let rbs_core::media::StreamParams::Video { format, .. } = format else {
        panic!("video stream expected");
    };
    let mut packets = Vec::with_capacity(count as usize);
    for i in 0..count {
        let frame = frames.frame(&format, i, cadence.time_of(i)).unwrap();
        loop {
            match session.submit(&frame) {
                Ok(()) => break,
                Err(EncodeError::Busy) => session.drain(&mut packets).unwrap(),
                Err(e) => panic!("submit {i}: {e}"),
            }
        }
        session.drain(&mut packets).unwrap();
    }
    session.flush(&mut packets).unwrap();
    packets
}

#[test]
fn h264_packets_are_length_prefixed_with_avcc_extradata() {
    let Some(mut s) = session(&config(
        CodecId::H264,
        PixelFormat::Nv12,
        MemoryKind::System,
    )) else {
        return;
    };
    let stream = s.stream().clone();
    assert_eq!(
        stream.bitstream,
        BitstreamFormat::LengthPrefixed { length_size: 4 }
    );
    let avcc = AvcConfig::parse(&stream.extradata).expect("valid avcC");
    assert_eq!(avcc.length_size, 4);
    assert!(!avcc.sps.is_empty() && !avcc.pps.is_empty());
    assert_eq!(avcc.profile, 100, "default profile is High");

    let packets = encode(&mut s, &mut SystemFrameSource::new(), 60);
    assert_eq!(packets.len(), 60);
    assert!(packets[0].keyframe(), "first packet must be a keyframe");
    for (i, p) in packets.iter().enumerate() {
        let units: Vec<&[u8]> = nal::length_prefixed_units(p.data(), 4)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| panic!("packet {i}: {e:?}"));
        assert!(!units.is_empty());
        let idr = units.iter().any(|u| nal::h264_type(u) == Some(h264::IDR));
        assert_eq!(idr, p.keyframe(), "packet {i}: IDR slices iff keyframe");
        assert_eq!(
            p.pts(),
            Cadence::frames(ORIGIN, FrameRate::FPS_30).time_of(i as u64)
        );
    }
    // The 30-frame interval puts another keyframe within frames 1..=30.
    assert!(packets[1..=30].iter().any(|p| p.keyframe()));
    let stats = s.stats();
    assert_eq!(stats.copied, 60);
    assert_eq!(stats.wrapped, 0);
    assert_eq!(stats.packets, 60);
}

#[test]
fn io_surfaces_are_wrapped_once_and_released_after_encoding() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let mut frames = IoSurfaceSource::new();
    let packets = encode(&mut s, &mut frames, 3 * support::RING as u64);
    assert_eq!(packets.len(), 3 * support::RING);
    assert_eq!(frames.held(), 0, "every lease is released once encoded");
    let stats = s.stats();
    assert_eq!(stats.copied, 0, "IOSurface input is never copied");
    assert!(
        stats.wrapped as usize <= frames.surfaces(),
        "each surface is wrapped at most once ({} wraps, {} surfaces)",
        stats.wrapped,
        frames.surfaces()
    );
}

#[test]
fn dropping_a_session_releases_pending_frames() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let mut frames = IoSurfaceSource::new();
    let format = format(PixelFormat::Nv12);
    let cadence = Cadence::frames(ORIGIN, FrameRate::FPS_30);
    for i in 0..4 {
        let frame = frames.frame(&format, i, cadence.time_of(i)).unwrap();
        s.submit(&frame).unwrap();
    }
    drop(s);
    assert_eq!(
        frames.held(),
        0,
        "drop completes or discards pending frames"
    );
}

#[test]
fn mismatching_input_is_refused() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let mut frames = IoSurfaceSource::new();
    let cadence = Cadence::frames(ORIGIN, FrameRate::FPS_30);
    // A system frame for a metal session.
    let system = SystemFrameSource::new()
        .frame(&format(PixelFormat::Nv12), 0, cadence.time_of(0))
        .unwrap();
    assert!(matches!(
        s.submit(&system),
        Err(EncodeError::InvalidInput(_))
    ));
    // Timestamps must increase.
    let first = frames
        .frame(&format(PixelFormat::Nv12), 0, cadence.time_of(1))
        .unwrap();
    s.submit(&first).unwrap();
    let again = frames
        .frame(&format(PixelFormat::Nv12), 1, cadence.time_of(1))
        .unwrap();
    assert!(matches!(
        s.submit(&again),
        Err(EncodeError::InvalidInput(_))
    ));
    let mut out = Vec::new();
    s.flush(&mut out).unwrap();
    assert_eq!(out.len(), 1);
    // The test still holds both frames; only the encoder's references must be gone.
    assert_eq!(frames.held(), 2);
    drop((first, again));
    drop(s);
    assert_eq!(frames.held(), 0);
}

#[test]
fn hevc_main10_from_p010_surfaces() {
    let Some(mut s) = session(&config(CodecId::Hevc, PixelFormat::P010, MemoryKind::Metal)) else {
        return;
    };
    let stream = s.stream().clone();
    // hvcC: version 1, general_profile_idc 2 (Main 10) in the low 5 bits of byte 1.
    assert_eq!(stream.extradata[0], 1);
    assert_eq!(stream.extradata[1] & 0x1f, 2, "Main 10 profile");
    let mut frames = IoSurfaceSource::new();
    let packets = encode(&mut s, &mut frames, 20);
    assert_eq!(packets.len(), 20);
    assert!(packets[0].keyframe());
    for p in &packets {
        assert!(nal::length_prefixed_units(p.data(), 4).all(|u| u.is_ok()));
    }
    assert_eq!(frames.held(), 0);
}

#[test]
fn unsupported_configurations_are_refused_at_create() {
    let factory = VideoToolboxFactory::with_software();
    let mut p010_h264 = config(CodecId::H264, PixelFormat::P010, MemoryKind::System);
    assert!(factory.create_session(&p010_h264).is_err());
    p010_h264.format = format(PixelFormat::I420);
    assert!(factory.create_session(&p010_h264).is_err());
    let mut gpu = config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Gpu);
    assert!(factory.create_session(&gpu).is_err());
    gpu.memory = MemoryKind::System;
    gpu.rate_control = RateControl::Cbr { bitrate_bps: 0 };
    assert!(factory.create_session(&gpu).is_err());
}

#[test]
fn a_full_session_is_busy_until_drained() {
    let Some(mut s) = session(&config(
        CodecId::H264,
        PixelFormat::Nv12,
        MemoryKind::System,
    )) else {
        return;
    };
    let mut frames = SystemFrameSource::new();
    let format = format(PixelFormat::Nv12);
    let cadence = Cadence::frames(ORIGIN, FrameRate::FPS_30);
    for i in 0..FRAMES_IN_FLIGHT as u64 {
        let frame = frames.frame(&format, i, cadence.time_of(i)).unwrap();
        s.submit(&frame).unwrap();
    }
    let next = FRAMES_IN_FLIGHT as u64;
    let frame = frames.frame(&format, next, cadence.time_of(next)).unwrap();
    assert!(matches!(s.submit(&frame), Err(EncodeError::Busy)));
    let mut out = Vec::new();
    s.flush(&mut out).unwrap();
    assert_eq!(out.len(), FRAMES_IN_FLIGHT);
    s.submit(&frame).unwrap();
    s.flush(&mut out).unwrap();
    assert_eq!(out.len(), FRAMES_IN_FLIGHT + 1);
}

#[test]
fn a_surface_of_another_size_is_refused() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let mut frames = IoSurfaceSource::new();
    let actual = VideoFormat::new(
        Size::new(320, 180),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let frame = frames.mislabelled(&actual, &format(PixelFormat::Nv12), ORIGIN);
    assert!(matches!(
        s.submit(&frame),
        Err(EncodeError::InvalidInput(_))
    ));
    drop(frame);
    assert_eq!(frames.held(), 0);
}

#[test]
fn surfaces_that_are_not_ready_are_refused() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let mut frames = IoSurfaceSource::new().with_sync(SurfaceSync::keyed_mutex(0, 1));
    let frame = frames.frame(&format(PixelFormat::Nv12), 0, ORIGIN).unwrap();
    assert!(matches!(
        s.submit(&frame),
        Err(EncodeError::InvalidInput(_))
    ));
}

#[test]
fn switching_rate_control_mode_is_unsupported() {
    let Some(mut s) = session(&config(
        CodecId::H264,
        PixelFormat::Nv12,
        MemoryKind::System,
    )) else {
        return;
    };
    let vbr = RateControl::Vbr {
        target_bps: 1_000_000,
        max_bps: 2_000_000,
    };
    assert!(matches!(
        s.reconfigure(&EncoderChange::Bitrate(vbr)),
        Err(EncodeError::Unsupported)
    ));
    let cbr = RateControl::Cbr {
        bitrate_bps: 1_000_000,
    };
    s.reconfigure(&EncoderChange::Bitrate(cbr)).unwrap();
}

#[test]
fn a_ring_larger_than_the_cache_still_releases_every_surface() {
    let Some(mut s) = session(&config(CodecId::H264, PixelFormat::Nv12, MemoryKind::Metal)) else {
        return;
    };
    let ring = SURFACE_CACHE + 8;
    let mut frames = IoSurfaceSource::with_ring(ring);
    let count = 3 * ring as u64;
    let packets = encode(&mut s, &mut frames, count);
    assert_eq!(packets.len(), count as usize);
    assert_eq!(frames.held(), 0);
    let stats = s.stats();
    assert!(
        stats.wrapped as usize > SURFACE_CACHE,
        "evicted surfaces are wrapped again"
    );
    assert!(stats.wrapped <= count);
}
