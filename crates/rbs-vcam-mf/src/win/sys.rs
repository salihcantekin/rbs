//! Small Windows helpers shared by the factory and the setup tool: who the user is,
//! where the files are, and plain registry reads (`HKLM`, 64-bit view, query only).

use std::path::PathBuf;

use rbs_vcam_mf_source::{announced, clsid};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_core::{HSTRING, PCWSTR, PWSTR};

/// The media source DLL's file name, installed next to the app.
pub const SOURCE_DLL: &str = "rbs_vcam_mf_source.dll";
/// The setup tool's file name, installed next to the app.
pub const SETUP_EXE: &str = "rbs-vcam-setup.exe";
/// The camera's name in apps.
pub const FRIENDLY_NAME: &str = "RBS Virtual Camera";

/// The string form of `sid`.
pub fn sid_string(sid: PSID) -> Option<String> {
    let mut s = PWSTR::null();
    // SAFETY: `sid` is a valid SID of the caller; `s` is LocalAlloc'd, freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut s) }.ok()?;
    // SAFETY: a null-terminated string from the call above.
    let out = unsafe { s.to_string() }.ok();
    // SAFETY: allocated by `ConvertSidToStringSidW`, freed once.
    unsafe { LocalFree(Some(HLOCAL(s.0.cast()))) };
    out
}

/// The SID string of this process's user.
pub fn current_user() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: our own process token with query access, closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut need = 0u32;
    // SAFETY: a size query.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut need) };
    let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds `need` bytes, 8-byte aligned.
    let r = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            need,
            &mut need,
        )
    };
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(token) };
    r.ok()?;
    // SAFETY: `buf` holds a `TOKEN_USER`; its SID points into `buf`, alive here.
    sid_string(unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid })
}

/// `%ProgramData%` from the shell's known folder.
pub fn program_data() -> Option<PathBuf> {
    // SAFETY: the returned string is CoTaskMemAlloc'd and freed below.
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: a null-terminated string from the call above.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: allocated by `SHGetKnownFolderPath`, freed once.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    s.map(PathBuf::from)
}

/// The user's format file (ADR-0070 decision 3).
pub fn format_path(sid: &str) -> Option<PathBuf> {
    Some(program_data()?.join(announced::format_file(sid)))
}

/// The folder of the format files.
pub fn format_dir() -> Option<PathBuf> {
    Some(program_data()?.join("RBS").join("VirtualCamera"))
}

/// The user's class id in registry form.
pub fn class_id(sid: &str) -> String {
    clsid::braced(&clsid::for_sid(sid))
}

/// The user's class key under `HKLM`.
pub fn class_key(sid: &str) -> String {
    format!("SOFTWARE\\Classes\\CLSID\\{}", class_id(sid))
}

/// The folder of this executable (the app or the setup tool).
pub fn install_dir() -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    // SAFETY: our own module's path into a buffer of the largest path length.
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let exe = PathBuf::from(String::from_utf16(&buf[..n]).ok()?);
    exe.parent().map(PathBuf::from)
}

/// A `REG_SZ` (expanded) value under `HKLM`, 64-bit view.
pub fn hklm_string(key: &str, name: Option<&str>) -> Option<String> {
    let key = HSTRING::from(key);
    let name = name.map(HSTRING::from);
    let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
    let mut buf = [0u16; 1024];
    let mut len = size_of_val(&buf) as u32;
    // SAFETY: null-terminated strings; `buf` holds `len` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            name_ptr,
            REG_ROUTINE_FLAGS(RRF_RT_REG_SZ.0 | RRF_SUBKEY_WOW6464KEY.0),
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    Some(
        String::from_utf16(&buf[..chars])
            .ok()?
            .trim_end_matches('\0')
            .to_owned(),
    )
}

/// Whether two paths name the same file, compared case-insensitively after making
/// both canonical.
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    canon(a) == canon(b)
}
