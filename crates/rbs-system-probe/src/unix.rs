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

pub(crate) fn os_version() -> Result<String, ProbeError> {
    let mut u = std::mem::MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: `u` points to writable memory the size of `utsname`, which the call
    // fills on success.
    let rc = unsafe { libc::uname(u.as_mut_ptr()) };
    if rc != 0 {
        return Err(ProbeError::Failed(std::io::Error::last_os_error()));
    }
    // SAFETY: `uname` returned 0, so it initialised `u`.
    let u = unsafe { u.assume_init() };
    let field = |f: &[libc::c_char]| {
        // `c_char` is `i8` or `u8` by platform; each is one byte of the C string.
        #[allow(clippy::unnecessary_cast)]
        let bytes: Vec<u8> = f
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(format!(
        "{} {} {}",
        field(&u.sysname),
        field(&u.release),
        field(&u.version)
    ))
}

/// The executables of the parent processes, nearest first, at most `max`; stops at
/// pid 1 or the first process it cannot read (ADR-0141 decision 10).
pub(crate) fn ancestors(max: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    // SAFETY: `getppid` has no preconditions and cannot fail.
    let mut pid = unsafe { libc::getppid() };
    while pid > 1 && out.len() < max {
        let Some(path) = exe_of(pid) else {
            break;
        };
        out.push(path);
        match parent_of(pid) {
            Some(parent) if parent != pid => pid = parent,
            _ => break,
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn exe_of(pid: libc::pid_t) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable for its whole length, which is passed as the size.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(std::ffi::OsString::from_vec(buf).into())
}

#[cfg(target_os = "macos")]
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` points to writable memory of `size` bytes, the size the flavour
    // `PROC_PIDTBSDINFO` fills.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: the call filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    libc::pid_t::try_from(info.pbi_ppid).ok()
}

#[cfg(not(target_os = "macos"))]
fn exe_of(pid: libc::pid_t) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "macos"))]
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`; `comm` may hold spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
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
