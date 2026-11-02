//! Encoder port: the contract every video and audio encoder adapter implements.
//!
//! Adapters (FFmpeg, NVENC, AMF, QSV, VideoToolbox, VA-API, x264, OpenH264, Opus, ...)
//! live in their own crates, implement [`VideoEncoderFactory`] or
//! [`AudioEncoderFactory`], declare what they can do with the keys in [`caps`], and run
//! the suites in `conformance` (feature `conformance`) in their tests.
//!
//! Contract: `docs/specs/output-ports.md`. Decisions: ADR-0003, ADR-0007, ADR-0019.
//! This crate depends only on `rbs-core` (principle P1).

pub mod caps;
pub mod config;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod encoder;
pub mod error;

pub use config::{
    AudioEncoderConfig, EncoderChange, EncoderPreset, EncoderTuning, KeyframeInterval, RateControl,
    VideoEncoderConfig,
};
pub use encoder::{AudioEncoder, AudioEncoderFactory, VideoEncoder, VideoEncoderFactory};
pub use error::{CreateError, EncodeError};
