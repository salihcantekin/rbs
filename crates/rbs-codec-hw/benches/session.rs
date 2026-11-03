//! Micro-benchmark of the shared session (measure-performance skill, ADR-0009):
//! `submit` plus `drain` per 1080p NV12 frame over an engine that copies the frame
//! into its own input buffer (as every hardware engine does) and finishes each
//! picture at once with a small packet, and once without the copy (the session's
//! own bookkeeping). Measures the session's bookkeeping and the
//! row copy, which run on the encoder worker thread for every vendor.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benchmarks fail by panicking.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{RateControl, VideoEncoder};
use rbs_codec_hw::copy::{DstPlane, copy_420};
use rbs_codec_hw::plan::Rc;
use rbs_codec_hw::session::{Engine, EngineError, Output, Session};
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, PixelFormat, Size, StreamDescriptor, StreamParams,
    SystemFrame, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};

const SLOTS: usize = 8;
const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

struct CopyEngine {
    copy: bool,
    inputs: Vec<Vec<u8>>,
    tokens: [u64; SLOTS],
}

impl Engine for CopyEngine {
    fn slots(&self) -> usize {
        SLOTS
    }

    fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError> {
        if !self.copy {
            return Ok(());
        }
        let (luma, chroma) = self.inputs[slot].split_at_mut(WIDTH * HEIGHT);
        let ok = copy_420(
            frame,
            [
                DstPlane {
                    data: luma,
                    pitch: WIDTH,
                },
                DstPlane {
                    data: chroma,
                    pitch: WIDTH,
                },
            ],
        );
        if ok {
            Ok(())
        } else {
            Err(EngineError::Broken("copy"))
        }
    }

    fn encode(&mut self, slot: usize, token: u64, _idr: bool) -> Result<(), EngineError> {
        self.tokens[slot] = token;
        Ok(())
    }

    fn output(&mut self, slot: usize, _wait: bool) -> Result<Option<Output>, EngineError> {
        Ok(Some(Output {
            data: vec![0, 0, 0, 1, 0x41, 0x88],
            keyframe: false,
            token: self.tokens[slot],
        }))
    }

    fn end_of_stream(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn set_rate_control(&mut self, _: Rc) -> Result<(), EngineError> {
        Ok(())
    }
}

fn submit_drain(c: &mut Criterion) {
    run(c, true, "session/submit_drain_copy_1080p");
    run(c, false, "session/submit_drain_bookkeeping");
}

fn run(c: &mut Criterion, copy: bool, name: &str) {
    let format = VideoFormat::new(
        Size::new(WIDTH as u32, HEIGHT as u32),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let engine = CopyEngine {
        copy,
        inputs: (0..SLOTS)
            .map(|_| vec![0; WIDTH * HEIGHT * 3 / 2])
            .collect(),
        tokens: [0; SLOTS],
    };
    let stream = StreamDescriptor::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        Bytes::new(),
        StreamParams::video(format, FrameRate::FPS_60),
    );
    let mut session = Session::new(
        engine,
        CodecId::H264,
        format,
        RateControl::Cbr {
            bitrate_bps: 6_000_000,
        },
        true,
        stream,
    );
    let mut frames = SystemFrameSource::new();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_60);
    let mut index = 0;
    let mut packets = Vec::with_capacity(1024);
    c.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let frame = frames
                    .frame(&format, index, cadence.time_of(index))
                    .unwrap();
                index += 1;
                let started = Instant::now();
                session.submit(&frame).unwrap();
                session.drain(&mut packets).unwrap();
                total += started.elapsed();
                packets.clear();
            }
            total
        });
    });
}

criterion_group!(benches, submit_drain);
criterion_main!(benches);
