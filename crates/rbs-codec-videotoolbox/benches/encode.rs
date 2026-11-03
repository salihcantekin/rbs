//! Micro-benchmarks of the encode hand-off (measure-performance skill, ADR-0009):
//! `submit` plus `drain` per 1080p frame, from IOSurfaces (the zero-copy default) and
//! from system memory (one copy into a pool buffer). Only those two calls are timed;
//! filling the test frames is not. VideoToolbox encodes asynchronously, so the numbers
//! are the cost on the encoder worker thread, not the encode latency.
//!
//! Uses the hardware adapter when the machine has a media engine, else the software
//! one. On other platforms it does nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benchmarks fail by panicking.

#[cfg(target_os = "macos")]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(target_os = "macos")]
mod bench {
    use std::time::{Duration, Instant};

    use criterion::Criterion;
    use rbs_codec::conformance::{FrameSource, SystemFrameSource};
    use rbs_codec::{EncodeError, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig};
    use rbs_codec_videotoolbox::VideoToolboxFactory;
    use rbs_core::capability::CapabilityValue;
    use rbs_core::media::{CodecId, ColorSpace, MemoryKind, PixelFormat, Size, VideoFormat};
    use rbs_core::registry::AdapterFactory;
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};

    use crate::support::IoSurfaceSource;

    fn factory() -> Option<(VideoToolboxFactory, &'static str)> {
        let has_h264 = |f: &VideoToolboxFactory| {
            matches!(
                f.probe().unwrap().get(&rbs_codec::caps::VIDEO_ENCODE),
                Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "h264")
            )
        };
        [
            (VideoToolboxFactory::new(), "hw"),
            (VideoToolboxFactory::with_software(), "sw"),
        ]
        .into_iter()
        .find(|(f, _)| has_h264(f))
    }

    fn run(c: &mut Criterion, memory: MemoryKind, frames: &mut dyn FrameSource) {
        let Some((factory, kind)) = factory() else {
            eprintln!("no VideoToolbox H.264 encoder; skipped");
            return;
        };
        let format = VideoFormat::new(
            Size::new(1920, 1080),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        let config = VideoEncoderConfig::new(
            CodecId::H264,
            format,
            FrameRate::FPS_60,
            memory,
            RateControl::Cbr {
                bitrate_bps: 6_000_000,
            },
        )
        .with_keyframe_interval(KeyframeInterval::Frames(120));
        let mut session = factory.create_session(&config).unwrap();
        let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
        let mut index = 0;
        let mut packets = Vec::with_capacity(1024);
        c.bench_function(&format!("submit_drain/{memory}_1080p_{kind}"), |b| {
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
                            Ok(()) => break,
                            Err(EncodeError::Busy) => session.drain(&mut packets).unwrap(),
                            Err(e) => panic!("submit: {e}"),
                        }
                    }
                    session.drain(&mut packets).unwrap();
                    total += started.elapsed();
                    drop(frame);
                    packets.clear();
                }
                total
            })
        });
        session.flush(&mut packets).unwrap();
    }

    pub fn main() {
        let mut c = Criterion::default().configure_from_args();
        run(&mut c, MemoryKind::Metal, &mut IoSurfaceSource::new());
        run(&mut c, MemoryKind::System, &mut SystemFrameSource::new());
        c.final_summary();
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    bench::main();
}
