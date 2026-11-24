//! Who may read or change a secret file on Windows: the Unix mode bits' counterpart
//! (0600, `chmod`). A file is private when its access list allows reading or changing
//! only to the current user, the system account and Administrators (who can take any
//! file anyway, like root). Marked low-level module (P11): calls into Win32, each
//! `unsafe` block with its invariant. Cold path: a save or a read of a settings file.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorDacl, GetTokenInformation, INHERIT_ONLY_ACE, IsWellKnownSid,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
    TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_APPEND_DATA, FILE_READ_DATA, FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `ACCESS_ALLOWED_ACE_TYPE`: an entry that grants access.
const ALLOWED: u8 = 0;

/// Rights that change a file's content or who may.
pub const CHANGE: u32 =
    FILE_WRITE_DATA | FILE_APPEND_DATA | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;
/// Rights that read a file's content.
pub const READ: u32 = FILE_READ_DATA | GENERIC_READ | GENERIC_ALL;

/// Whether someone besides the current user, the system and Administrators holds any
/// of `rights` on `path`: `Ok(true)` for a file without an access list (everyone may),
/// `Err` when the system cannot say.
pub fn others_may(path: &Path, rights: u32) -> Result<bool, String> {
    let user = CurrentUser::get()?;
    let name = wide(path);
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `name` is a null-terminated path; the out pointers are valid and the
    // ones not asked for are null. On success `sd` owns `dacl` and is freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(format!(
            "the file's access list cannot be read (error {err})"
        ));
    }
    let _sd = Local(sd);
    if dacl.is_null() {
        return Ok(true);
    }
    // SAFETY: `dacl` points into `sd`, alive until `_sd` drops.
    let count = unsafe { (*dacl).AceCount };
    for i in 0..u32::from(count) {
        let mut ace: *mut core::ffi::c_void = null_mut();
        // SAFETY: `i` is below the list's count; `ace` receives a pointer into it.
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
            return Err("the file's access list cannot be read".to_owned());
        }
        // SAFETY: every entry starts with an `ACE_HEADER`.
        let header = unsafe { *(ace as *const ACE_HEADER) };
        if header.AceType != ALLOWED || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        let allowed = ace as *const ACCESS_ALLOWED_ACE;
        // SAFETY: an allowed entry is an `ACCESS_ALLOWED_ACE`; its SID starts at
        // `SidStart` and lies inside the entry.
        let (mask, sid) = unsafe { ((*allowed).Mask, &raw const (*allowed).SidStart as PSID) };
        if mask & rights != 0 && !user.trusted(sid) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Gives `path` an access list of its own that allows only the current user and the
/// system, nothing inherited: the counterpart of mode 0600. Call before writing the
/// secret into the file.
pub fn make_private(path: &Path) -> std::io::Result<()> {
    let sid = CurrentUser::get()
        .and_then(|u| u.string())
        .map_err(std::io::Error::other)?;
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl` is null-terminated; `sd` receives a LocalAlloc'd descriptor freed below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let _sd = Local(sd);
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = null_mut();
    // SAFETY: `sd` is the valid descriptor made above; the out pointers are valid.
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(std::io::Error::other("no access list was made"));
    }
    let name = wide(path);
    // SAFETY: `name` is a null-terminated path; `dacl` points into `sd`, alive here;
    // owner, group and audit list are not changed (null).
    let err = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(err as i32));
    }
    Ok(())
}

/// The current user's SID, in a buffer of its own.
struct CurrentUser {
    /// A `TOKEN_USER` followed by the SID it points to; `u64` for its alignment.
    buf: Vec<u64>,
}

impl CurrentUser {
    fn get() -> Result<CurrentUser, String> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the current process pseudo-handle needs no closing; `token` is
        // closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err("the current user cannot be read".to_owned());
        }
        let mut len = 0u32;
        // SAFETY: a size query with no buffer; it fails and sets `len`.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: `buf` holds at least `len` writable bytes, aligned for `TOKEN_USER`.
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        };
        // SAFETY: `token` was opened above and is closed once.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err("the current user cannot be read".to_owned());
        }
        Ok(CurrentUser { buf })
    }

    fn sid(&self) -> PSID {
        // SAFETY: `buf` holds a `TOKEN_USER` whose SID points into `buf`.
        unsafe { (*(self.buf.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    /// The user, the system or Administrators.
    fn trusted(&self, sid: PSID) -> bool {
        // SAFETY: both SIDs are valid: one inside an access list alive in the caller,
        // one inside `buf`.
        unsafe {
            EqualSid(sid, self.sid()) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }

    /// The SID as text (`S-1-5-21-...`).
    fn string(&self) -> Result<String, String> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: the SID is valid; `text` receives a LocalAlloc'd string freed below.
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 || text.is_null() {
            return Err("the current user cannot be read".to_owned());
        }
        let _text = Local(text.cast());
        // SAFETY: a null-terminated UTF-16 string, alive until `_text` drops; the slice
        // ends before the terminator.
        let s = unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
        };
        Ok(s)
    }
}

/// Memory the system allocated with `LocalAlloc`, freed once when dropped.
struct Local(*mut core::ffi::c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the call that made it, freed only here.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn a_private_file_is_private_and_an_inherited_one_is_checked() {
        let dir = std::env::temp_dir().join(format!("rbs-acl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.toml");
        std::fs::write(&path, b"x").unwrap();
        make_private(&path).unwrap();
        assert!(!others_may(&path, CHANGE | READ).unwrap());
        // Everyone may read and change it: refused.
        let sddl: Vec<u16> = "D:P(A;;FA;;;WD)".encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: as in `make_private`.
        assert_ne!(0, unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        });
        let _sd = Local(sd);
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: as in `make_private`.
        unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
        let name = wide(&path);
        // SAFETY: as in `make_private`.
        let err = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        assert_eq!(err, ERROR_SUCCESS);
        assert!(others_may(&path, CHANGE).unwrap());
        assert!(others_may(&path, READ).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
