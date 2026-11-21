//! Input errors.

use std::fmt;

/// Why an input call failed.
///
/// No variant carries heap data. Non-exhaustive: variants may be added in a minor version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    /// An event class or call this backend does not offer.
    Unsupported(&'static str),
    /// The OS refuses access (phase 2: Input Monitoring on macOS).
    PermissionDenied,
    /// [`MAX_SUBSCRIPTIONS`](crate::MAX_SUBSCRIPTIONS) subscriptions are open.
    Busy,
    /// An OS error code; the adapter documents its meaning.
    Backend {
        /// The OS code.
        code: i64,
    },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::Unsupported(what) => write!(f, "not supported: {what}"),
            InputError::PermissionDenied => f.write_str("the system denied access to input"),
            InputError::Busy => f.write_str("too many input subscriptions are open"),
            InputError::Backend { code } => write!(f, "input backend error {code}"),
        }
    }
}

impl std::error::Error for InputError {}
