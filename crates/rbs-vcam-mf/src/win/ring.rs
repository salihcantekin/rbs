//! Maps the section the media source created (`Global\org.rbs.vcam.<SID>`), after
//! checking that the source made it: its owner must be `LocalService`, the account
//! Frame Server runs the source in. A section of that name made by anyone else never
//! gets frames.

use std::ptr::NonNull;

use rbs_vcam_ring::Region;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID,
};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
};
use windows_core::HSTRING;

use super::sys::sid_string;
use crate::sender::{Keep, RingSource};

/// `READ_CONTROL`: read the object's security descriptor (its owner).
const READ_CONTROL: u32 = 0x0002_0000;

/// `ACCESS_ALLOWED_ACE_TYPE`.
const ACCESS_ALLOWED: u8 = 0;

/// `LocalService`.
const LOCAL_SERVICE: &str = "S-1-5-19";

/// The section's name for `sid` (as the source names it).
pub fn section_name(sid: &str) -> String {
    format!("Global\\org.rbs.vcam.{sid}")
}

/// An open, mapped section; unmapped and closed on drop.
struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

// SAFETY: a process-wide handle and view with no thread affinity; the bytes are only
// accessed through `Region`'s atomics and the writer's slot protocol.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: our own view and handle, each released once; every `Region` over the
        // view is dropped before this (the sender drops its writer first).
        unsafe {
            if !self.view.Value.is_null() {
                let _ = UnmapViewOfFile(self.view);
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Who must have made a section: its owner, and the only accounts its access list
/// may name.
#[derive(Debug, Clone)]
struct Trust {
    owner: String,
    allowed: Vec<String>,
}

impl Trust {
    /// The source's section for `sid`: owned by `LocalService`, granting only
    /// `LocalService` and the user (the access list the source sets).
    fn source(sid: &str) -> Self {
        Self {
            owner: LOCAL_SERVICE.to_owned(),
            allowed: vec![LOCAL_SERVICE.to_owned(), sid.to_owned()],
        }
    }
}

/// The SID strings of `dacl`'s entries; `None` when it is missing or holds anything
/// other than access-allowed entries.
fn dacl_sids(dacl: *mut ACL) -> Option<Vec<String>> {
    if dacl.is_null() {
        return None;
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` points into a descriptor the caller keeps alive for this call.
    unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .ok()?;
    (0..info.AceCount)
        .map(|i| {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `i` is below the list's entry count.
            unsafe { GetAce(dacl, i, &mut ace) }.ok()?;
            if ace.is_null() {
                return None;
            }
            // SAFETY: every entry starts with an `ACE_HEADER`.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED {
                return None;
            }
            // SAFETY: an access-allowed entry; its SID starts at `SidStart`.
            let sid = PSID(unsafe { &raw mut (*(ace as *mut ACCESS_ALLOWED_ACE)).SidStart }.cast());
            sid_string(sid)
        })
        .collect()
}

/// Whether the section behind `handle` has `trust`'s owner and names no other account
/// in its access list than `trust` allows (a squatter's section gets no frames).
fn trusted(handle: HANDLE, trust: &Trust) -> bool {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: queries our own handle; `owner` and `dacl` point into `psd`, freed below.
    let r = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut psd),
        )
    };
    let ok = r.is_ok()
        && !owner.is_invalid()
        && sid_string(owner).as_deref() == Some(trust.owner.as_str())
        && dacl_sids(dacl).is_some_and(|sids| sids.iter().all(|s| trust.allowed.contains(s)));
    if !psd.0.is_null() {
        // SAFETY: allocated by `GetSecurityInfo`, freed once.
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    }
    ok
}

/// Opens and maps the section of `sid`'s camera, if it exists and the source made it.
pub fn open(sid: &str) -> Option<(Region, Keep)> {
    open_trusted(&section_name(sid), Some(&Trust::source(sid)))
}

/// Opens and maps `name` if it exists and `trust` (when given) holds for it.
fn open_trusted(name: &str, trust: Option<&Trust>) -> Option<(Region, Keep)> {
    let name = HSTRING::from(name);
    // `READ_CONTROL`, so `trusted` can read the owner and the access list.
    let access = FILE_MAP_READ.0 | FILE_MAP_WRITE.0 | READ_CONTROL;
    // SAFETY: opens a named section with read, write and read-control access (what the
    // source's access list grants the user).
    let handle = unsafe { OpenFileMappingW(access, false, &name) }.ok()?;
    let mut mapping = Mapping {
        handle,
        view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
    };
    if trust.is_some_and(|t| !trusted(handle, t)) {
        return None;
    }
    // SAFETY: maps the whole section we hold a handle to.
    mapping.view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0) };
    let ptr = NonNull::new(mapping.view.Value.cast::<u8>())?;
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: describes our own view.
    let n = unsafe {
        VirtualQuery(
            Some(ptr.as_ptr().cast_const().cast()),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 {
        return None;
    }
    // SAFETY: `ptr` maps `RegionSize` bytes (the view, rounded to pages, all of it
    // backed by the section), valid until `mapping` is dropped, which the sender does
    // only after the regions over it.
    let region = unsafe { Region::from_raw(ptr, info.RegionSize) }.ok()?;
    Some((region, Box::new(mapping)))
}

/// The sender's ring source on Windows.
pub struct WinRing {
    name: String,
    trust: Option<Trust>,
}

impl WinRing {
    /// The ring of `sid`'s camera.
    pub fn new(sid: &str) -> Self {
        Self {
            name: section_name(sid),
            trust: Some(Trust::source(sid)),
        }
    }

    /// Whether the section `name` opens with the writer's access, `owner` owns it and
    /// its access list names only `allowed`.
    #[cfg(test)]
    pub fn trusted_for_test(name: &str, owner: &str, allowed: &[&str]) -> bool {
        let trust = Trust {
            owner: owner.to_owned(),
            allowed: allowed.iter().map(|s| (*s).to_owned()).collect(),
        };
        open_trusted(name, Some(&trust)).is_some()
    }

    /// A ring of any name and owner (the two-process test's section).
    #[cfg(test)]
    pub fn any_owner(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            trust: None,
        }
    }
}

impl RingSource for WinRing {
    fn map(&mut self) -> Option<(Region, Keep)> {
        open_trusted(&self.name, self.trust.as_ref())
    }
}
