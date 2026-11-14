//! Free space, volume and available memory from the operating system, for recording
//! protection (ADR-0090 decision 11, spec `docs/specs/recording-protection.md`).
//!
//! Every call may block (a network drive can take seconds or hang), so callers run
//! them on a worker thread, never on a real-time or control thread. A platform without
//! a reading answers [`ProbeError::Unsupported`], never a guess.
//!
//! - Windows: `GetVolumePathNameW`, `GetDiskFreeSpaceExW`, `GlobalMemoryStatusEx`.
//! - macOS and other Unix: `statvfs` and the folder's device id; available memory is
//!   `MemAvailable` from `/proc/meminfo` on Linux and unsupported on macOS (free pages
//!   alone understate what macOS can give back).

use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Why there is no reading.
#[derive(Debug)]
pub enum ProbeError {
    /// The call failed (the folder is missing, the drive is gone, access denied).
    Failed(std::io::Error),
    /// This platform has no such reading.
    Unsupported,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "{e}"),
            Self::Unsupported => f.write_str("not available on this platform"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// A folder's volume and its free space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// Identifies the volume: folders with the same value share free space. Opaque.
    pub volume: u64,
    /// Bytes the current user can still write there.
    pub free_bytes: u64,
}

/// The volume of `folder` and the bytes the current user can still write there.
pub fn volume_space(folder: &Path) -> Result<Space, ProbeError> {
    #[cfg(unix)]
    {
        unix::volume_space(folder)
    }
    #[cfg(windows)]
    {
        windows::volume_space(folder)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = folder;
        Err(ProbeError::Unsupported)
    }
}

/// Physical memory available to programs without swapping, in bytes.
pub fn available_memory() -> Result<u64, ProbeError> {
    #[cfg(target_os = "linux")]
    {
        unix::available_memory()
    }
    #[cfg(windows)]
    {
        windows::available_memory()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Err(ProbeError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;

    #[test]
    fn the_temp_folder_has_a_volume_and_some_free_space() {
        let dir = std::env::temp_dir();
        let a = volume_space(&dir).unwrap();
        let b = volume_space(&dir).unwrap();
        assert_eq!(a.volume, b.volume, "stable for one folder");
        assert!(a.free_bytes > 0);
    }

    #[test]
    fn a_missing_folder_fails_instead_of_guessing() {
        let missing = std::env::temp_dir().join("rbs-system-probe-no-such-folder-7c1f");
        assert!(matches!(volume_space(&missing), Err(ProbeError::Failed(_))));
    }

    #[test]
    fn memory_is_read_or_unsupported() {
        match available_memory() {
            Ok(bytes) => assert!(bytes > 0),
            Err(ProbeError::Unsupported) => {}
            Err(e) => panic!("{e}"),
        }
    }
}
