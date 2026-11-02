//! Capture errors.

use std::fmt;

/// Why a capture call failed.
///
/// No variant carries heap data, so errors can be built on the render thread without
/// allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    /// The target kind, selector or feature is not offered by this adapter.
    Unsupported(&'static str),
    /// The target id does not exist (any more).
    TargetNotFound,
    /// The target went away while the session was active (window closed, display or
    /// camera unplugged).
    TargetLost,
    /// The user or the OS refused access (macOS privacy settings, portal dialog
    /// cancelled).
    PermissionDenied,
    /// None of the requested pixel formats or memory kinds can be delivered.
    FormatUnsupported,
    /// The device is opened exclusively by another application.
    DeviceBusy,
    /// An OS or library error.
    Backend {
        /// The OS or library error code (for example an `HRESULT`).
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
            CaptureError::FormatUnsupported => {
                f.write_str("none of the requested formats can be delivered")
            }
            CaptureError::DeviceBusy => f.write_str("capture device is busy"),
            CaptureError::Backend { code, recoverable } => write!(
                f,
                "capture backend error {code:#x} ({})",
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
        assert!(!CaptureError::FormatUnsupported.is_recoverable());
        assert!(
            CaptureError::Backend {
                code: 1,
                recoverable: true
            }
            .is_recoverable()
        );
    }
}
