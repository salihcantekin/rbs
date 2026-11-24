//! Why a source cannot deliver (protocol 1.20, ADR-0077).

use serde::{Deserialize, Serialize};

/// Why a source cannot deliver (protocol 1.20, ADR-0077). A client treats a value it does
/// not know as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceFault {
    /// The target is gone: a window closed, a device removed.
    TargetGone,
    /// The system refused access, or the user has not decided yet.
    PermissionDenied,
    /// Another application holds the device.
    Busy,
    /// Any other cause.
    #[serde(other)]
    Other,
}
