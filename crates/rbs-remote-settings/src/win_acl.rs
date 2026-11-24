//! Who may read or change a secret file on Windows: the Unix mode bits' counterpart
//! (0600, `chmod`). A file is private when its owner is trusted and its access list
//! grants reading or changing only to trusted accounts: the current user, the system
//! and Administrators (who can take any file anyway, like root). Marked low-level
//! module (P11): calls into Win32, each `unsafe` block with its invariant. Cold path: a
//! save or a read of a settings file.

use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetTokenInformation, INHERIT_ONLY_ACE, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_READ_DATA,
    FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_ACE_TYPE, ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
    ACCESS_DENIED_OBJECT_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Rights that change a file's content, replace it or change who may.
pub const CHANGE: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;
/// Rights that read a file's content.
pub const READ: u32 = FILE_READ_DATA | GENERIC_READ | GENERIC_ALL;

/// What to do so only the user may use a file, for refusal messages.
pub const HOW_TO: &str = "make it yours only (icacls <file> /inheritance:r /grant:r \"%USERNAME%:F\" /setowner \"%USERNAME%\")";

/// Whether the file at `path` is owned by someone untrusted, or someone untrusted holds
/// any of `rights` on it: `Ok(true)` also for a file without an access list (everyone
/// may: FAT and exFAT drives), `Err` when the system cannot say.
pub fn others_may(path: &Path, rights: u32) -> Result<bool, String> {
    let user = CurrentUser::get()?;
    let name = wide(path);
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `name` is a null-terminated path; the out pointers are valid and the
    // ones not asked for are null. On success `sd` owns `owner` and `dacl` and is freed
    // below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
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
    // The owner may always change the access list.
    if owner.is_null() || !user.trusted(owner) || dacl.is_null() {
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
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match u32::from(header.AceType) {
            // A denial only takes rights away; ignoring it can only refuse more.
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => continue,
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {}
            // Any other grant (object, compound) is not read: refused.
            _ => return Ok(true),
        }
        let allowed = ace as *const ACCESS_ALLOWED_ACE;
        // SAFETY: an allowed entry and an allowed callback entry share the
        // `ACCESS_ALLOWED_ACE` layout up to `SidStart`, where their SID starts, inside
        // the entry.
        let (mask, sid) = unsafe { ((*allowed).Mask, &raw const (*allowed).SidStart as PSID) };
        if mask & rights != 0 && !user.trusted(sid) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Creates the new file `path` (it must not exist) with an access list of its own that
/// allows only the current user and the system, nothing inherited: the counterpart of
/// creating it with mode 0600. Opened for writing and shared with no one while open.
pub fn create_private(path: &Path) -> std::io::Result<File> {
    let sid = CurrentUser::get()
        .and_then(|u| u.string())
        .map_err(std::io::Error::other)?;
    let sd = descriptor(&format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let name = wide(path);
    // SAFETY: `name` is a null-terminated path; `attributes` points to a valid
    // descriptor alive until `sd` drops after the call. CREATE_NEW fails if the file
    // exists, so the list is the one given here.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a valid handle just opened and owned by nothing else; `File` closes it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// A security descriptor from its text form.
fn descriptor(sddl: &str) -> std::io::Result<Local> {
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `text` is null-terminated; `sd` receives a LocalAlloc'd descriptor that
    // the returned `Local` frees.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Local(sd))
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
        // SAFETY: both SIDs are valid: one inside a descriptor alive in the caller,
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

/// The path as a null-terminated wide string. Paths longer than MAX_PATH fail the
/// calls, which refuses the file: safe.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use std::io::Write;

    /// A folder of the test's own, removed when the test ends, even by a panic.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("rbs-acl-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A file made with the access list `sddl` (owner: the current user).
    fn file_with(dir: &Dir, name: &str, sddl: &str) -> std::path::PathBuf {
        let path = dir.0.join(name);
        let sd = descriptor(sddl).unwrap();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let wide = wide(&path);
        // SAFETY: as in `create_private`.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        assert_ne!(h, INVALID_HANDLE_VALUE);
        // SAFETY: a valid handle just opened.
        drop(unsafe { File::from_raw_handle(h) });
        path
    }

    #[test]
    fn a_file_made_private_is_private() {
        let dir = Dir::new("private");
        let path = dir.0.join("secret.toml");
        let mut f = create_private(&path).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);
        assert!(!others_may(&path, CHANGE | READ).unwrap());
    }

    #[test]
    fn grants_to_others_are_refused_and_denials_and_inherit_only_entries_are_not() {
        let dir = Dir::new("grants");
        let me = CurrentUser::get().unwrap().string().unwrap();
        let everyone = file_with(&dir, "everyone", "D:P(A;;FA;;;WD)");
        assert!(others_may(&everyone, CHANGE).unwrap());
        assert!(others_may(&everyone, READ).unwrap());
        let users_read = file_with(
            &dir,
            "users-read",
            &format!("D:P(A;;FA;;;{me})(A;;FA;;;SY)(A;;FR;;;BU)"),
        );
        assert!(!others_may(&users_read, CHANGE).unwrap());
        assert!(others_may(&users_read, READ).unwrap());
        let denied = file_with(&dir, "denied", &format!("D:P(D;;FA;;;AN)(A;;FA;;;{me})"));
        assert!(!others_may(&denied, CHANGE | READ).unwrap());
        let null = file_with(&dir, "null", "D:NO_ACCESS_CONTROL");
        assert!(others_may(&null, CHANGE).unwrap());
    }
}
