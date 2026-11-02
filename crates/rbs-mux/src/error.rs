//! Typed errors of the muxer port. No adapter may panic across the port boundary.

use std::fmt;

/// Why a muxer session could not be created.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// The container cannot carry a stream, or the container is not written by this
    /// adapter.
    Unsupported {
        /// What is unsupported, for diagnostics.
        what: String,
    },
    /// The configuration is inconsistent (for example no streams, or missing
    /// extradata the container needs).
    InvalidConfig(String),
}

impl CreateError {
    /// Creation errors of a muxer are never fixed by retrying.
    pub const fn is_recoverable(&self) -> bool {
        false
    }
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateError::Unsupported { what } => write!(f, "unsupported: {what}"),
            CreateError::InvalidConfig(e) => write!(f, "invalid configuration: {e}"),
        }
    }
}

impl std::error::Error for CreateError {}

/// Why a muxer call failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxError {
    /// `write` or `finish` was called before `begin`, or a call after `finish`.
    InvalidState(&'static str),
    /// The stream index is not part of this session.
    UnknownStream(u16),
    /// A packet's dts is earlier than the previous packet's dts of the same stream.
    /// The packet was dropped; the session continues.
    NonMonotonic {
        /// The stream.
        stream: u16,
    },
    /// The packet does not match its stream (codec or bitstream format). Dropped.
    InvalidPacket(String),
    /// A container limit was exceeded (for example a 32-bit size or timestamp field).
    Overflow(&'static str),
}

impl MuxError {
    /// True when the session can continue after dropping the offending packet.
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            MuxError::NonMonotonic { .. } | MuxError::InvalidPacket(_) | MuxError::UnknownStream(_)
        )
    }
}

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuxError::InvalidState(e) => write!(f, "invalid state: {e}"),
            MuxError::UnknownStream(s) => write!(f, "unknown stream {s}"),
            MuxError::NonMonotonic { stream } => {
                write!(f, "stream {stream}: dts earlier than the previous packet")
            }
            MuxError::InvalidPacket(e) => write!(f, "invalid packet: {e}"),
            MuxError::Overflow(e) => write!(f, "container limit exceeded: {e}"),
        }
    }
}

impl std::error::Error for MuxError {}
