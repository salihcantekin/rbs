//! Why a source cannot deliver (ADR-0077).
//!
//! A typed cause that source nodes record next to their counters, so the application
//! can say "window closed" or "camera in use" without reading error text. Adapters that
//! cannot tell the cause report [`SourceFault::Other`].

/// The cause of a source's failure, as far as the adapter knows it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceFault {
    /// The target is gone: a window closed, a display or device removed, an endpoint
    /// unplugged, an application stopped.
    TargetGone,
    /// The system refused access, or the user has not decided yet.
    PermissionDenied,
    /// Another application holds the device.
    Busy,
    /// Any other cause; the adapter's own text says more.
    Other,
}

impl SourceFault {
    /// The value for an atomic byte; 0 is reserved for "no fault" (see [`Self::from_code`]).
    pub const fn code(self) -> u8 {
        match self {
            SourceFault::TargetGone => 1,
            SourceFault::PermissionDenied => 2,
            SourceFault::Busy => 3,
            SourceFault::Other => 4,
        }
    }

    /// The fault stored by [`Self::code`]; `None` for 0 (no fault) and unknown codes.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(SourceFault::TargetGone),
            2 => Some(SourceFault::PermissionDenied),
            3 => Some(SourceFault::Busy),
            4 => Some(SourceFault::Other),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_zero_means_none() {
        for f in [
            SourceFault::TargetGone,
            SourceFault::PermissionDenied,
            SourceFault::Busy,
            SourceFault::Other,
        ] {
            assert_eq!(SourceFault::from_code(f.code()), Some(f));
        }
        assert_eq!(SourceFault::from_code(0), None);
        assert_eq!(SourceFault::from_code(200), None);
    }
}
