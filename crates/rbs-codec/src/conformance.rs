//! Conformance suites for encoder adapters.
//!
//! An adapter crate runs these in `tests/conformance.rs`:
//!
//! ```ignore
//! let mut frames = SystemFrameSource::new();
//! rbs_codec::conformance::video_encoder(&MyFactory::default(), &mut frames).assert_passed();
//! ```
//!
//! The suites check the invariants of `docs/specs/output-ports.md` for every codec and
//! rate-control mode the adapter declares. Cases the caller cannot exercise (no
//! hardware, no frame source for the declared memory kind) are skipped, not failed.
//! The suites are test support: they allocate freely and are never used at runtime.

use rbs_core::capability::{CapabilitySet, CapabilityValue};
pub use rbs_core::conformance::{Case, Outcome, PROBE_BUDGET, Report};
use rbs_core::conformance::{guarded, probe_case, set_values};
use rbs_core::media::{
    AudioBlock, AudioFormat, BitstreamFormat, Bytes, CodecId, EncodedPacket, MediaKind, MemoryKind,
    PixelFormat, PlaneLayout, Size, StreamParams, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate, SampleRate};

use crate::caps;
use crate::config::{
    AudioEncoderConfig, EncoderChange, KeyframeInterval, RateControl, VideoEncoderConfig,
};
use crate::encoder::{AudioEncoder, AudioEncoderFactory, VideoEncoder, VideoEncoderFactory};
use crate::error::EncodeError;

/// Supplies input frames for the video suite in the memory kind the adapter takes.
///
/// [`SystemFrameSource`] covers system memory. GPU adapters supply a source backed by
/// their device (for example from `rbs-gpu` test helpers).
pub trait FrameSource {
    /// The memory kind of the frames this source produces.
    fn memory(&self) -> MemoryKind;

    /// True when the source can produce frames in `pixel_format`.
    fn supports(&self, pixel_format: PixelFormat) -> bool;

    /// A frame of `format` at `timestamp`. `index` varies the picture content so
    /// encoders see motion. `None` if the source cannot produce this format.
    fn frame(
        &mut self,
        format: &VideoFormat,
        index: u64,
        timestamp: Timestamp,
    ) -> Option<VideoFrame>;
}

/// Produces system-memory frames (NV12, I420, P010) with a moving gradient.
#[derive(Debug, Default)]
pub struct SystemFrameSource;

impl SystemFrameSource {
    /// A new source.
    pub fn new() -> Self {
        Self
    }
}

impl FrameSource for SystemFrameSource {
    fn memory(&self) -> MemoryKind {
        MemoryKind::System
    }

    fn supports(&self, pixel_format: PixelFormat) -> bool {
        matches!(
            pixel_format,
            PixelFormat::Nv12 | PixelFormat::I420 | PixelFormat::P010
        )
    }

    fn frame(
        &mut self,
        format: &VideoFormat,
        index: u64,
        timestamp: Timestamp,
    ) -> Option<VideoFrame> {
        let pf = format.pixel_format;
        if !self.supports(pf) {
            return None;
        }
        let Size { width, height } = format.size;
        let mut data = Vec::new();
        let mut planes = Vec::new();
        for plane in 0..pf.plane_count() {
            let stride = usize::try_from(pf.min_row_bytes(plane, width)?).ok()?;
            let rows = pf.plane_rows(plane, height)? as usize;
            planes.push(PlaneLayout {
                offset: data.len(),
                stride,
            });
            for row in 0..rows {
                for col in 0..stride {
                    let value = if plane == 0 {
                        (col + row + index as usize * 4) as u8
                    } else {
                        128
                    };
                    data.push(value);
                }
            }
        }
        let system = SystemFrame::new(*format, Bytes::from(data), &planes).ok()?;
        Some(VideoFrame::system(timestamp, system, None).with_sequence(index))
    }
}

fn range_pick(caps: &CapabilitySet, key: &rbs_core::capability::CapabilityKey, want: i64) -> i64 {
    match caps.get(key) {
        Some(CapabilityValue::IntRange { min, max }) => want.clamp(*min, *max),
        Some(CapabilityValue::Int(v)) => want.min(*v),
        _ => want,
    }
}

