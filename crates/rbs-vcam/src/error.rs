use std::fmt;

/// Errors of the virtual camera port (spec, "Errors and failure behaviour").
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VcamError {
    /// This platform or build cannot have the camera; the text is shown to the user.
    Unsupported(String),
    /// The camera is not added to the system.
    NotInstalled,
    /// The user or the OS refused (UAC cancelled, no administrator rights).
    PermissionDenied(String),
    /// A configuration outside the adapter's capabilities.
    InvalidConfig(String),
    /// A frame that breaks the session's configuration (size, format, memory kind), or
    /// a slot token of another session.
    InvalidFrame(&'static str),
    /// An OS call failed.
    Os {
        /// The OS error code (an `HRESULT` on Windows).
        code: i64,
        /// What failed.
        message: String,
    },
}

impl fmt::Display for VcamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "virtual camera not available: {reason}"),
            Self::NotInstalled => f.write_str("the virtual camera is not added"),
            Self::PermissionDenied(reason) => write!(f, "permission denied: {reason}"),
            Self::InvalidConfig(reason) => write!(f, "invalid virtual camera settings: {reason}"),
            Self::InvalidFrame(reason) => write!(f, "invalid frame: {reason}"),
            Self::Os { code, message } => write!(f, "{message} (error {code:#x})"),
        }
    }
}

impl std::error::Error for VcamError {}
