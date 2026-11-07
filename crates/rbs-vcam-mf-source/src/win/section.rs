//! The shared frame section (ADR-0066 decision 9, ADR-0070 decision 2): created by the
//! source as `Global\org.rbs.vcam.<SID>`, readable and writable by the source's own
//! account and by that SID only. A section of that name the source did not create is
//! used only when its owner is the source's own account and its access list grants
//! nobody else anything (a squatted name is never read).

use std::ptr::NonNull;

use rbs_vcam_ring::{Layout, Region, initialize};
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_core::{HSTRING, PCWSTR, PWSTR, Result};

use crate::SECTION_USER_ACCESS as USER_ACCESS;
/// `ACCESS_ALLOWED_ACE_TYPE`.
const ACCESS_ALLOWED: u8 = 0;

/// The ring size of the largest format the source offers (ADR-0066 decision 11).
fn largest_len() -> usize {
    crate::announced::SIZES
        .iter()
        .filter_map(|&(w, h)| Layout::nv12(w, h, w).ok())
        .map(|l| l.total_len())
        .max()
        .unwrap_or(0)
}

/// The section's name for `sid`. The in-process test uses the session namespace:
/// creating a `Global\` object needs `SeCreateGlobalPrivilege`, which Frame Server has
/// and a test process may not.
pub(crate) fn name(sid: &str) -> String {
    let space = if cfg!(test) { "Local" } else { "Global" };
    format!("{space}\\org.rbs.vcam.{sid}")
}

