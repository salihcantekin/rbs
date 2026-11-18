//! File system operations, behind a trait so the rules are tested without the OS.

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use crate::names::RelPath;

/// Held while an add-on is installed, cleaned or removed; dropping it unlocks.
pub trait LockGuard: Send {}

/// A failed file system operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// Another process holds the lock.
    Busy,
    /// Windows refused a rename because another program has a file open.
    SharingViolation,
    /// The path does not exist.
    NotFound,
    /// A path component is a link where none may be (a `*_beneath` call).
    Link,
    /// Any other failure.
    Io(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("another RBS is changing this add-on"),
            Self::SharingViolation => f.write_str("a file is in use by another program"),
            Self::NotFound => f.write_str("not found"),
            Self::Link => f.write_str("a path component is a link"),
            Self::Io(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for FsError {}

/// The OS file operations an install needs.
///
/// `*_beneath` calls never follow a link in `rel` and never leave `root`
/// (Linux: `openat2` with `RESOLVE_BENEATH`, or an `openat` walk with `O_NOFOLLOW`
/// per component; macOS: `O_NOFOLLOW` opens; Windows: reparse-point checks). They
/// create missing parent directories the same way.
pub trait AddOnFs: Send + Sync {
    /// Takes an OS advisory lock on `path` (created if missing); [`FsError::Busy`]
    /// when another process holds it.
    fn lock(&self, path: &Path) -> Result<Box<dyn LockGuard>, FsError>;
    /// Creates (or truncates) a regular file for reading and writing, with `mode`
    /// (Unix permission bits; setuid, setgid and sticky are already cleared).
    fn create_file_beneath(&self, root: &Path, rel: &RelPath, mode: u32) -> Result<File, FsError>;
    /// Creates a directory and its missing parents.
    fn create_dir_beneath(&self, root: &Path, rel: &RelPath) -> Result<(), FsError>;
    /// Creates a symbolic link at `rel` whose text is `target`.
    fn symlink_beneath(&self, root: &Path, rel: &RelPath, target: &str) -> Result<(), FsError>;
    /// Creates a hard link at `rel` to the existing file `existing`.
    fn hard_link_beneath(
        &self,
        root: &Path,
        rel: &RelPath,
        existing: &RelPath,
    ) -> Result<(), FsError>;
    /// Opens an existing regular file for reading.
    fn open_read(&self, path: &Path) -> Result<File, FsError>;
    /// Flushes a file's data and metadata to disk.
    fn sync_file(&self, file: &File) -> Result<(), FsError>;
    /// Flushes a directory's entries to disk (a no-op where the OS has none).
    fn sync_dir(&self, dir: &Path) -> Result<(), FsError>;
    /// Bytes free on the volume that holds `dir`.
    fn free_space(&self, dir: &Path) -> Result<u64, FsError>;
    /// Renames within one volume; [`FsError::SharingViolation`] is its own error.
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError>;
    /// Deletes one file; [`FsError::NotFound`] when it is not there.
    fn remove_file(&self, path: &Path) -> Result<(), FsError>;
    /// Deletes a directory tree without following links; `Ok` when it is not there.
    fn remove_all(&self, dir: &Path) -> Result<(), FsError>;
    /// Names of the entries of `dir`; empty when it is not there.
    fn list_dir(&self, dir: &Path) -> Result<Vec<String>, FsError>;
    /// Whether `path` exists (without following a final link).
    fn exists(&self, path: &Path) -> bool;
    /// Waits `duration` (the rename retry). Tests advance a `TestClock` instead.
    fn pause(&self, duration: Duration);
}
