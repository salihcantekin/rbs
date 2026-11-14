//! `GetVolumePathNameW`, `GetDiskFreeSpaceExW` and `GlobalMemoryStatusEx`.

use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::{ProbeError, Space};

/// Longest volume path asked for; longer paths fail the call, which is reported.
const VOLUME_PATH_LEN: usize = 1024;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn volume_space(folder: &Path) -> Result<Space, ProbeError> {
    let path = wide(folder.as_os_str());
    let mut root = vec![0u16; VOLUME_PATH_LEN];
    // SAFETY: `path` is NUL-terminated and outlives the call; `root` has room for
    // `VOLUME_PATH_LEN` UTF-16 units, the length passed.
    let ok =
        unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), VOLUME_PATH_LEN as u32) };
    if ok == 0 {
        return Err(ProbeError::Failed(std::io::Error::last_os_error()));
    }
    let len = root.iter().position(|c| *c == 0).unwrap_or(root.len());
    let mut hasher = DefaultHasher::new();
    // Volume roots compare without case on Windows.
    String::from_utf16_lossy(&root[..len])
        .to_lowercase()
        .hash(&mut hasher);
    let volume = hasher.finish();
    let mut free: u64 = 0;
    // SAFETY: `path` is NUL-terminated and outlives the call; `free` is a valid u64
    // for the bytes available to the caller; the two other outputs are optional and
    // passed as null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(ProbeError::Failed(std::io::Error::last_os_error()));
    }
    Ok(Space {
        volume,
        free_bytes: free,
    })
}

pub(crate) fn available_memory() -> Result<u64, ProbeError> {
    // SAFETY: MEMORYSTATUSEX is plain data; all zeros is a valid value.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a valid MEMORYSTATUSEX with `dwLength` set, as the call
    // requires.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return Err(ProbeError::Failed(std::io::Error::last_os_error()));
    }
    Ok(status.ullAvailPhys)
}
