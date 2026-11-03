//! Checks the conformance suites themselves: a correct fake adapter passes, and
//! adapters that break one invariant each fail with the right case.

use std::collections::VecDeque;

use rbs_codec::caps;
use rbs_codec::conformance::{self, Outcome, Report, SystemFrameSource};
use rbs_codec::{
    AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError, EncoderChange,
    KeyframeInterval, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    AudioBlock, BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor, StreamParams,
    VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

/// Which invariant the fake video encoder breaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    NoInitialKeyframe,
    ShiftedPts,
    AcceptsWrongSize,
    IgnoresKeyframeRequest,
    DropsLastFrame,
    Panics,
    /// Accepts CBR padding without declaring it, and with any rate-control mode.
    AcceptsCbrPadding,
}

struct FakeVideoFactory {
    fault: Fault,
    /// Declares and honours `codec.video.cbr_padding`.
    padding: bool,
}

fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| v.to_string()).collect())
}

impl AdapterFactory for FakeVideoFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.codec.fake-video")
    }
    fn display_name(&self) -> &str {
        "fake video encoder"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::HARDWARE, CapabilityValue::Flag(false))
            .with(caps::VIDEO_ENCODE, set(&["h264", "hevc"]))
            .with(caps::VIDEO_RATE_CONTROL, set(&["cbr", "constant_quality"]))
            .with(caps::INPUT_MEMORY, set(&["system"]))
            .with(caps::INPUT_PIXEL_FORMAT, set(&["nv12"]))
            .with(
                caps::VIDEO_WIDTH,
                CapabilityValue::IntRange { min: 16, max: 4096 },
            )
            .with(
                caps::VIDEO_HEIGHT,
                CapabilityValue::IntRange { min: 16, max: 4096 },
            )
            .with(
                caps::RECONFIGURE,
                set(&[caps::RECONFIGURE_BITRATE, caps::RECONFIGURE_KEYFRAME]),
            )
            .with(caps::VIDEO_CBR_PADDING, CapabilityValue::Flag(self.padding)))
    }
}

impl VideoEncoderFactory for FakeVideoFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        if self.fault != Fault::AcceptsCbrPadding {
            config
                .check_cbr_padding()
                .map_err(CreateError::InvalidConfig)?;
            if config.cbr_padding && !self.padding {
                return Err(CreateError::Unsupported {
                    what: "CBR padding".into(),
                });
            }
        }
        let interval = config.keyframe_interval.frames_at(config.frame_rate);
        Ok(Box::new(FakeVideo {
            fault: self.fault,
            config: config.clone(),
            interval,
            stream: StreamDescriptor::new(
                config.codec,
                BitstreamFormat::LengthPrefixed { length_size: 4 },
                Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff]),
                StreamParams::video(config.format, config.frame_rate),
            ),
            since_keyframe: None,
            force_keyframe: false,
            pending: VecDeque::new(),
            submitted: 0,
        }))
    }
}

struct FakeVideo {
    fault: Fault,
    config: VideoEncoderConfig,
    interval: u32,
    stream: StreamDescriptor,
    since_keyframe: Option<u32>,
    force_keyframe: bool,
    pending: VecDeque<EncodedPacket>,
    submitted: u64,
}

impl VideoEncoder for FakeVideo {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        if self.fault == Fault::Panics {
            panic!("fake encoder panic");
        }
        if *frame.format() != self.config.format && self.fault != Fault::AcceptsWrongSize {
            return Err(EncodeError::InvalidInput("format mismatch".into()));
        }
        self.submitted += 1;
        let keyframe = match self.since_keyframe {
            None => self.fault != Fault::NoInitialKeyframe,
            Some(n) => self.force_keyframe || n + 1 >= self.interval,
        };
        self.force_keyframe = false;
        self.since_keyframe = Some(if keyframe {
            0
        } else {
            self.since_keyframe.map_or(0, |n| n + 1)
        });
        let pts = if self.fault == Fault::ShiftedPts {
            Timestamp::from_nanos(frame.timestamp().as_nanos() + 1)
        } else {
            frame.timestamp()
        };
        self.pending.push_back(
            EncodedPacket::new(
                self.config.codec,
                self.stream.bitstream,
                pts,
                pts,
                keyframe,
                Bytes::from_static(b"fake"),
            )
            .map_err(|e| EncodeError::Failed {
                reason: e.to_string(),
                recoverable: false,
            })?,
        );
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        // Hold one packet back to look like a real pipeline.
        while self.pending.len() > 1 {
            if let Some(p) = self.pending.pop_front() {
                out.push(p);
            }
        }
        Ok(())
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        match change {
            EncoderChange::RequestKeyframe => {
                if self.fault != Fault::IgnoresKeyframeRequest {
                    self.force_keyframe = true;
                }
                Ok(())
            }
            EncoderChange::Bitrate(rc) => {
                self.config.rate_control = *rc;
                Ok(())
            }
            _ => Err(EncodeError::Unsupported),
        }
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        if self.fault == Fault::DropsLastFrame {
            self.pending.pop_back();
        }
        out.extend(self.pending.drain(..));
        Ok(())
    }
}

fn run(fault: Fault) -> Report {
    let factory = FakeVideoFactory {
        fault,
        padding: false,
    };
    conformance::video_encoder(&factory, &mut SystemFrameSource::new())
}

fn failed_cases(report: &Report) -> Vec<String> {
    report.failures().map(|c| c.name.clone()).collect()
}

#[test]
fn correct_video_encoder_passes() {
    let report = run(Fault::None);
    report.assert_ran(&["h264/cbr/packets", "hevc/constant_quality/request-keyframe"]);
    assert!(report.skipped().next().is_none(), "{report}");
    report.assert_ran(&["h264/cbr-padding-refused", "hevc/cbr-padding-needs-cbr"]);
    // probe + per codec: 2 rate controls x 5 cases + 2 padding cases + 2 invalid-input
    // cases.
    assert_eq!(report.cases.len(), 1 + 2 * (2 * 5 + 2 + 2), "{report}");
}

