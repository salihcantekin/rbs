//! `statvfs` and the device id (macOS, Linux and other Unix).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::{ProbeError, Space};

pub(crate) fn volume_space(folder: &Path) -> Result<Space, ProbeError> {
    let volume = std::fs::metadata(folder).map_err(ProbeError::Failed)?.dev();
    let c = CString::new(folder.as_os_str().as_bytes()).map_err(|e| {
        ProbeError::Failed(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    })?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is a NUL-terminated path that outlives the call, and `st` points to
    // writable memory the size of `statvfs`, which the call fills on success.
    let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(ProbeError::Failed(std::io::Error::last_os_error()));
    }
    // SAFETY: `statvfs` returned 0, so it initialised `st`.
    let st = unsafe { st.assume_init() };
    // `f_bavail` (blocks free to unprivileged users) times the fragment size. The
    // field widths differ between platforms, hence the conversions.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    let free_bytes = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
    Ok(Space { volume, free_bytes })
}

#[cfg(target_os = "linux")]
pub(crate) fn available_memory() -> Result<u64, ProbeError> {
    let text = std::fs::read_to_string("/proc/meminfo").map_err(ProbeError::Failed)?;
    parse_mem_available(&text).ok_or(ProbeError::Unsupported)
}

/// `MemAvailable` in bytes from the text of `/proc/meminfo` (given in KiB).
#[cfg(any(target_os = "linux", test))]
fn parse_mem_available(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_available_is_read_in_bytes() {
        let text = "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:    2048 kB\n";
        assert_eq!(parse_mem_available(text), Some(2048 * 1024));
        assert_eq!(parse_mem_available("MemTotal: 1 kB\n"), None);
    }
}