fn codec_by_name(name: &str, kind: MediaKind) -> Option<CodecId> {
    [
        CodecId::H264,
        CodecId::Hevc,
        CodecId::Av1,
        CodecId::Vp9,
        CodecId::Aac,
        CodecId::Opus,
    ]
    .into_iter()
    .find(|c| c.as_str() == name && c.media_kind() == kind)
}

fn pixel_format_by_name(name: &str) -> Option<PixelFormat> {
    [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010]
        .into_iter()
        .find(|p| p.as_str() == name)
}

const VIDEO_FRAMES: u64 = 45;
const KEYFRAME_EVERY: u32 = 15;
const REQUEST_KEYFRAME_AT: u64 = 22;
const CHANGE_BITRATE_AT: u64 = 30;
const VIDEO_ORIGIN: Timestamp = Timestamp::from_nanos(10_000_000_000);

/// Runs the video encoder suite against `factory`, using `frames` for input.
pub fn video_encoder(factory: &dyn VideoEncoderFactory, frames: &mut dyn FrameSource) -> Report {
    let mut report = Report::default();
    let Some(caps) = probe_case(&mut report, factory) else {
        return report;
    };

    let memory = frames.memory();
    let memories = set_values(&caps, &caps::INPUT_MEMORY);
    if !memories.iter().any(|m| m == memory.as_str()) {
        report.record(
            "input.memory",
            Outcome::Skipped(format!(
                "frame source gives {memory}, adapter takes {memories:?}"
            )),
        );
        return report;
    }
    let Some(pixel_format) = set_values(&caps, &caps::INPUT_PIXEL_FORMAT)
        .iter()
        .filter_map(|n| pixel_format_by_name(n))
        .find(|p| frames.supports(*p))
    else {
        report.record(
            "input.pixel_format",
            Outcome::Skipped("no declared pixel format the frame source can produce".into()),
        );
        return report;
    };

    let width = range_pick(&caps, &caps::VIDEO_WIDTH, 320) as u32 & !1;
    let height = range_pick(&caps, &caps::VIDEO_HEIGHT, 240) as u32 & !1;
    let format = VideoFormat::new(
        Size::new(width, height),
        pixel_format,
        rbs_core::media::ColorSpace::BT709_LIMITED,
    );
    let b_frames = match caps.get(&caps::VIDEO_B_FRAMES_MAX) {
        Some(CapabilityValue::Int(n)) => (*n).clamp(0, 2) as u32,
        _ => 0,
    };
    let reconfigure = set_values(&caps, &caps::RECONFIGURE);
    let rate_controls = set_values(&caps, &caps::VIDEO_RATE_CONTROL);
    if rate_controls.is_empty() {
        report.record(
            "codec.video.rate_control",
            Outcome::Failed("no rate-control mode declared".into()),
        );
    }

    let codec_names = set_values(&caps, &caps::VIDEO_ENCODE);
    if codec_names.is_empty() {
        report.record(
            "codec.video.encode",
            Outcome::Failed("no video codec declared".into()),
        );
    }
    for name in &codec_names {
        let Some(codec) = codec_by_name(name, MediaKind::Video) else {
            report.record(
                name.clone(),
                Outcome::Skipped("codec unknown to this suite".into()),
            );
            continue;
        };
        for rc_name in &rate_controls {
            let rate_control = match rc_name.as_str() {
                "cbr" => RateControl::Cbr {
                    bitrate_bps: 1_000_000,
                },
                "vbr" => RateControl::Vbr {
                    target_bps: 1_000_000,
                    max_bps: 2_000_000,
                },
                "constant_quality" => RateControl::ConstantQuality { level: 23 },
                other => {
                    report.record(
                        format!("{name}/{other}"),
                        Outcome::Failed("unknown rate-control name".into()),
                    );
                    continue;
                }
            };
            let config =
                VideoEncoderConfig::new(codec, format, FrameRate::FPS_30, memory, rate_control)
                    .with_keyframe_interval(KeyframeInterval::Frames(KEYFRAME_EVERY))
                    .with_max_b_frames(b_frames);
            let prefix = format!("{name}/{rc_name}");
            video_sequence(&mut report, &prefix, factory, frames, &config, &reconfigure);
        }
        // Invalid input is refused, using the first rate-control mode.
        if let Some(rc_name) = rate_controls.first() {
            let rate_control = match rc_name.as_str() {
                "constant_quality" => RateControl::ConstantQuality { level: 23 },
                _ => RateControl::Cbr {
                    bitrate_bps: 1_000_000,
                },
            };
            let config =
                VideoEncoderConfig::new(codec, format, FrameRate::FPS_30, memory, rate_control)
                    .with_keyframe_interval(KeyframeInterval::Frames(KEYFRAME_EVERY));
            invalid_input_case(&mut report, name, factory, frames, &config);
        }
    }
    report
}

