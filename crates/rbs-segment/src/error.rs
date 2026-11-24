//! Person mask errors.

use std::fmt;

use crate::info::LevelId;

/// Why a person mask call failed. No variant carries heap data, so errors can be built
/// on the render thread without allocating. Non-exhaustive: variants may be added in a
/// minor version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentError {
    /// The backend cannot run on this computer.
    Unavailable,
    /// The model the backend needs was not provided.
    ModelMissing,
    /// The model failed its checks or could not be loaded.
    ModelLoadFailed,
    /// The device the session ran on was lost; the control thread may re-open.
    DeviceLost,
    /// The backend does not offer this quality level.
    Unsupported {
        /// The level that was asked for.
        level: LevelId,
    },
    /// The session cannot take the input size it was given.
    SizeChanged,
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("the backend is not available on this computer"),
            Self::ModelMissing => f.write_str("the model is missing"),
            Self::ModelLoadFailed => f.write_str("the model could not be loaded"),
            Self::DeviceLost => f.write_str("the device was lost"),
            Self::Unsupported { level } => write!(f, "the quality level {level:?} is not offered"),
            Self::SizeChanged => f.write_str("the input size is not supported by this session"),
        }
    }
}

impl std::error::Error for SegmentError {}
