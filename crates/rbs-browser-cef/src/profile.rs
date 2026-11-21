//! Removing temporary profiles that earlier trees left behind (ADR-0109 decision 10).
//!
//! A tree removes its own temporary profile on exit (`remove_profile`). One that was
//! killed, crashed, or whose CEF processes still held files leaves it in the temporary
//! directory. Each browser process sweeps those of dead owners once at start, on a
//! short-lived thread, so the start does not wait for it.

use std::time::SystemTime;

use rbs_browser_ipc::profile::stale_profiles;

/// Starts the one-shot sweep of stale temporary profiles. A failure to remove one is
/// ignored; the next tree tries again.
pub fn sweep_stale_in_background() {
    let _ = std::thread::Builder::new()
        .name("rbs-browser-sweep".into())
        .spawn(|| {
            let dir = std::env::temp_dir();
            for path in stale_profiles(&dir, std::process::id(), process_alive) {
                if owned_by_us(&path) {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        });
}

/// Whether process `pid` still runs and may be the owner of a directory created at
/// `created` (a pid the OS gave to a later process is not). Unknown counts as alive.
#[cfg(windows)]
fn process_alive(pid: u32, created: Option<SystemTime>) -> bool {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, STILL_ACTIVE,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess has no preconditions; the handle is closed below.
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        // No process with that id.
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return false,
        // Access denied and the like: someone else's live process.
        Err(_) => return true,
    };
    let mut code = 0_u32;
    // SAFETY: `process` is a valid handle and `code` a valid out pointer.
    let read = unsafe { GetExitCodeProcess(process, &mut code) }.is_ok();
    let mut times = [FILETIME::default(); 4];
    let [start, exit, kernel, user] = &mut times;
    // SAFETY: `process` is a valid handle opened with query rights; the four out
    // pointers are distinct valid locals.
    let started = unsafe { GetProcessTimes(process, start, exit, kernel, user) }
        .is_ok()
        .then(|| filetime_to_system(*start))
        .flatten();
    // SAFETY: `process` was opened above and is not used after this.
    let _ = unsafe { CloseHandle(process) };
    let running = !read || code == STILL_ACTIVE.0 as u32;
    running && rbs_browser_ipc::profile::may_own(started, created)
}

/// A FILETIME (100 ns ticks since 1601-01-01 UTC) as a `SystemTime`.
#[cfg(windows)]
fn filetime_to_system(t: windows::Win32::Foundation::FILETIME) -> Option<SystemTime> {
    /// 100 ns ticks from 1601-01-01 to 1970-01-01.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let since_unix = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(
        since_unix.checked_mul(100)?,
    ))
}

/// Whether process `pid` still runs. Unknown counts as alive. Unix reports no portable
/// process start time, so a reused pid keeps its directory until the pid is free.
#[cfg(unix)]
fn process_alive(pid: u32, _created: Option<SystemTime>) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether `path` belongs to this user. A shared temporary directory on Unix may hold
/// other users' profiles; on Windows the temporary directory is the user's own.
#[cfg(unix)]
fn owned_by_us(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == uid)
}

#[cfg(not(unix))]
fn owned_by_us(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}
