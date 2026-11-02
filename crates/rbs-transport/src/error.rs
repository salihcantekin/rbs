//! Typed errors of the transport port. No adapter may panic across the port boundary.
//! Error messages must never contain the configured secret.

use std::fmt;

/// Why a transport session could not be created.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// URL scheme, input kind or option not supported by this adapter.
    Unsupported {
        /// What is unsupported, for diagnostics.
        what: String,
    },
    /// The configuration is inconsistent (for example a malformed URL).
    InvalidConfig(String),
}

impl CreateError {
    /// Creation errors of a transport are never fixed by retrying.
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

/// Why a transport call failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// `send` before `start`.
    NotStarted,
    /// The session was finished or failed for good.
    Closed,
    /// The queue is full and this transport never drops (files). Nothing was queued;
    /// the caller keeps the item and sends it again later, applying its own bounded
    /// buffering. Recoverable.
    QueueFull,
    /// The item does not match the session's input kind or streams.
    InvalidInput(String),
    /// Connecting or the connection failed (network, TLS, handshake).
    Connection(String),
    /// The peer rejected us (authentication, unknown stream key, protocol error).
    Rejected(String),
    /// Local I/O failed (disk full, permission denied).
    Io(String),
    /// `finish` could not deliver everything before its timeout.
    Timeout,
}

impl TransportError {
    /// True when the session can continue or recover by reconnecting.
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            TransportError::QueueFull
                | TransportError::InvalidInput(_)
                | TransportError::Connection(_)
        )
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::NotStarted => f.write_str("not started"),
            TransportError::Closed => f.write_str("closed"),
            TransportError::QueueFull => f.write_str("send queue full"),
            TransportError::InvalidInput(e) => write!(f, "invalid input: {e}"),
            TransportError::Connection(e) => write!(f, "connection failed: {e}"),
            TransportError::Rejected(e) => write!(f, "rejected by peer: {e}"),
            TransportError::Io(e) => write!(f, "I/O error: {e}"),
            TransportError::Timeout => f.write_str("timed out delivering queued data"),
        }
    }
}

impl std::error::Error for TransportError {}
