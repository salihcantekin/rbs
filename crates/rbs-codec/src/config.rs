//! Encoder configuration and runtime changes.

use std::time::Duration;

use rbs_core::media::{AudioFormat, CodecId, MemoryKind, VideoFormat};
use rbs_core::timing::FrameRate;

/// How the encoder spends bits.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate, for streaming.
    Cbr {
        /// Target bits per second.
        bitrate_bps: u64,
    },
    /// Constant bitrate padded with filler data, so that every second carries the full
    /// bitrate even when the picture needs fewer bits (some ingest servers and players
    /// expect it). Only adapters that list `cbr_padded` in `codec.video.rate_control`
    /// take it.
    CbrPadded {
        /// Target bits per second, reached with filler data when needed.
        bitrate_bps: u64,
    },
    /// Variable bitrate with a ceiling.
    Vbr {
        /// Average bits per second.
        target_bps: u64,
        /// Peak bits per second.
        max_bps: u64,
    },
    /// Constant quality, for recording. The adapter maps the level to its own mode
    /// (CQP, CRF, CQ).
    ConstantQuality {
        /// 0 (best) ..= 51 (worst).
        level: u8,
    },
}

impl RateControl {
    /// Stable name used in the `codec.video.rate_control` capability.
    pub const fn name(&self) -> &'static str {
        match self {
            RateControl::Cbr { .. } => "cbr",
            RateControl::CbrPadded { .. } => "cbr_padded",
            RateControl::Vbr { .. } => "vbr",
            RateControl::ConstantQuality { .. } => "constant_quality",
        }
    }
}

/// How often the encoder inserts a keyframe.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeInterval {
    /// Every `n` frames.
    Frames(u32),
    /// At least once per duration (streaming services typically require 2 s).
    Duration(Duration),
}

impl KeyframeInterval {
    /// The interval in frames at `rate`, rounded down, at least 1.
    pub fn frames_at(&self, rate: FrameRate) -> u32 {
        match *self {
            KeyframeInterval::Frames(n) => n.max(1),
            KeyframeInterval::Duration(d) => {
                let per_second = rate.per_second();
                let frames = d.as_nanos() * u128::from(per_second.num())
                    / (1_000_000_000 * u128::from(per_second.den()));
                u32::try_from(frames).unwrap_or(u32::MAX).max(1)
            }
        }
    }
}

/// Speed versus quality trade-off, mapped by each adapter to its own presets.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderPreset {
    /// Best quality the machine can sustain in real time.
    Quality,
    /// The default.
    #[default]
    Balanced,
    /// Least resource use.
    Speed,
}

/// Vendor-neutral tuning. Build with `EncoderTuning::default()` and set fields.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderTuning {
    /// Speed versus quality.
    pub preset: EncoderPreset,
    /// Minimise latency (no look-ahead, no frame reordering beyond `max_b_frames`).
    pub low_latency: bool,
}

/// Everything needed to create a video encoder session. Build with
/// [`VideoEncoderConfig::new`] and the `with_*` methods.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncoderConfig {
    /// Output codec.
    pub codec: CodecId,
    /// Input picture format: size, pixel format (for example NV12, P010, I420) and the
    /// colour description to signal in the bitstream.
    pub format: VideoFormat,
    /// Nominal frame rate.
    pub frame_rate: FrameRate,
    /// Where input frames live.
    pub memory: MemoryKind,
    /// Rate control.
    pub rate_control: RateControl,
    /// Keyframe spacing.
    pub keyframe_interval: KeyframeInterval,
    /// Most B-frames between references; 0 for lowest latency.
    pub max_b_frames: u32,
    /// Codec profile name, for example `high` or `main10`; `None` lets the adapter pick.
    pub profile: Option<String>,
    /// Speed and latency tuning.
    pub tuning: EncoderTuning,
    /// Opaque adapter-specific options. The engine never reads them (principle P3).
    pub adapter_options: Vec<(String, String)>,
}

/// Everything needed to create an audio encoder session. Build with
/// [`AudioEncoderConfig::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEncoderConfig {
    /// Output codec.
    pub codec: CodecId,
    /// Sample rate and channel layout of the input and output.
    pub format: AudioFormat,
    /// Target bits per second.
    pub bitrate_bps: u64,
    /// Opaque adapter-specific options. The engine never reads them (principle P3).
    pub adapter_options: Vec<(String, String)>,
}

impl VideoEncoderConfig {
    /// A configuration with a 2 s keyframe interval, no B-frames, no profile, default
    /// tuning and no adapter options.
    pub fn new(
        codec: CodecId,
        format: VideoFormat,
        frame_rate: FrameRate,
        memory: MemoryKind,
        rate_control: RateControl,
    ) -> Self {
        Self {
            codec,
            format,
            frame_rate,
            memory,
            rate_control,
            keyframe_interval: KeyframeInterval::Duration(Duration::from_secs(2)),
            max_b_frames: 0,
            profile: None,
            tuning: EncoderTuning::default(),
            adapter_options: Vec::new(),
        }
    }

    /// Sets the keyframe interval.
    pub fn with_keyframe_interval(mut self, interval: KeyframeInterval) -> Self {
        self.keyframe_interval = interval;
        self
    }

    /// Sets the most B-frames between references.
    pub fn with_max_b_frames(mut self, max_b_frames: u32) -> Self {
        self.max_b_frames = max_b_frames;
        self
    }

    /// Sets the codec profile name.
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Sets the tuning.
    pub fn with_tuning(mut self, tuning: EncoderTuning) -> Self {
        self.tuning = tuning;
        self
    }

    /// Adds an opaque adapter-specific option.
    pub fn with_adapter_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.adapter_options.push((key.into(), value.into()));
        self
    }
}

impl AudioEncoderConfig {
    /// A configuration with no adapter options.
    pub fn new(codec: CodecId, format: AudioFormat, bitrate_bps: u64) -> Self {
        Self {
            codec,
            format,
            bitrate_bps,
            adapter_options: Vec::new(),
        }
    }

    /// Adds an opaque adapter-specific option.
    pub fn with_adapter_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.adapter_options.push((key.into(), value.into()));
        self
    }
}

/// A change applied to a running encoder. Changes never alter `stream()`; a change
/// that would (codec, size, profile) requires a new output chain.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChange {
    /// New rate control settings (typically a new bitrate from adaptive streaming).
    Bitrate(RateControl),
    /// Make the next frame a keyframe.
    RequestKeyframe,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_interval_in_frames() {
        let two_s = KeyframeInterval::Duration(Duration::from_secs(2));
        assert_eq!(two_s.frames_at(FrameRate::FPS_60), 120);
        assert_eq!(two_s.frames_at(FrameRate::FPS_29_97), 59);
        assert_eq!(KeyframeInterval::Frames(0).frames_at(FrameRate::FPS_30), 1);
        assert_eq!(
            KeyframeInterval::Duration(Duration::ZERO).frames_at(FrameRate::FPS_30),
            1
        );
    }

    #[test]
    fn rate_control_names_match_capability_values() {
        let modes = [
            RateControl::Cbr { bitrate_bps: 1 },
            RateControl::CbrPadded { bitrate_bps: 1 },
            RateControl::Vbr {
                target_bps: 1,
                max_bps: 2,
            },
            RateControl::ConstantQuality { level: 20 },
        ];
        let names: Vec<_> = modes.iter().map(RateControl::name).collect();
        assert_eq!(names, crate::caps::RATE_CONTROL_NAMES);
    }
}
