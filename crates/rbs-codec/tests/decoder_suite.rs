//! Checks the live decoder suite: a correct fake decoder passes, and one that breaks an
//! invariant fails with the right case.
#![allow(clippy::expect_used)]

use std::collections::VecDeque;

use rbs_codec::caps;
use rbs_codec::conformance::{self, DecodeClip};
use rbs_codec::{CreateError, DecodeError, VideoDecoder, VideoDecoderConfig, VideoDecoderFactory};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, PlaneLayout, Size,
    StreamDescriptor, StreamParams, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;

const SIZE: Size = Size::new(64, 48);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    DecodesBeforeKeyframe,
    WrongPts,
    IgnoresMaxSize,
}

struct FakeFactory(Fault);

impl AdapterFactory for FakeFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.codec.fake-decoder")
    }
    fn display_name(&self) -> &str {
        "fake video decoder"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(
            caps::VIDEO_DECODE,
            CapabilityValue::Set(vec!["h264".into()]),
        ))
    }
}

impl VideoDecoderFactory for FakeFactory {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> {
        Ok(Box::new(Fake {
            fault: self.0,
            max: config.max_size,
            have_key: false,
            queue: VecDeque::new(),
            stream: StreamDescriptor::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                Bytes::new(),
                StreamParams::video(
                    VideoFormat::new(
                        config.max_size,
                        PixelFormat::Nv12,
                        ColorSpace::BT709_LIMITED,
                    ),
                    FrameRate::FPS_30,
                ),
            ),
        }))
    }
}

struct Fake {
    fault: Fault,
    max: Size,
    have_key: bool,
    queue: VecDeque<VideoFrame>,
    stream: StreamDescriptor,
}

impl VideoDecoder for Fake {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn submit(&mut self, p: &EncodedPacket) -> Result<(), DecodeError> {
        if p.codec() != CodecId::H264 {
            return Err(DecodeError::InvalidInput("codec".into()));
        }
        if p.keyframe() {
            // The fake bitstream: width and height as two big-endian u16.
            let d = p.data();
            let (w, h) = (
                u32::from(d[0]) << 8 | u32::from(d[1]),
                u32::from(d[2]) << 8 | u32::from(d[3]),
            );
            if self.fault != Fault::IgnoresMaxSize && (w > self.max.width || h > self.max.height) {
                return Err(DecodeError::InvalidInput("too large".into()));
            }
            self.have_key = true;
        } else if !self.have_key && self.fault != Fault::DecodesBeforeKeyframe {
            return Err(DecodeError::NeedKeyframe);
        }
        let format = VideoFormat::new(SIZE, PixelFormat::Nv12, ColorSpace::BT709_LIMITED);
        let data = Bytes::from(vec![0u8; 64 * 48 * 3 / 2]);
        let frame = SystemFrame::new(
            format,
            data,
            &[
                PlaneLayout {
                    offset: 0,
                    stride: 64,
                },
                PlaneLayout {
                    offset: 64 * 48,
                    stride: 64,
                },
            ],
        )
        .expect("valid fake frame");
        let pts = if self.fault == Fault::WrongPts {
            Timestamp::from_nanos(p.pts().as_nanos() + 1)
        } else {
            p.pts()
        };
        self.queue.push_back(VideoFrame::system(pts, frame, None));
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError> {
        out.extend(self.queue.drain(..));
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DecodeError> {
        self.queue.clear();
        self.have_key = false;
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError> {
        self.drain(out)
    }
}

fn clip() -> DecodeClip {
    let packets = (0..5u64)
        .map(|i| {
            let data = if i == 0 {
                Bytes::from_static(&[0, 64, 0, 48])
            } else {
                Bytes::from_static(&[9, 9, 9, 9])
            };
            let t = Timestamp::from_nanos(1_000_000 * (i + 1));
            EncodedPacket::new(CodecId::H264, BitstreamFormat::AnnexB, t, t, i == 0, data)
                .expect("packet")
        })
        .collect();
    DecodeClip {
        codec: CodecId::H264,
        packets,
        size: SIZE,
    }
}

fn failed(report: &rbs_core::conformance::Report) -> Vec<String> {
    report.failures().map(|c| c.name.clone()).collect()
}

#[test]
fn a_correct_decoder_passes() {
    let report = conformance::video_decoder(&FakeFactory(Fault::None), &clip());
    report.assert_passed();
    report.assert_ran(&[
        "delta-before-keyframe",
        "decode-clip",
        "second-flush-empty",
        "reset-then-keyframe",
        "foreign-codec",
        "max-size",
    ]);
}

#[test]
fn decoding_before_a_keyframe_fails() {
    let r = conformance::video_decoder(&FakeFactory(Fault::DecodesBeforeKeyframe), &clip());
    assert!(failed(&r).contains(&"delta-before-keyframe".to_string()));
}

#[test]
fn a_changed_timestamp_fails() {
    let r = conformance::video_decoder(&FakeFactory(Fault::WrongPts), &clip());
    assert!(failed(&r).contains(&"decode-clip".to_string()));
}

#[test]
fn ignoring_max_size_fails() {
    let r = conformance::video_decoder(&FakeFactory(Fault::IgnoresMaxSize), &clip());
    assert!(failed(&r).contains(&"max-size".to_string()));
}