fn video_sequence(
    report: &mut Report,
    prefix: &str,
    factory: &dyn VideoEncoderFactory,
    frames: &mut dyn FrameSource,
    config: &VideoEncoderConfig,
    reconfigure: &[String],
) {
    let mut encoder = match guarded(|| factory.create(config).map_err(|e| e.to_string())) {
        Ok(e) => e,
        Err(e) => {
            report.record(
                format!("{prefix}/create"),
                Outcome::Failed(format!("declared configuration refused: {e}")),
            );
            return;
        }
    };
    report.check(
        format!("{prefix}/stream"),
        guarded(|| check_video_stream(encoder.as_ref(), config)),
    );

    let cadence = Cadence::frames(VIDEO_ORIGIN, config.frame_rate);
    let mut packets = Vec::with_capacity(64);
    let mut keyframe_requested = false;
    let mut keyframe_case: Result<(), String> = Ok(());
    let mut bitrate_case: Result<(), String> = Ok(());
    let run = guarded(|| {
        for index in 0..VIDEO_FRAMES {
            if index == REQUEST_KEYFRAME_AT {
                match encoder.reconfigure(&EncoderChange::RequestKeyframe) {
                    Ok(()) => keyframe_requested = true,
                    Err(EncodeError::Unsupported) => {
                        if reconfigure.iter().any(|r| r == caps::RECONFIGURE_KEYFRAME) {
                            keyframe_case =
                                Err("declares keyframe requests but returned Unsupported".into());
                        }
                    }
                    Err(e) => keyframe_case = Err(format!("keyframe request failed: {e}")),
                }
            }
            if index == CHANGE_BITRATE_AT {
                let change = match config.rate_control {
                    RateControl::Cbr { bitrate_bps } => Some(RateControl::Cbr {
                        bitrate_bps: bitrate_bps / 2,
                    }),
                    RateControl::Vbr {
                        target_bps,
                        max_bps,
                    } => Some(RateControl::Vbr {
                        target_bps: target_bps / 2,
                        max_bps: max_bps / 2,
                    }),
                    RateControl::ConstantQuality { .. } => None,
                };
                if let Some(rc) = change {
                    bitrate_case = match encoder.reconfigure(&EncoderChange::Bitrate(rc)) {
                        Ok(()) => Ok(()),
                        Err(EncodeError::Unsupported)
                            if !reconfigure.iter().any(|r| r == caps::RECONFIGURE_BITRATE) =>
                        {
                            Ok(())
                        }
                        Err(e) => Err(format!("bitrate change failed: {e}")),
                    };
                }
            }
            let ts = cadence.time_of(index);
            let frame = frames
                .frame(&config.format, index, ts)
                .ok_or_else(|| "frame source returned no frame".to_string())?;
            match encoder.submit(&frame) {
                Ok(()) => {}
                Err(EncodeError::Busy) => {
                    encoder
                        .drain(&mut packets)
                        .map_err(|e| format!("drain: {e}"))?;
                    encoder
                        .submit(&frame)
                        .map_err(|e| format!("submit after drain: {e}"))?;
                }
                Err(e) => return Err(format!("submit frame {index}: {e}")),
            }
            encoder
                .drain(&mut packets)
                .map_err(|e| format!("drain: {e}"))?;
        }
        encoder
            .flush(&mut packets)
            .map_err(|e| format!("flush: {e}"))?;
        // A second flush (the node's final tick, then `release`) has nothing left.
        let flushed = packets.len();
        encoder
            .flush(&mut packets)
            .map_err(|e| format!("second flush: {e}"))?;
        if packets.len() != flushed {
            return Err("a second flush produced packets".into());
        }
        Ok(())
    });
    if let Err(e) = run {
        report.record(format!("{prefix}/encode"), Outcome::Failed(e));
        return;
    }

    let stream = encoder.stream().clone();
    report.check(
        format!("{prefix}/packets"),
        check_video_packets(&packets, &stream, config, &cadence),
    );
    let kf_indices = keyframe_indices(&packets, &cadence);
    report.check(
        format!("{prefix}/keyframe-interval"),
        check_keyframe_spacing(&kf_indices),
    );
    if keyframe_case.is_ok() && keyframe_requested {
        let window = REQUEST_KEYFRAME_AT..=REQUEST_KEYFRAME_AT + u64::from(config.max_b_frames);
        if !kf_indices.iter().any(|i| window.contains(i)) {
            keyframe_case = Err(format!(
                "no keyframe in frames {window:?} after request (keyframes at {kf_indices:?})"
            ));
        }
    }
    report.check(format!("{prefix}/request-keyframe"), keyframe_case);
    report.check(format!("{prefix}/reconfigure-bitrate"), bitrate_case);
}