#[test]
fn declared_cbr_padding_is_encoded() {
    let factory = FakeVideoFactory {
        fault: Fault::None,
        padding: true,
    };
    let report = conformance::video_encoder(&factory, &mut SystemFrameSource::new());
    report.assert_passed();
    report.assert_ran(&["h264/cbr-padding/packets", "hevc/cbr-padding-needs-cbr"]);
    // The refusal case is replaced by a 5-case encode.
    assert_eq!(report.cases.len(), 1 + 2 * (2 * 5 + 5 + 1 + 2), "{report}");
}

#[test]
fn each_video_fault_is_caught() {
    let expectations = [
        (Fault::NoInitialKeyframe, "h264/cbr/packets"),
        (Fault::ShiftedPts, "h264/cbr/packets"),
        (Fault::AcceptsWrongSize, "h264/invalid-size"),
        (Fault::AcceptsWrongSize, "h264/invalid-pixel-format"),
        (Fault::IgnoresKeyframeRequest, "h264/cbr/request-keyframe"),
        (Fault::DropsLastFrame, "h264/cbr/packets"),
        (Fault::Panics, "h264/cbr/encode"),
        (Fault::AcceptsCbrPadding, "h264/cbr-padding-refused"),
        (Fault::AcceptsCbrPadding, "h264/cbr-padding-needs-cbr"),
    ];
    for (fault, case) in expectations {
        let report = run(fault);
        let failed = failed_cases(&report);
        assert!(
            failed.iter().any(|c| c == case),
            "{fault:?}: expected {case} to fail, got {failed:?}\n{report}"
        );
    }
}

struct Unavailable;

impl AdapterFactory for Unavailable {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.codec.unavailable")
    }
    fn display_name(&self) -> &str {
        "no hardware"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new())
    }
}

impl VideoEncoderFactory for Unavailable {
    fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        Err(CreateError::Device("no hardware".into()))
    }
}

#[test]
fn missing_hardware_skips_instead_of_failing() {
    let report = conformance::video_encoder(&Unavailable, &mut SystemFrameSource::new());
    assert!(report.is_success(), "{report}");
    assert_eq!(report.skipped().count(), 1);
}

#[test]
fn keyframe_interval_helper_is_used_by_fake() {
    let interval = KeyframeInterval::Frames(15);
    assert_eq!(interval.frames_at(rbs_core::timing::FrameRate::FPS_30), 15);
}

/// AAC-like fake: 1024-sample frames, pts from the first block.
struct FakeAudioFactory {
    shift_pts: bool,
}

impl AdapterFactory for FakeAudioFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.codec.fake-audio")
    }
    fn display_name(&self) -> &str {
        "fake audio encoder"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::AUDIO_ENCODE, set(&["aac"]))
            .with(caps::AUDIO_SAMPLE_RATES, set(&["44100", "48000"])))
    }
}

impl AudioEncoderFactory for FakeAudioFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(FakeAudio {
            shift_pts: self.shift_pts,
            config: config.clone(),
            stream: StreamDescriptor::new(
                config.codec,
                BitstreamFormat::Raw,
                Bytes::from_static(&[0x11, 0x90]),
                StreamParams::audio(config.format, 1024),
            ),
            origin: None,
            buffered: 0,
            emitted: 0,
            pending: Vec::new(),
        }))
    }
}

struct FakeAudio {
    shift_pts: bool,
    config: AudioEncoderConfig,
    stream: StreamDescriptor,
    origin: Option<Timestamp>,
    buffered: u64,
    emitted: u64,
    pending: Vec<EncodedPacket>,
}

impl FakeAudio {
    fn emit(&mut self) {
        let Some(origin) = self.origin else { return };
        let rate = self.config.format.sample_rate;
        let mut pts = origin + rate.duration_of(self.emitted * 1024);
        if self.shift_pts && self.emitted > 3 {
            pts = pts + std::time::Duration::from_millis(1);
        }
        self.pending.push(
            EncodedPacket::new(
                self.config.codec,
                BitstreamFormat::Raw,
                pts,
                pts,
                true,
                Bytes::from_static(b"aac"),
            )
            .unwrap_or_else(|_| unreachable!("dts == pts"))
            .with_duration(rate.duration_of(1024)),
        );
        self.emitted += 1;
    }
}

impl AudioEncoder for FakeAudio {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn frame_size(&self) -> u32 {
        1024
    }
    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.config.format {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        self.origin.get_or_insert(block.timestamp());
        self.buffered += block.frames() as u64;
        while self.buffered >= 1024 {
            self.buffered -= 1024;
            self.emit();
        }
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.pending);
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Err(EncodeError::Unsupported)
    }
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        if self.buffered > 0 {
            self.buffered = 0;
            self.emit();
        }
        self.drain(out)
    }
}

#[test]
fn correct_audio_encoder_passes() {
    let report = conformance::audio_encoder(&FakeAudioFactory { shift_pts: false });
    report.assert_passed();
    assert_eq!(report.cases.len(), 3, "{report}");
}

#[test]
fn audio_timestamp_drift_is_caught() {
    let report = conformance::audio_encoder(&FakeAudioFactory { shift_pts: true });
    assert!(
        report
            .cases
            .iter()
            .any(|c| c.name == "aac/packets" && matches!(c.outcome, Outcome::Failed(_))),
        "{report}"
    );
}

#[test]
fn codec_id_names_match_capability_values() {
    assert_eq!(CodecId::H264.as_str(), "h264");
}
