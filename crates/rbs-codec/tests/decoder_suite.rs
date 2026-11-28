//! Checks the live decoder suite: a correct fake decoder passes, and one that breaks an
//! invariant fails with the right case.
#![allow(clippy::expect_used)]

use std::collections::VecDeque;

use rbs_codec::caps;
use rbs_codec::conformance::{self, DecodeClip};
use rbs_codec::{
    CreateError, DecoderStats, VideoDecodeError, VideoDecoder, VideoDecoderConfig,
    VideoDecoderFactory,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, PlaneLayout, Size,
    SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

const SIZE: Size = Size::new(64, 48);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    DecodesBeforeKeyframe,
    WrongPts,
    IgnoresMaxSize,
    RoundsPtsTo100ns,
    FlushKeepsSession,
    ResetDoesNotGate,
    AcceptsForeignCodec,
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
            have_size: false,
            pictures: 0,
            queue: VecDeque::new(),
        }))
    }
}

struct Fake {
    fault: Fault,
    max: Size,
    have_key: bool,
    have_size: bool,
    pictures: u64,
    queue: VecDeque<VideoFrame>,
}

impl VideoDecoder for Fake {
    fn output_format(&self) -> Option<VideoFormat> {
        self.have_size
            .then(|| VideoFormat::new(SIZE, PixelFormat::Nv12, ColorSpace::BT709_LIMITED))
    }
    fn submit(&mut self, p: &EncodedPacket) -> Result<(), VideoDecodeError> {
        if p.codec() != CodecId::H264 && self.fault != Fault::AcceptsForeignCodec {
            return Err(VideoDecodeError::InvalidInput("codec".into()));
        }
        if p.keyframe() {
            // The fake bitstream: width and height as two big-endian u16.
            let d = p.data();
            if d.len() < 4 {
                return Err(VideoDecodeError::InvalidInput("short".into()));
            }
            let (w, h) = (
                u32::from(d[0]) << 8 | u32::from(d[1]),
                u32::from(d[2]) << 8 | u32::from(d[3]),
            );
            if self.fault != Fault::IgnoresMaxSize && (w > self.max.width || h > self.max.height) {
                return Err(VideoDecodeError::InvalidInput("too large".into()));
            }
            self.have_key = true;
            self.have_size = true;
        } else if !self.have_key && self.fault != Fault::DecodesBeforeKeyframe {
            return Err(VideoDecodeError::NeedKeyframe);
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
        let pts = match self.fault {
            Fault::WrongPts => Timestamp::from_nanos(p.pts().as_nanos() + 1),
            Fault::RoundsPtsTo100ns => Timestamp::from_nanos(p.pts().as_nanos() / 100 * 100),
            _ => p.pts(),
        };
        self.pictures += 1;
        self.queue.push_back(VideoFrame::system(pts, frame, None));
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        out.extend(self.queue.drain(..));
        Ok(())
    }
    fn stats(&self) -> DecoderStats {
        let mut s = DecoderStats::default();
        s.pictures = self.pictures;
        s.need_keyframe = !self.have_key;
        s
    }
    fn reset(&mut self) -> Result<(), VideoDecodeError> {
        self.queue.clear();
        self.have_key = self.fault == Fault::ResetDoesNotGate && self.have_key;
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        self.drain(out)?;
        if self.fault != Fault::FlushKeepsSession {
            self.have_key = false;
        }
        Ok(())
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
            let t = Timestamp::from_nanos(11_111_111 * (i + 1) + 7);
            EncodedPacket::new(CodecId::H264, BitstreamFormat::AnnexB, t, t, i == 0, data)
                .expect("packet")
        })
        .collect();
    DecodeClip {
        codec: CodecId::H264,
        packets,
        size: SIZE,
        lookahead: 0,
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
        "stats",
        "output-format",
        "max-surfaces",
        "hostile-input",
        "decode-after-hostile-input",
        "no-lookahead",
        "flush-gates-and-second-flush-empty",
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

fn assert_fails(fault: Fault, case: &str) {
    let r = conformance::video_decoder(&FakeFactory(fault), &clip());
    assert!(
        failed(&r).contains(&case.to_string()),
        "{case}: {:?}",
        failed(&r)
    );
}

#[test]
fn rounding_the_timestamp_to_100ns_fails() {
    assert_fails(Fault::RoundsPtsTo100ns, "decode-clip");
}

#[test]
fn a_flush_that_keeps_the_session_open_fails() {
    assert_fails(
        Fault::FlushKeepsSession,
        "flush-gates-and-second-flush-empty",
    );
}

#[test]
fn a_reset_that_does_not_gate_fails() {
    assert_fails(Fault::ResetDoesNotGate, "reset-then-keyframe");
}

#[test]
fn accepting_a_foreign_codec_fails() {
    assert_fails(Fault::AcceptsForeignCodec, "foreign-codec");
}
