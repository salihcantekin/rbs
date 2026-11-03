//! Audio I/O errors.

use std::fmt;

/// Which error space an [`AudioIoError::Backend`] code comes from.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorDomain {
    /// A Windows `HRESULT`.
    HResult,
    /// A POSIX `errno` value.
    Errno,
    /// An Apple `OSStatus`.
    OsStatus,
    /// A library-specific code; the adapter documents its meaning.
    Library,
}

/// Why an audio I/O call failed.
///
/// No variant carries heap data, so errors can be built on the audio thread without
/// allocating. Non-exhaustive: variants may be added in a minor version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioIoError {
    /// The endpoint kind, call or feature is not offered by this adapter.
    Unsupported(&'static str),
    /// The endpoint id does not exist (any more).
    EndpointNotFound,
    /// The endpoint went away while the stream was active (device unplugged,
    /// application exited).
    EndpointLost,
    /// The user or the OS refused access (macOS microphone privacy, a portal policy).
    PermissionDenied,
    /// Access was never granted or denied; call `request_permission` first.
    PermissionNotDetermined,
    /// The endpoint cannot deliver audio in any form the adapter can convert.
    FormatUnsupported,
    /// The device's format changed while active (the user changed its rate in the OS
    /// settings). The stream is stopped; open a new one to get the new format.
    FormatChanged,
    /// The device is opened exclusively by another application.
    DeviceBusy,
    /// The buffers passed to `read` do not match the stream's channel count or have
    /// different lengths. A caller bug; nothing was read.
    BufferMismatch,
    /// An OS or library error.
    Backend {
        /// Which error space `code` belongs to.
        domain: ErrorDomain,
        /// The OS or library error code.
        code: i64,
        /// Whether retrying the same operation may succeed.
        recoverable: bool,
    },
}

impl AudioIoError {
    /// Whether the engine should retry (with back-off) rather than fall back to another
    /// adapter or report to the user.
    pub const fn is_recoverable(&self) -> bool {
        match self {
            AudioIoError::EndpointLost | AudioIoError::DeviceBusy | AudioIoError::FormatChanged => {
                true
            }
            AudioIoError::Backend { recoverable, .. } => *recoverable,
            AudioIoError::Unsupported(_)
            | AudioIoError::EndpointNotFound
            | AudioIoError::PermissionDenied
            | AudioIoError::PermissionNotDetermined
            | AudioIoError::FormatUnsupported
            | AudioIoError::BufferMismatch => false,
        }
    }
}

impl fmt::Display for AudioIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioIoError::Unsupported(what) => write!(f, "unsupported: {what}"),
            AudioIoError::EndpointNotFound => f.write_str("audio endpoint not found"),
            AudioIoError::EndpointLost => f.write_str("audio endpoint lost"),
            AudioIoError::PermissionDenied => f.write_str("audio capture permission denied"),
            AudioIoError::PermissionNotDetermined => {
                f.write_str("audio capture permission not requested yet")
            }
            AudioIoError::FormatUnsupported => {
                f.write_str("the endpoint's sample format cannot be converted")
            }
            AudioIoError::FormatChanged => f.write_str("the endpoint's format changed"),
            AudioIoError::DeviceBusy => f.write_str("audio device is busy"),
            AudioIoError::BufferMismatch => {
                f.write_str("read buffers do not match the stream's channels")
            }
            AudioIoError::Backend {
                domain,
                code,
                recoverable,
            } => write!(
                f,
                "audio backend error {domain:?} {code:#x} ({})",
                if *recoverable { "recoverable" } else { "fatal" }
            ),
        }
    }
}

impl std::error::Error for AudioIoError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverability_follows_the_spec() {
        assert!(AudioIoError::EndpointLost.is_recoverable());
        assert!(AudioIoError::DeviceBusy.is_recoverable());
        assert!(AudioIoError::FormatChanged.is_recoverable());
        assert!(!AudioIoError::PermissionDenied.is_recoverable());
        assert!(!AudioIoError::BufferMismatch.is_recoverable());
        let backend = AudioIoError::Backend {
            domain: ErrorDomain::OsStatus,
            code: -50,
            recoverable: false,
        };
        assert!(!backend.is_recoverable());
        assert!(backend.to_string().contains("OsStatus"));
    }
}
