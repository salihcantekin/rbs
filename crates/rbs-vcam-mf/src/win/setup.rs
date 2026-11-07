//! `rbs-vcam-setup.exe`: the one elevated step (ADR-0070 decision 5).
//!
//! `register <SID>` writes the user's class key under `HKLM` and the user's format
//! file with its access list; `unregister <SID>` deletes both. The SID must be a user
//! account and the user signed in to the tool's own session, whoever approved the
//! prompt. Nothing else is accepted.

use std::path::Path;

use rbs_vcam_mf_source::announced::{Announced, FORMAT_LEN};
use rbs_vcam_mf_source::clsid::is_user_sid;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    LookupAccountNameW, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, SidTypeUser,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows_core::{BOOL, HSTRING, PCWSTR, PWSTR};

use super::sys::{
    FRIENDLY_NAME, SOURCE_DLL, class_key, format_dir, format_path, install_dir, sid_string,
};

/// Exit codes.
pub const OK: i32 = 0;
/// Wrong arguments.
pub const USAGE: i32 = 2;
/// The SID is not the session's user account.
pub const REFUSED: i32 = 3;
/// A system call failed.
pub const FAILED: i32 = 4;

/// The tool's `main`: returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let [verb, sid] = args else {
        return USAGE;
    };
    if !matches!(verb.as_str(), "register" | "unregister") {
        return USAGE;
    }
    if !is_user_sid(sid) || session_user().as_deref() != Some(sid.as_str()) {
        return REFUSED;
    }
    let done = if verb == "register" {
        register(sid)
    } else {
        unregister(sid)
    };
    if done.is_some() { OK } else { FAILED }
}

fn wts_string(class: WTS_INFO_CLASS) -> Option<String> {
    let mut buf = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: queries our own session; `buf` is freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            class,
            &mut buf,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: a null-terminated string from the call above.
    let s = unsafe { buf.to_string() }.ok();
    // SAFETY: allocated by `WTSQuerySessionInformationW`, freed once.
    unsafe { WTSFreeMemory(buf.0.cast()) };
    s
}

/// The SID string of the user signed in to this process's session, if it is a user
/// account (`SidTypeUser`).
fn session_user() -> Option<String> {
    let name = wts_string(WTSUserName)?;
    let domain = wts_string(WTSDomainName)?;
    if name.is_empty() {
        return None;
    }
    let account = HSTRING::from(format!("{domain}\\{name}"));
    let mut sid = vec![0u64; 16];
    let mut sid_len = (sid.len() * 8) as u32;
    let mut dom = [0u16; 256];
    let mut dom_len = dom.len() as u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: `sid` holds `sid_len` bytes (8-aligned), `dom` holds `dom_len` characters.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            &account,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_len,
            Some(PWSTR(dom.as_mut_ptr())),
            &mut dom_len,
            &mut kind,
        )
    }
    .ok()?;
    if kind != SidTypeUser {
        return None;
    }
    sid_string(PSID(sid.as_mut_ptr().cast()))
}

fn reg_set(key: HKEY, name: Option<&str>, value: &str) -> Option<()> {
    let name = name.map(HSTRING::from);
    let name = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
    let data: Vec<u8> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: a `REG_SZ` with its terminating null on our open key.
    (unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(&data)) } == ERROR_SUCCESS).then_some(())
}

fn reg_create(path: &str) -> Option<HKEY> {
    let path = HSTRING::from(path);
    let mut key = HKEY::default();
    // SAFETY: creates or opens a key in the 64-bit view; closed by the caller.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_WOW64_64KEY,
            None,
            &mut key,
            None,
        )
    };
    (r == ERROR_SUCCESS).then_some(key)
}

fn close(key: HKEY) {
    // SAFETY: a key we opened, closed once.
    let _ = unsafe { RegCloseKey(key) };
}

/// Sets `path`'s owner and protected access list from `sddl` (`O:..D:P...`).
fn secure(path: &Path, sddl: &str) -> Option<()> {
    let sddl = HSTRING::from(sddl);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a valid null-terminated SDDL string; `psd` is LocalAlloc'd, freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None)
    }
    .ok()?;
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let (mut defaulted, mut present) = (BOOL::default(), BOOL::default());
    // SAFETY: both point into `psd`, alive until it is freed below.
    let parts = unsafe {
        GetSecurityDescriptorOwner(psd, &mut owner, &mut defaulted)
            .and_then(|()| GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted))
    };
    let name = HSTRING::from(path.as_os_str());
    let r = parts.ok().map(|()| {
        // SAFETY: a null-terminated path; owner and access list from `psd`.
        unsafe {
            SetNamedSecurityInfoW(
                &name,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(owner),
                None,
                Some(dacl),
                None,
            )
        }
    });
    // SAFETY: allocated by the conversion above, freed once.
    unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    (r == Some(ERROR_SUCCESS)).then_some(())
}

/// `FILE_GENERIC_READ | FILE_WRITE_DATA`: what the user may do with their format file.
const USER_FILE_ACCESS: u32 = 0x0012_0089 | 0x0002;

fn register(sid: &str) -> Option<()> {
    let dll = install_dir()?.join(SOURCE_DLL);
    let key = class_key(sid);
    let class = reg_create(&key)?;
    let ok =
        reg_set(class, None, FRIENDLY_NAME).and_then(|()| reg_set(class, Some("RbsVcamUser"), sid));
    close(class);
    ok?;
    let server = reg_create(&format!("{key}\\InprocServer32"))?;
    let ok = reg_set(server, None, &dll.to_string_lossy())
        .and_then(|()| reg_set(server, Some("ThreadingModel"), "Both"));
    close(server);
    ok?;

    let dir = format_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    secure(
        &dir,
        "O:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;FR;;;WD)",
    )?;
    let file = format_path(sid)?;
    let valid = std::fs::read(&file)
        .is_ok_and(|b| b.len() == FORMAT_LEN && Announced::from_value(&b).to_value()[..] == b[..]);
    if !valid {
        std::fs::write(&file, Announced::default().to_value()).ok()?;
    }
    secure(
        &file,
        &format!("O:BAD:P(A;;FA;;;BA)(A;;FA;;;SY)(A;;FR;;;LS)(A;;0x{USER_FILE_ACCESS:x};;;{sid})"),
    )
}

fn unregister(sid: &str) -> Option<()> {
    let key = HSTRING::from(class_key(sid));
    // SAFETY: deletes our class key and its subkeys (64-bit process: the 64-bit view).
    let r = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &key) };
    if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
        return None;
    }
    match std::fs::remove_file(format_path(sid)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => None,
        _ => Some(()),
    }
}
