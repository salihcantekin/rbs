//! Typed errors of the encoder port. No adapter may panic across the port boundary.

use std::fmt;

/// Why an encoder session could not be created.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// The configuration asks for something the adapter does not support.
    Unsupported {
        /// What is unsupported, for diagnostics.
        what: String,
    },
    /// No free session or device resource right now (for example a hardware session
    /// limit). The engine tries the next candidate adapter.
    ResourceExhausted,
    /// The device or library failed.
    Device(String),
    /// The configuration is inconsistent (for example a zero bitrate).
    InvalidConfig(String),
}

impl CreateError {
    /// True when trying again later or with another adapter may succeed.
    pub const fn is_recoverable(&self) -> bool {
        matches!(self, CreateError::ResourceExhausted)
    }
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateError::Unsupported { what } => write!(f, "unsupported: {what}"),
            CreateError::ResourceExhausted => f.write_str("no free encoder session"),
            CreateError::Device(e) => write!(f, "device error: {e}"),
            CreateError::InvalidConfig(e) => write!(f, "invalid configuration: {e}"),
        }
    }
}

impl std::error::Error for CreateError {}

/// Why an encoder call failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The input does not match the session (format, size, memory kind, sample
    /// format). The input was not encoded.
    InvalidInput(String),
    /// The requested runtime change is not supported by this adapter.
    Unsupported,
    /// The encoder's input queue is full; submit again after draining.
    Busy,
    /// The device was lost or reset. The session must be recreated.
    DeviceLost,
    /// Any other failure inside the adapter.
    Failed {
        /// Description for diagnostics.
        reason: String,
        /// Whether the session can continue.
        recoverable: bool,
    },
}

impl EncodeError {
    /// True when the session can continue (possibly after draining or
    /// reconfiguring); false when it must be dropped and recreated or replaced.
    pub const fn is_recoverable(&self) -> bool {
        match self {
            EncodeError::InvalidInput(_) | EncodeError::Unsupported | EncodeError::Busy => true,
            EncodeError::DeviceLost => false,
            EncodeError::Failed { recoverable, .. } => *recoverable,
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::InvalidInput(e) => write!(f, "invalid input: {e}"),
            EncodeError::Unsupported => f.write_str("unsupported change"),
            EncodeError::Busy => f.write_str("encoder queue full"),
            EncodeError::DeviceLost => f.write_str("device lost"),
            EncodeError::Failed { reason, .. } => write!(f, "encode failed: {reason}"),
        }
    }
}

impl std::error::Error for EncodeError {}
