//! Capture errors.

use std::fmt;

/// Which error space a [`CaptureError::Backend`] code comes from.
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

/// Why a capture call failed.
///
/// No variant carries heap data, so errors can be built on the render thread without
/// allocating. Non-exhaustive: variants may be added in a minor version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    /// The target kind, call or feature is not offered by this adapter.
    Unsupported(&'static str),
    /// The target id does not exist (any more).
    TargetNotFound,
    /// The target went away while the session was active (window closed, display or
    /// camera unplugged).
    TargetLost,
    /// The user or the OS refused access (macOS privacy settings, a portal policy).
    /// A closed picker is [`CaptureError::Cancelled`] instead.
    PermissionDenied,
    /// Access was never granted or denied; call `request_permission` first.
    PermissionNotDetermined,
    /// The user closed the OS picker without choosing. Not a permission denial; the UI
    /// must not present it as one.
    Cancelled,
    /// None of the requested pixel formats or memory kinds can be delivered, or the
    /// frames cannot be shared with the requested GPU device.
    FormatUnsupported,
    /// The device is opened exclusively by another application.
    DeviceBusy,
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

impl CaptureError {
    /// Whether the engine should retry (with back-off) rather than fall back to another
    /// adapter or report to the user.
    pub const fn is_recoverable(&self) -> bool {
        match self {
            CaptureError::TargetLost | CaptureError::DeviceBusy => true,
            CaptureError::Backend { recoverable, .. } => *recoverable,
            CaptureError::Unsupported(_)
            | CaptureError::TargetNotFound
            | CaptureError::PermissionDenied
            | CaptureError::PermissionNotDetermined
            | CaptureError::Cancelled
            | CaptureError::FormatUnsupported => false,
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Unsupported(what) => write!(f, "unsupported: {what}"),
            CaptureError::TargetNotFound => f.write_str("capture target not found"),
            CaptureError::TargetLost => f.write_str("capture target lost"),
            CaptureError::PermissionDenied => f.write_str("capture permission denied"),
            CaptureError::PermissionNotDetermined => {
                f.write_str("capture permission not requested yet")
            }
            CaptureError::Cancelled => f.write_str("capture target selection cancelled"),
            CaptureError::FormatUnsupported => {
                f.write_str("none of the requested formats can be delivered")
            }
            CaptureError::DeviceBusy => f.write_str("capture device is busy"),
            CaptureError::Backend {
                domain,
                code,
                recoverable,
            } => write!(
                f,
                "capture backend error {domain:?} {code:#x} ({})",
                if *recoverable { "recoverable" } else { "fatal" }
            ),
        }
    }
}

impl std::error::Error for CaptureError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverability_follows_the_spec() {
        assert!(CaptureError::TargetLost.is_recoverable());
        assert!(CaptureError::DeviceBusy.is_recoverable());
        assert!(!CaptureError::PermissionDenied.is_recoverable());
        assert!(!CaptureError::PermissionNotDetermined.is_recoverable());
        assert!(!CaptureError::FormatUnsupported.is_recoverable());
        let backend = CaptureError::Backend {
            domain: ErrorDomain::HResult,
            code: 0x887A_0026,
            recoverable: true,
        };
        assert!(backend.is_recoverable());
        assert!(backend.to_string().contains("HResult"));
    }
}
