//! Maps the section the media source created (`Global\org.rbs.vcam.<SID>`), after
//! checking that the source made it: its owner must be `LocalService`, the account
//! Frame Server runs the source in. A section of that name made by anyone else never
//! gets frames.

use std::ptr::NonNull;

use rbs_vcam_ring::Region;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
};
use windows_core::HSTRING;

use super::sys::sid_string;
use crate::sender::{Keep, RingSource};

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

/// Whether the section behind `handle` is owned by `owner`.
fn owned_by(handle: HANDLE, owner_sid: &str) -> bool {
    let mut owner = PSID::default();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: queries our own handle; `owner` points into `psd`, freed below.
    let r = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut psd),
        )
    };
    let ok = r.is_ok() && !owner.is_invalid() && sid_string(owner).as_deref() == Some(owner_sid);
    if !psd.0.is_null() {
        // SAFETY: allocated by `GetSecurityInfo`, freed once.
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    }
    ok
}

/// Opens and maps `name`, if it exists and the source made it (owned by
/// `LocalService`).
pub fn open(name: &str) -> Option<(Region, Keep)> {
    open_owned(name, Some(LOCAL_SERVICE))
}

/// Opens and maps `name` if it exists and `owner` (when given) owns it.
fn open_owned(name: &str, owner: Option<&str>) -> Option<(Region, Keep)> {
    let name = HSTRING::from(name);
    // SAFETY: opens a named section with read and write access (what the source's
    // access list grants the user).
    let handle =
        unsafe { OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, &name) }.ok()?;
    let mut mapping = Mapping {
        handle,
        view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
    };
    if owner.is_some_and(|o| !owned_by(handle, o)) {
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
    owner: Option<&'static str>,
}

impl WinRing {
    /// The ring of `sid`'s camera.
    pub fn new(sid: &str) -> Self {
        Self {
            name: section_name(sid),
            owner: Some(LOCAL_SERVICE),
        }
    }

    /// A ring of any name and owner (the two-process test's section).
    #[cfg(test)]
    pub fn any_owner(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            owner: None,
        }
    }
}

impl RingSource for WinRing {
    fn map(&mut self) -> Option<(Region, Keep)> {
        open_owned(&self.name, self.owner)
    }
}