/// A mapped section with a freshly initialised ring.
#[derive(Debug)]
pub(crate) struct Section {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// SAFETY: the handle and the view are process-wide kernel resources with no thread
// affinity; access to the mapped bytes goes through `Region`, whose atomics are the
// only shared state.
unsafe impl Send for Section {}

impl Section {
    /// Creates (or takes over, see the module comment) the section for `sid`, maps it
    /// and writes a new ring header for `layout` with a new generation. Not real-time.
    pub(crate) fn create(sid: &str, layout: &Layout) -> Result<(Self, Region)> {
        let own = TokenSid::current()?;
        let own_str = own.to_string()?;
        let user = OwnedSid::parse(sid)?;
        // Protected DACL: the source's own account full access, the user map read and
        // write, nobody else anything.
        let sddl = HSTRING::from(format!(
            "D:P(A;;GA;;;{own_str})(A;;0x{USER_ACCESS:x};;;{sid})"
        ));
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a valid null-terminated SDDL string; `psd` receives a LocalAlloc'd
        // descriptor, freed below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?
        };
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        // Sized for the largest format the source offers, not the announced one: a
        // writer still holding the section from a smaller format keeps it alive, and
        // the next stream must still fit in it.
        let len = layout.total_len().max(largest_len());
        let size = len as u64;
        let name = HSTRING::from(name(sid));
        // SAFETY: a pagefile-backed section (no file handle) with the attributes above,
        // which outlive the call.
        let created = unsafe {
            CreateFileMappingW(
                HANDLE(-1isize as _),
                Some(&sa),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                &name,
            )
        };
        // SAFETY: read right after the call that set it.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: `psd` came from the conversion above and is not used again.
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        let handle = created?;
        let mut section = Self {
            handle,
            view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
            len,
        };
        if existed && !section.trusted(&own, &user)? {
            return Err(E_ACCESSDENIED.into());
        }
        // SAFETY: maps the whole section we hold a handle to.
        let view = unsafe { MapViewOfFile(section.handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        let Some(ptr) = NonNull::new(view.Value.cast::<u8>()) else {
            return Err(windows_core::Error::from_thread());
        };
        // Set in place: a second `Section` copying the handle would close it on drop,
        // and a section whose last handle is closed loses its name.
        section.view = view;
        // SAFETY: `ptr` is a mapping of `len` bytes that stays valid until `section`
        // is dropped; the caller drops every `Region` clone (the reader) before it.
        let region = unsafe { Region::from_raw(ptr, len) }
            .map_err(|_| windows_core::Error::from(E_ACCESSDENIED))?;
        // A new generation every time, so a writer still attached starts over.
        // SAFETY: no preconditions.
        let generation = (unsafe { GetTickCount64() } as u32) | 1;
        initialize(&region, layout, generation)
            .map_err(|_| windows_core::Error::from(E_ACCESSDENIED))?;
        Ok((section, region))
    }

    /// Whether an existing section is one the source itself made: owned by the source's
    /// account, and every access-allowed entry is for that account or the user.
    fn trusted(&self, own: &TokenSid, user: &OwnedSid) -> Result<bool> {
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: queries our own handle; the out pointers point into `psd`, freed below.
        let r = unsafe {
            GetSecurityInfo(
                self.handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                Some(&mut psd),
            )
        };
        if r.is_err() {
            return Ok(false);
        }
        let ok = trusted_parts(owner, dacl, own.sid(), user.sid());
        // SAFETY: `psd` was allocated by `GetSecurityInfo`.
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        Ok(ok)
    }
}

/// The owner and access-list check of [`Section::trusted`].
fn trusted_parts(owner: PSID, dacl: *mut ACL, own: PSID, user: PSID) -> bool {
    // SAFETY: valid SIDs from the descriptor and from our own buffers.
    if owner.is_invalid() || dacl.is_null() || unsafe { EqualSid(owner, own) }.is_err() {
        return false;
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` points into the descriptor, alive for this call.
    let r = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if r.is_err() {
        return false;
    }
    (0..info.AceCount).all(|i| {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `i` is below the ACL's entry count.
        if unsafe { GetAce(dacl, i, &mut ace) }.is_err() || ace.is_null() {
            return false;
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED {
            return false;
        }
        // SAFETY: an access-allowed ACE; its SID starts at `SidStart`.
        let sid = PSID(unsafe { &raw mut (*(ace as *mut ACCESS_ALLOWED_ACE)).SidStart }.cast());
        // SAFETY: valid SIDs.
        unsafe { EqualSid(sid, own) }.is_ok() || unsafe { EqualSid(sid, user) }.is_ok()
    })
}

impl Drop for Section {
    fn drop(&mut self) {
        if !self.view.Value.is_null() {
            // SAFETY: our own view, mapped once; every `Region` over it is gone.
            let _ = unsafe { UnmapViewOfFile(self.view) };
        }
        // SAFETY: our own handle, closed once.
        let _ = unsafe { CloseHandle(self.handle) };
        let _ = self.len;
    }
}

/// The current process's user SID (the source's account: `LocalService` under Frame
/// Server).
pub(crate) struct TokenSid {
    buf: Vec<u64>,
}

impl TokenSid {
    pub(crate) fn current() -> Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: our own process token, query access, closed below.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
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
        r?;
        Ok(Self { buf })
    }

    fn sid(&self) -> PSID {
        // SAFETY: `buf` holds a `TOKEN_USER` written by `GetTokenInformation`; its SID
        // points into `buf`.
        unsafe { (*(self.buf.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    pub(crate) fn to_string(&self) -> Result<String> {
        let mut s = PWSTR::null();
        // SAFETY: a valid SID; `s` is LocalAlloc'd and freed below.
        unsafe { ConvertSidToStringSidW(self.sid(), &mut s)? };
        // SAFETY: a null-terminated string from the call above.
        let out = unsafe { s.to_string() }.map_err(|_| windows_core::Error::from(E_ACCESSDENIED));
        // SAFETY: allocated by `ConvertSidToStringSidW`.
        unsafe { LocalFree(Some(HLOCAL(s.0.cast()))) };
        out
    }
}

/// A SID parsed from its string form.
pub(crate) struct OwnedSid(PSID);

impl OwnedSid {
    /// Parses `sid` (`S-1-...`); an error for anything else.
    pub(crate) fn parse(sid: &str) -> Result<Self> {
        let s = HSTRING::from(sid);
        let mut out = PSID::default();
        // SAFETY: a null-terminated string; `out` is LocalAlloc'd, freed on drop.
        unsafe { ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut out)? };
        Ok(Self(out))
    }

    fn sid(&self) -> PSID {
        self.0
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSidToSidW`, freed once.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}
