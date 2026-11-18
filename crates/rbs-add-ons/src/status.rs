//! Statuses and errors ([add-ons spec], "Status" and the error table).
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

use std::fmt;

/// Where an add-on is, as shown in Settings and by `addon.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not on disk (a version newer than this build's counts as not installed).
    NotInstalled,
    /// Downloading.
    Downloading {
        /// Bytes received.
        bytes: u64,
        /// Bytes expected.
        total: u64,
    },
    /// Checking the archive's hash.
    Verifying,
    /// Unpacking and moving into place.
    Installing,
    /// The catalog's version is installed.
    Installed {
        /// That version.
        version: String,
    },
    /// Only a lower version is on disk.
    UpdateAvailable {
        /// The highest lower version on disk.
        installed: String,
        /// The catalog's version.
        available: String,
    },
    /// The last install failed.
    Failed {
        /// Why.
        reason: FailReason,
    },
}

/// Why an install failed, as shown on the add-on's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The download failed or stalled.
    Network,
    /// A URL or redirect was not `https` or went to a host outside the list.
    HostNotAllowed,
    /// More bytes arrived than the catalog allows.
    TooLarge,
    /// The archive's SHA-256 differs from the catalog's.
    HashMismatch,
    /// The archive broke a name, link, size or content rule.
    UnsafeArchive,
    /// Not enough free space.
    NoSpace,
    /// A file system error.
    Io,
}

/// A failed install, clean or remove. Nothing panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// Another install, clean or remove of this add-on runs.
    Busy,
    /// Cancelled by the user.
    Cancelled,
    /// Not enough free space.
    NoSpace,
    /// The download failed.
    Network(String),
    /// No byte arrived within the stall timeout.
    Stalled,
    /// A URL or redirect went to a host outside the list.
    HostNotAllowed(String),
    /// A URL or redirect was not `https`.
    NotHttps,
    /// More bytes than the catalog's size.
    TooLarge,
    /// The SHA-256 did not match.
    HashMismatch,
    /// The archive broke a rule.
    UnsafeArchive(String),
    /// A file system error.
    Io(String),
    /// The catalog has no artifact for this computer.
    NotAvailable,
}

impl InstallError {
    /// The status reason after this error; `None` when the status does not change
    /// (`Busy`, `NotAvailable`: refused before anything started; `Cancelled`: the
    /// status returns to what it was).
    pub fn fail_reason(&self) -> Option<FailReason> {
        Some(match self {
            Self::Busy | Self::NotAvailable | Self::Cancelled => return None,
            Self::Network(_) | Self::Stalled => FailReason::Network,
            Self::HostNotAllowed(_) | Self::NotHttps => FailReason::HostNotAllowed,
            Self::TooLarge => FailReason::TooLarge,
            Self::HashMismatch => FailReason::HashMismatch,
            Self::UnsafeArchive(_) => FailReason::UnsafeArchive,
            Self::NoSpace => FailReason::NoSpace,
            Self::Io(_) => FailReason::Io,
        })
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("another install or removal of this add-on is running"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::NoSpace => f.write_str("not enough free space"),
            Self::Network(e) => write!(f, "download failed: {e}"),
            Self::Stalled => f.write_str("the download stalled"),
            Self::HostNotAllowed(h) => write!(f, "`{h}` is not an allowed host"),
            Self::NotHttps => f.write_str("only https downloads are allowed"),
            Self::TooLarge => f.write_str("the download is larger than expected"),
            Self::HashMismatch => f.write_str("the download does not match its checksum"),
            Self::UnsafeArchive(e) => write!(f, "unsafe archive: {e}"),
            Self::Io(e) => write!(f, "file error: {e}"),
            Self::NotAvailable => f.write_str("not available for this computer"),
        }
    }
}

impl std::error::Error for InstallError {}

#[cfg(test)]
mod tests {
    use super::{FailReason, InstallError};

    #[test]
    fn every_error_maps_as_the_spec_table_says() {
        use InstallError as E;
        for (e, r) in [
            (E::Busy, None),
            (E::NotAvailable, None),
            (E::Cancelled, None),
            (E::Network(String::new()), Some(FailReason::Network)),
            (E::Stalled, Some(FailReason::Network)),
            (
                E::HostNotAllowed(String::new()),
                Some(FailReason::HostNotAllowed),
            ),
            (E::NotHttps, Some(FailReason::HostNotAllowed)),
            (E::TooLarge, Some(FailReason::TooLarge)),
            (E::HashMismatch, Some(FailReason::HashMismatch)),
            (
                E::UnsafeArchive(String::new()),
                Some(FailReason::UnsafeArchive),
            ),
            (E::NoSpace, Some(FailReason::NoSpace)),
            (E::Io(String::new()), Some(FailReason::Io)),
        ] {
            assert_eq!(e.fail_reason(), r, "{e:?}");
        }
    }
}
