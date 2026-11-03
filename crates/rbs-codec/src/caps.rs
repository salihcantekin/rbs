//! Well-known capability keys of encoder adapters.
//!
//! Values use the stable names of `rbs_core::media` types (`CodecId::as_str`,
//! `MemoryKind::as_str`, `PixelFormat::as_str`). See `docs/specs/output-ports.md`.

use rbs_core::capability::CapabilityKey;

pub use rbs_core::registry::HARDWARE;

/// Set: video codecs the adapter encodes (`h264`, `hevc`, `av1`, `vp9`).
pub const VIDEO_ENCODE: CapabilityKey = CapabilityKey::new_static("codec.video.encode");
/// Set: audio codecs the adapter encodes (`aac`, `opus`).
pub const AUDIO_ENCODE: CapabilityKey = CapabilityKey::new_static("codec.audio.encode");
/// Set: `<codec>.<profile>` names, for example `h264.high`, `hevc.main10`.
pub const VIDEO_PROFILES: CapabilityKey = CapabilityKey::new_static("codec.video.profiles");
/// Set: bit depths, `8` and/or `10`.
pub const VIDEO_BIT_DEPTH: CapabilityKey = CapabilityKey::new_static("codec.video.bit_depth");
/// IntRange: supported picture widths.
pub const VIDEO_WIDTH: CapabilityKey = CapabilityKey::new_static("codec.video.width");
/// IntRange: supported picture heights.
pub const VIDEO_HEIGHT: CapabilityKey = CapabilityKey::new_static("codec.video.height");
/// Int: highest frame rate at the largest size.
pub const VIDEO_FPS_MAX: CapabilityKey = CapabilityKey::new_static("codec.video.fps.max");
/// Set: rate-control modes, see [`RATE_CONTROL_NAMES`].
pub const VIDEO_RATE_CONTROL: CapabilityKey = CapabilityKey::new_static("codec.video.rate_control");
/// Int: most B-frames between references; 0 when not supported.
pub const VIDEO_B_FRAMES_MAX: CapabilityKey = CapabilityKey::new_static("codec.video.b_frames.max");
/// Set: runtime changes honoured, see [`RECONFIGURE_BITRATE`] and [`RECONFIGURE_KEYFRAME`].
pub const RECONFIGURE: CapabilityKey = CapabilityKey::new_static("codec.reconfigure");
/// Int: concurrent sessions; absent means unlimited.
pub const SESSIONS_MAX: CapabilityKey = CapabilityKey::new_static("codec.sessions.max");
/// Set: input memory kinds (`MemoryKind` names).
pub const INPUT_MEMORY: CapabilityKey = CapabilityKey::new_static("input.memory");
/// Set: input pixel formats (`PixelFormat` names).
pub const INPUT_PIXEL_FORMAT: CapabilityKey = CapabilityKey::new_static("input.pixel_format");
/// Set: audio sample rates in Hz, as decimal strings.
pub const AUDIO_SAMPLE_RATES: CapabilityKey = CapabilityKey::new_static("codec.audio.sample_rates");
/// Int: most audio channels.
pub const AUDIO_CHANNELS_MAX: CapabilityKey = CapabilityKey::new_static("codec.audio.channels.max");

/// Value of [`RECONFIGURE`]: bitrate changes while running.
pub const RECONFIGURE_BITRATE: &str = "bitrate";
/// Value of [`RECONFIGURE`]: keyframe on request.
pub const RECONFIGURE_KEYFRAME: &str = "keyframe";

/// Values of [`VIDEO_RATE_CONTROL`], matching [`crate::RateControl::name`].
pub const RATE_CONTROL_NAMES: [&str; 4] = ["cbr", "cbr_padded", "vbr", "constant_quality"];
