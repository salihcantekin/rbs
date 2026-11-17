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

/// The four privacy switch values of `kind` (ADR-0141 decision 6); `None` for any
/// value that is missing or unreadable.
pub(crate) fn consent_values(
    kind: crate::privacy_consent::ConsentKind,
) -> crate::privacy_consent::ConsentValues {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const STORE: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\";
    let store = format!("{STORE}{}", kind.store());
    crate::privacy_consent::ConsentValues {
        policy: reg_u32(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Policies\Microsoft\Windows\AppPrivacy",
            kind.policy(),
        ),
        machine: reg_string(HKEY_LOCAL_MACHINE, &store, "Value"),
        user: reg_string(HKEY_CURRENT_USER, &store, "Value"),
        desktop_apps: reg_string(HKEY_CURRENT_USER, &format!("{store}\\NonPackaged"), "Value"),
    }
}

fn reg_get(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    name: &str,
    flags: u32,
    buf: &mut [u8],
) -> Option<usize> {
    use windows_sys::Win32::System::Registry::RegGetValueW;
    let key = wide(OsStr::new(key));
    let name = wide(OsStr::new(name));
    let mut len = u32::try_from(buf.len()).ok()?;
    // SAFETY: `key` and `name` are NUL-terminated and outlive the call; `buf` is
    // writable for `len` bytes, and the call writes at most `len` bytes and stores the
    // written length in `len`.
    let rc = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(len as usize)
}

fn reg_u32(root: windows_sys::Win32::System::Registry::HKEY, key: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::RRF_RT_REG_DWORD;
    let mut buf = [0u8; 4];
    let n = reg_get(root, key, name, RRF_RT_REG_DWORD, &mut buf)?;
    (n == 4).then(|| u32::from_le_bytes(buf))
}

fn reg_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    name: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
    let mut buf = [0u8; 64];
    let n = reg_get(root, key, name, RRF_RT_REG_SZ, &mut buf)?;
    let units: Vec<u16> = buf[..n]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16(&units).ok()
}