fn check_video_stream(
    encoder: &dyn VideoEncoder,
    config: &VideoEncoderConfig,
) -> Result<(), String> {
    let s = encoder.stream();
    if s.codec != config.codec {
        return Err(format!("stream codec {} != {}", s.codec, config.codec));
    }
    match s.params {
        StreamParams::Video {
            format, frame_rate, ..
        } => {
            if format != config.format {
                return Err(format!("stream format {format:?} != {:?}", config.format));
            }
            if frame_rate != config.frame_rate {
                return Err(format!(
                    "stream frame rate {frame_rate:?} != {:?}",
                    config.frame_rate
                ));
            }
        }
        _ => return Err("video encoder reports a non-video stream".into()),
    }
    if matches!(s.bitstream, BitstreamFormat::LengthPrefixed { .. }) && s.extradata.is_empty() {
        return Err("length-prefixed bitstream without extradata (parameter sets)".into());
    }
    Ok(())
}

fn check_video_packets(
    packets: &[EncodedPacket],
    stream: &rbs_core::media::StreamDescriptor,
    config: &VideoEncoderConfig,
    cadence: &Cadence,
) -> Result<(), String> {
    if packets.len() as u64 != VIDEO_FRAMES {
        return Err(format!(
            "{} packets for {VIDEO_FRAMES} frames",
            packets.len()
        ));
    }
    let first = &packets[0];
    if !first.keyframe() {
        return Err("first packet is not a keyframe".into());
    }
    let mut last_dts = None;
    for (i, p) in packets.iter().enumerate() {
        if p.codec() != config.codec || p.bitstream() != stream.bitstream {
            return Err(format!(
                "packet {i}: codec or bitstream differs from stream()"
            ));
        }
        if p.dts() > p.pts() {
            return Err(format!("packet {i}: dts {:?} > pts {:?}", p.dts(), p.pts()));
        }
        if last_dts.is_some_and(|d| p.dts() < d) {
            return Err(format!("packet {i}: dts goes backwards"));
        }
        if p.data().is_empty() {
            return Err(format!("packet {i}: empty payload"));
        }
        last_dts = Some(p.dts());
    }
    let mut pts: Vec<Timestamp> = packets.iter().map(|p| p.pts()).collect();
    pts.sort();
    let expected: Vec<Timestamp> = (0..VIDEO_FRAMES).map(|i| cadence.time_of(i)).collect();
    if pts != expected {
        return Err("packet pts values are not the submitted frame timestamps".into());
    }
    Ok(())
}

