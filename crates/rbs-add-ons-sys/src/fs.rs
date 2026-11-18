//! File system operations for add-ons.
//!
//! `*_beneath` never follows a link inside `root`. On Unix every component is opened
//! with `openat` and `O_NOFOLLOW` from the directory before it, so a link anywhere on
//! the way fails instead of being followed (this is the walk the spec allows on every
//! Unix, without needing Linux 5.6's `openat2`). On Windows each existing component
//! is checked for a reparse point before it is used, and files are opened with
//! `FILE_FLAG_OPEN_REPARSE_POINT`; add-on directories belong to the user, so another
//! process of the same user is not a threat this guards against.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;
use std::time::Duration;

use rbs_add_ons::{AddOnFs, FsError, LockGuard, RelPath};

/// The production [`AddOnFs`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SysFs;

struct Lock(#[allow(dead_code)] File); // Held for its drop, which unlocks.

impl LockGuard for Lock {}

fn io_err(e: io::Error) -> FsError {
    if e.kind() == io::ErrorKind::NotFound {
        return FsError::NotFound;
    }
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
        if matches!(e.raw_os_error(), Some(32 | 33)) {
            return FsError::SharingViolation;
        }
    }
    FsError::Io(e.to_string())
}

fn split(rel: &RelPath) -> Result<(&[String], &str), FsError> {
    match rel.components().split_last() {
        Some((last, parents)) => Ok((parents, last.as_str())),
        None => Err(FsError::Io("empty path".into())),
    }
}

