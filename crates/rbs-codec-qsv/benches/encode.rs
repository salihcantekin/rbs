//! Micro-benchmark of the encode hand-off (measure-performance skill, ADR-0009):
//! `submit` plus `drain` per 1080p NV12 frame from system memory (one copy into a QSV
//! input surface). Only those two calls are timed; filling the test frames is
//! not. QSV encodes on the GPU while the next frame is copied, so the number is the
//! cost on the encoder worker thread, not the encode latency.
//!
//! Does nothing without QSV.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benchmarks fail by panicking.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{
    EncodeError, KeyframeInterval, RateControl, VideoEncoderConfig, VideoEncoderFactory,
};
use rbs_codec_qsv::QsvFactory;
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{CodecId, ColorSpace, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};

fn submit_drain(c: &mut Criterion) {
    let factory = QsvFactory::new();
    if !matches!(
        factory.probe().unwrap().get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "h264")
    ) {
        eprintln!("no QSV H.264 encoder; skipped");
        return;
    }
    let format = VideoFormat::new(
        Size::new(1920, 1080),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let config = VideoEncoderConfig::new(
        CodecId::H264,
        format,
        FrameRate::FPS_60,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: 6_000_000,
        },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(120));
    let mut session = factory.create(&config).unwrap();
    let mut frames = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    let mut index = 0;
    let mut packets = Vec::with_capacity(1024);
    c.bench_function("submit_drain/system_1080p", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let frame = frames
                    .frame(&format, index, cadence.time_of(index))
                    .unwrap();
                index += 1;
                let started = Instant::now();
                loop {
                    match session.submit(&frame) {
                        Err(EncodeError::Busy) => session.drain(&mut packets).unwrap(),
                        other => break other.unwrap(),
                    }
                }
                session.drain(&mut packets).unwrap();
                total += started.elapsed();
                packets.clear();
            }
            total
        });
    });
    session.flush(&mut packets).unwrap();
}

criterion_group!(benches, submit_drain);
criterion_main!(benches);