fn keyframe_indices(packets: &[EncodedPacket], cadence: &Cadence) -> Vec<u64> {
    let mut out: Vec<u64> = packets
        .iter()
        .filter(|p| p.keyframe())
        .filter_map(|p| cadence.index_at_or_before(p.pts()))
        .collect();
    out.sort_unstable();
    out
}

fn check_keyframe_spacing(indices: &[u64]) -> Result<(), String> {
    let mut previous = 0;
    for &i in indices.iter().chain(std::iter::once(&VIDEO_FRAMES)) {
        if i - previous > u64::from(KEYFRAME_EVERY) {
            return Err(format!(
                "gap of {} frames between keyframes (interval {KEYFRAME_EVERY}); keyframes at {indices:?}",
                i - previous
            ));
        }
        previous = i;
    }
    Ok(())
}

fn invalid_input_case(
    report: &mut Report,
    name: &str,
    factory: &dyn VideoEncoderFactory,
    frames: &mut dyn FrameSource,
    config: &VideoEncoderConfig,
) {
    let size = config.format.size;
    let wrong_size = VideoFormat::new(
        Size::new(size.width + 16, size.height + 16),
        config.format.pixel_format,
        config.format.color,
    );
    let other_format = [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010]
        .into_iter()
        .find(|p| *p != config.format.pixel_format && frames.supports(*p));
    let mut variants = vec![("invalid-size", Some(wrong_size))];
    variants.push((
        "invalid-pixel-format",
        other_format.map(|p| VideoFormat::new(size, p, config.format.color)),
    ));
    for (label, wrong) in variants {
        let case = format!("{name}/{label}");
        let Some(frame) = wrong.and_then(|w| frames.frame(&w, 0, VIDEO_ORIGIN)) else {
            report.record(
                case,
                Outcome::Skipped("frame source cannot produce a mismatching frame".into()),
            );
            continue;
        };
        let result = guarded(|| {
            let mut encoder = factory.create(config).map_err(|e| e.to_string())?;
            match encoder.submit(&frame) {
                Err(EncodeError::InvalidInput(_)) => Ok(()),
                Ok(()) => Err("a frame that does not match the configuration was accepted".into()),
                Err(e) => Err(format!("expected InvalidInput, got {e}")),
            }
        });
        report.check(case, result);
    }
}

const AUDIO_ORIGIN: Timestamp = Timestamp::from_nanos(5_000_000_000);
const AUDIO_BLOCK: usize = 480;
const AUDIO_BLOCKS: usize = 100;

/// Runs the audio encoder suite against `factory` with generated 48 kHz stereo input.
pub fn audio_encoder(factory: &dyn AudioEncoderFactory) -> Report {
    let mut report = Report::default();
    let Some(caps) = probe_case(&mut report, factory) else {
        return report;
    };
    let rates = set_values(&caps, &caps::AUDIO_SAMPLE_RATES);
    if !rates.is_empty() && !rates.iter().any(|r| r == "48000") {
        report.record(
            "sample-rate",
            Outcome::Skipped("adapter does not declare 48000 Hz".into()),
        );
        return report;
    }
    let codec_names = set_values(&caps, &caps::AUDIO_ENCODE);
    if codec_names.is_empty() {
        report.record(
            "codec.audio.encode",
            Outcome::Failed("no audio codec declared".into()),
        );
    }
    for name in &codec_names {
        let Some(codec) = codec_by_name(name, MediaKind::Audio) else {
            report.record(
                name.clone(),
                Outcome::Skipped("codec unknown to this suite".into()),
            );
            continue;
        };
        let config = AudioEncoderConfig::new(codec, AudioFormat::STEREO_48K, 128_000);
        audio_sequence(&mut report, name, factory, &config);
    }
    report
}