impl AddOnFs for SysFs {
    fn lock(&self, path: &Path) -> Result<Box<dyn LockGuard>, FsError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => Ok(Box::new(Lock(file))),
            Err(TryLockError::WouldBlock) => Err(FsError::Busy),
            Err(TryLockError::Error(e)) => Err(io_err(e)),
        }
    }

    fn create_file_beneath(&self, root: &Path, rel: &RelPath, mode: u32) -> Result<File, FsError> {
        imp::create_file(root, rel, mode)
    }

    fn create_dir_beneath(&self, root: &Path, rel: &RelPath) -> Result<(), FsError> {
        imp::walk(root, rel.components(), true).map(drop)
    }

    fn symlink_beneath(&self, root: &Path, rel: &RelPath, target: &str) -> Result<(), FsError> {
        imp::symlink(root, rel, target)
    }

    fn hard_link_beneath(
        &self,
        root: &Path,
        rel: &RelPath,
        existing: &RelPath,
    ) -> Result<(), FsError> {
        imp::hard_link(root, rel, existing)
    }

    fn open_read(&self, path: &Path) -> Result<File, FsError> {
        File::open(path).map_err(io_err)
    }

    fn sync_file(&self, file: &File) -> Result<(), FsError> {
        file.sync_all().map_err(io_err)
    }

    fn sync_dir(&self, dir: &Path) -> Result<(), FsError> {
        imp::sync_dir(dir)
    }

    fn free_space(&self, dir: &Path) -> Result<u64, FsError> {
        imp::free_space(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        std::fs::rename(from, to).map_err(io_err)
    }

    fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        std::fs::remove_file(path).map_err(io_err)
    }

    fn remove_all(&self, dir: &Path) -> Result<(), FsError> {
        // `remove_dir_all` does not follow links (std, since 1.58.1).
        match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other.map_err(io_err),
        }
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<String>, FsError> {
        match std::fs::read_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(e)),
            Ok(entries) => Ok(entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use rbs_add_ons::{FsError, RelPath};
    use rustix::fs::{AtFlags, CWD, Mode, OFlags};
    use rustix::io::Errno;

    use super::split;

    fn errno(e: Errno) -> FsError {
        match e {
            Errno::LOOP => FsError::Link,
            Errno::NOENT => FsError::NotFound,
            other => FsError::Io(std::io::Error::from(other).to_string()),
        }
    }

    fn open_dir(at: &OwnedFd, name: &str) -> Result<OwnedFd, Errno> {
        rustix::fs::openat(
            at,
            name,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY,
            Mode::empty(),
        )
    }

    /// Opens `root`, then each of `parts` beneath it without following a link,
    /// creating missing directories when `create`.
    pub(super) fn walk(root: &Path, parts: &[String], create: bool) -> Result<OwnedFd, FsError> {
        if create {
            // `root` itself comes from the composition root and is trusted.
            std::fs::create_dir_all(root).map_err(super::io_err)?;
        }
        let mut dir = rustix::fs::openat(
            CWD,
            root,
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(errno)?;
        for part in parts {
            dir = match open_dir(&dir, part) {
                Ok(next) => next,
                Err(Errno::NOENT) if create => {
                    match rustix::fs::mkdirat(&dir, part.as_str(), Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(errno(e)),
                    }
                    open_dir(&dir, part).map_err(errno)?
                }
                // A file or a link where a directory must be.
                Err(Errno::NOTDIR) => return Err(FsError::Link),
                Err(e) => return Err(errno(e)),
            };
        }
        Ok(dir)
    }

    pub(super) fn create_file(root: &Path, rel: &RelPath, mode: u32) -> Result<File, FsError> {
        let (parents, last) = split(rel)?;
        let dir = walk(root, parents, true)?;
        let fd = rustix::fs::openat(
            &dir,
            last,
            OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as _),
        )
        .map_err(errno)?;
        // The exact mode, whatever the umask.
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode as _)).map_err(errno)?;
        Ok(File::from(fd))
    }

    pub(super) fn symlink(root: &Path, rel: &RelPath, target: &str) -> Result<(), FsError> {
        let (parents, last) = split(rel)?;
        let dir = walk(root, parents, true)?;
        rustix::fs::symlinkat(target, &dir, last).map_err(errno)
    }

    pub(super) fn hard_link(root: &Path, rel: &RelPath, existing: &RelPath) -> Result<(), FsError> {
        let (parents, last) = split(rel)?;
        let dir = walk(root, parents, true)?;
        let (eparents, elast) = split(existing)?;
        let edir = walk(root, eparents, false)?;
        // No AT_SYMLINK_FOLLOW: a link is never followed.
        rustix::fs::linkat(&edir, elast, &dir, last, AtFlags::empty()).map_err(errno)
    }

    pub(super) fn sync_dir(dir: &Path) -> Result<(), FsError> {
        let fd = walk(dir, &[], false)?;
        rustix::fs::fsync(&fd).map_err(errno)
    }

    pub(super) fn free_space(dir: &Path) -> Result<u64, FsError> {
        let s = rustix::fs::statvfs(dir).map_err(errno)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use rbs_add_ons::{FsError, RelPath};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, GetDiskFreeSpaceExW,
    };

    use super::{io_err, split};

    /// Refuses a reparse point (link, junction) on the way; creates missing
    /// directories when `create`.
    pub(super) fn walk(root: &Path, parts: &[String], create: bool) -> Result<PathBuf, FsError> {
        if create {
            // `root` itself comes from the composition root and is trusted.
            std::fs::create_dir_all(root).map_err(io_err)?;
        }
        let mut path = root.to_path_buf();
        for part in parts {
            path.push(part);
            match path.symlink_metadata() {
                Ok(m) if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                    return Err(FsError::Link);
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(FsError::Link),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                    std::fs::create_dir(&path).map_err(io_err)?;
                }
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(path)
    }

    pub(super) fn create_file(root: &Path, rel: &RelPath, _mode: u32) -> Result<File, FsError> {
        let (parents, last) = split(rel)?;
        let path = walk(root, parents, true)?.join(last);
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        {
            return Err(FsError::Link);
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(io_err)
    }

    pub(super) fn symlink(_root: &Path, _rel: &RelPath, _target: &str) -> Result<(), FsError> {
        Err(FsError::Io(
            "archives with symbolic links are not installed on Windows".into(),
        ))
    }

    pub(super) fn hard_link(root: &Path, rel: &RelPath, existing: &RelPath) -> Result<(), FsError> {
        let (parents, last) = split(rel)?;
        let path = walk(root, parents, true)?.join(last);
        let (eparents, elast) = split(existing)?;
        let epath = walk(root, eparents, false)?.join(elast);
        std::fs::hard_link(epath, path).map_err(io_err)
    }

    pub(super) fn sync_dir(_dir: &Path) -> Result<(), FsError> {
        // NTFS commits directory entries with the rename; there is no directory fsync.
        Ok(())
    }

    pub(super) fn free_space(dir: &Path) -> Result<u64, FsError> {
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free: u64 = 0;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
        // `free` is a valid u64 the call writes; the other outputs may be null.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io_err(std::io::Error::last_os_error()));
        }
        Ok(free)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use rbs_add_ons::{AddOnFs, FsError, RelPath};

    use super::SysFs;

    fn rel(p: &str) -> RelPath {
        RelPath::parse(p).unwrap()
    }

    #[test]
    fn the_lock_is_exclusive_between_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".lock");
        let first = SysFs.lock(&path).unwrap();
        assert!(matches!(SysFs.lock(&path), Err(FsError::Busy)));
        drop(first);
        assert!(SysFs.lock(&path).is_ok());
    }

    #[test]
    fn files_and_directories_are_created_beneath_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        SysFs.create_dir_beneath(tmp.path(), &rel("a/b")).unwrap();
        let f = SysFs
            .create_file_beneath(tmp.path(), &rel("a/b/c/f"), 0o640)
            .unwrap();
        drop(f);
        assert!(tmp.path().join("a/b/c/f").is_file());
        assert!(SysFs.free_space(tmp.path()).unwrap() > 0);
        SysFs.sync_dir(tmp.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_on_the_way_is_never_followed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("l")).unwrap();
        assert_eq!(
            SysFs
                .create_file_beneath(tmp.path(), &rel("l/evil"), 0o644)
                .err(),
            Some(FsError::Link)
        );
        assert_eq!(
            SysFs.create_dir_beneath(tmp.path(), &rel("l/sub")).err(),
            Some(FsError::Link)
        );
        std::os::unix::fs::symlink(outside.path().join("f"), tmp.path().join("file-link")).unwrap();
        assert!(
            SysFs
                .create_file_beneath(tmp.path(), &rel("file-link"), 0o644)
                .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        // The mode is exact, whatever the umask.
        let f = SysFs
            .create_file_beneath(tmp.path(), &rel("x"), 0o755)
            .unwrap();
        assert_eq!(f.metadata().unwrap().permissions().mode() & 0o7777, 0o755);
        // Links are created as given and hard links never follow a symbolic link.
        SysFs
            .symlink_beneath(tmp.path(), &rel("d/s"), "../x")
            .unwrap();
        SysFs
            .hard_link_beneath(tmp.path(), &rel("d/h"), &rel("x"))
            .unwrap();
        assert_eq!(
            std::fs::read_link(tmp.path().join("d/s")).unwrap().to_str(),
            Some("../x")
        );
        assert!(
            SysFs
                .hard_link_beneath(tmp.path(), &rel("d/h2"), &rel("l/f"))
                .is_err()
        );
    }

    #[test]
    fn remove_and_list_tolerate_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert_eq!(SysFs.remove_all(&missing), Ok(()));
        assert_eq!(SysFs.list_dir(&missing), Ok(Vec::new()));
        assert_eq!(SysFs.remove_file(&missing), Err(FsError::NotFound));
        assert!(!SysFs.exists(&missing));
    }
}
