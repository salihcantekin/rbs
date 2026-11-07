//! The activate object `IClassFactory::CreateInstance` returns. Frame Server sets the
//! camera's attributes on it, then asks it for the source.

use std::sync::Mutex;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement};

use super::com::{Alive, guard};
use super::config;
use super::source;

/// The activate object: an attribute store plus the source it creates once.
#[implement(IMFActivate)]
pub(crate) struct Activate {
    /// The class's user.
    sid: String,
    attributes: IMFAttributes,
    source: Mutex<Option<IMFMediaSourceEx>>,
    _alive: Alive,
}

impl Activate {
    /// The activate object for `sid`'s class, with an empty attribute store.
    pub(crate) fn new(sid: String) -> Result<Self> {
        let mut attributes = None;
        // SAFETY: creates an empty attribute store.
        unsafe { MFCreateAttributes(&mut attributes, 4)? };
        Ok(Self {
            sid,
            attributes: attributes.ok_or(windows_core::Error::from(E_POINTER))?,
            source: Mutex::new(None),
            _alive: Alive::new(),
        })
    }

    fn take_source(&self) -> Option<IMFMediaSourceEx> {
        self.source.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        guard(|| {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            let mut slot = self.source.lock().unwrap_or_else(|p| p.into_inner());
            let source = match slot.as_ref() {
                Some(s) => s.clone(),
                None => {
                    // The format file is read at each activation (ADR-0070 decision 3).
                    let config = config::source_config(&self.sid);
                    let s = source::create(&config, Some(&self.attributes))?;
                    *slot = Some(s.clone());
                    s
                }
            };
            drop(slot);
            // SAFETY: both pointers checked non-null; `query` writes an AddRef'd
            // interface or null.
            unsafe { source.query(riid, ppv.cast()).ok() }
        })
    }

    fn ShutdownObject(&self) -> Result<()> {
        guard(|| {
            if let Some(s) = self.take_source() {
                // SAFETY: our own source, shut down once.
                unsafe { s.Shutdown()? };
            }
            Ok(())
        })
    }

    fn DetachObject(&self) -> Result<()> {
        guard(|| {
            drop(self.take_source());
            Ok(())
        })
    }
}

/// Every attribute call goes to the store unchanged.
impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe {
            self.attributes
                .GetItem(key, (!value.is_null()).then_some(value))
        })
    }

    fn GetItemType(&self, key: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetItemType(key) })
    }

    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<BOOL> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.CompareItem(key, value) })
    }

    fn Compare(&self, theirs: Ref<IMFAttributes>, kind: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.Compare(theirs.as_ref(), kind) })
    }

    fn GetUINT32(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetUINT32(key) })
    }

    fn GetUINT64(&self, key: *const GUID) -> Result<u64> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetUINT64(key) })
    }

    fn GetDouble(&self, key: *const GUID) -> Result<f64> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetDouble(key) })
    }

    fn GetGUID(&self, key: *const GUID) -> Result<GUID> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetGUID(key) })
    }

    fn GetStringLength(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetStringLength(key) })
    }

    fn GetString(&self, key: *const GUID, value: PWSTR, size: u32, len: *mut u32) -> Result<()> {
        let a = &self.attributes;
        // The raw call keeps the caller's buffer and length exactly as given.
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe {
            (Interface::vtable(a).GetString)(a.as_raw(), key, value, size, len).ok()
        })
    }

    fn GetAllocatedString(&self, key: *const GUID, value: *mut PWSTR, len: *mut u32) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetAllocatedString(key, value, len) })
    }

    fn GetBlobSize(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetBlobSize(key) })
    }

    fn GetBlob(&self, key: *const GUID, buf: *mut u8, size: u32, len: *mut u32) -> Result<()> {
        let a = &self.attributes;
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { (Interface::vtable(a).GetBlob)(a.as_raw(), key, buf, size, len).ok() })
    }

    fn GetAllocatedBlob(&self, key: *const GUID, buf: *mut *mut u8, len: *mut u32) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetAllocatedBlob(key, buf, len) })
    }

    fn GetUnknown(
        &self,
        key: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        let a = &self.attributes;
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { (Interface::vtable(a).GetUnknown)(a.as_raw(), key, riid, ppv).ok() })
    }

    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetItem(key, value) })
    }

    fn DeleteItem(&self, key: *const GUID) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.DeleteItem(key) })
    }

    fn DeleteAllItems(&self) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.DeleteAllItems() })
    }

    fn SetUINT32(&self, key: *const GUID, value: u32) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetUINT32(key, value) })
    }

    fn SetUINT64(&self, key: *const GUID, value: u64) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetUINT64(key, value) })
    }

    fn SetDouble(&self, key: *const GUID, value: f64) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetDouble(key, value) })
    }

    fn SetGUID(&self, key: *const GUID, value: *const GUID) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetGUID(key, value) })
    }

    fn SetString(&self, key: *const GUID, value: &PCWSTR) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetString(key, *value) })
    }

    fn SetBlob(&self, key: *const GUID, buf: *const u8, size: u32) -> Result<()> {
        let a = &self.attributes;
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { (Interface::vtable(a).SetBlob)(a.as_raw(), key, buf, size).ok() })
    }

    fn SetUnknown(&self, key: *const GUID, value: Ref<IUnknown>) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.SetUnknown(key, value.as_ref()) })
    }

    fn LockStore(&self) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.LockStore() })
    }

    fn UnlockStore(&self) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.UnlockStore() })
    }

    fn GetCount(&self) -> Result<u32> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.GetCount() })
    }

    fn GetItemByIndex(&self, index: u32, key: *mut GUID, value: *mut PROPVARIANT) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe {
            self.attributes
                .GetItemByIndex(index, key, (!value.is_null()).then_some(value))
        })
    }

    fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> Result<()> {
        // SAFETY: the caller's pointers go to our own store unchanged; it checks them.
        guard(|| unsafe { self.attributes.CopyAllItems(dest.as_ref()) })
    }
}