fn audio_sequence(
    report: &mut Report,
    name: &str,
    factory: &dyn AudioEncoderFactory,
    config: &AudioEncoderConfig,
) {
    let mut encoder = match guarded(|| factory.create(config).map_err(|e| e.to_string())) {
        Ok(e) => e,
        Err(e) => {
            report.record(
                format!("{name}/create"),
                Outcome::Failed(format!("declared configuration refused: {e}")),
            );
            return;
        }
    };
    report.check(
        format!("{name}/stream"),
        guarded(|| check_audio_stream(encoder.as_ref(), config)),
    );

    let rate = config.format.sample_rate;
    let mut left = vec![0.0_f32; AUDIO_BLOCK];
    let mut right = vec![0.0_f32; AUDIO_BLOCK];
    let mut packets = Vec::with_capacity(128);
    let run = guarded(|| {
        for block in 0..AUDIO_BLOCKS {
            for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
                let n = (block * AUDIO_BLOCK + i) as f32;
                *l = (n * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
                *r = *l;
            }
            let ts = AUDIO_ORIGIN + rate.duration_of((block * AUDIO_BLOCK) as u64);
            let audio = AudioBlock::planar(config.format, ts, &[&left, &right])
                .map_err(|e| e.to_string())?;
            encoder
                .submit(&audio)
                .map_err(|e| format!("submit block {block}: {e}"))?;
            encoder
                .drain(&mut packets)
                .map_err(|e| format!("drain: {e}"))?;
        }
        encoder
            .flush(&mut packets)
            .map_err(|e| format!("flush: {e}"))?;
        Ok(())
    });
    if let Err(e) = run {
        report.record(format!("{name}/encode"), Outcome::Failed(e));
        return;
    }
    let frame_size = encoder.frame_size();
    report.check(
        format!("{name}/packets"),
        check_audio_packets(&packets, config, frame_size, rate),
    );
}

fn check_audio_stream(
    encoder: &dyn AudioEncoder,
    config: &AudioEncoderConfig,
) -> Result<(), String> {
    let s = encoder.stream();
    if s.codec != config.codec {
        return Err(format!("stream codec {} != {}", s.codec, config.codec));
    }
    match s.params {
        StreamParams::Audio {
            format, frame_size, ..
        } => {
            if format != config.format {
                return Err(format!("stream format {format:?} != {:?}", config.format));
            }
            if frame_size != encoder.frame_size() || frame_size == 0 {
                return Err(format!(
                    "stream frame_size {frame_size} != frame_size() {}",
                    encoder.frame_size()
                ));
            }
        }
        _ => return Err("audio encoder reports a non-audio stream".into()),
    }
    let needs_extradata = match s.codec {
        CodecId::Aac => s.bitstream != BitstreamFormat::Adts,
        CodecId::Opus => true,
        _ => false,
    };
    if needs_extradata && s.extradata.is_empty() {
        return Err("missing extradata (AudioSpecificConfig / OpusHead)".into());
    }
    Ok(())
}

fn check_audio_packets(
    packets: &[EncodedPacket],
    config: &AudioEncoderConfig,
    frame_size: u32,
    rate: SampleRate,
) -> Result<(), String> {
    let submitted = (AUDIO_BLOCK * AUDIO_BLOCKS) as u64;
    let needed = submitted.div_ceil(u64::from(frame_size.max(1)));
    if (packets.len() as u64) < needed {
        return Err(format!(
            "{} packets cover fewer than {submitted} samples (frame size {frame_size})",
            packets.len()
        ));
    }
    for (i, p) in packets.iter().enumerate() {
        if p.codec() != config.codec {
            return Err(format!("packet {i}: wrong codec"));
        }
        let expected = AUDIO_ORIGIN + rate.duration_of(i as u64 * u64::from(frame_size));
        let error = p.pts().as_nanos().abs_diff(expected.as_nanos());
        if error > 1_000 {
            return Err(format!(
                "packet {i}: pts {:?}, expected {expected:?} (first block time + {i} x {frame_size} samples)",
                p.pts()
            ));
        }
        if p.dts() > p.pts() {
            return Err(format!("packet {i}: dts > pts"));
        }
    }
    Ok(())
}
