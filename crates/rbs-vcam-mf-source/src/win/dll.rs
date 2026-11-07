//! The DLL's COM entry points: `DllGetClassObject` and `DllCanUnloadNow`.
//!
//! The class check of ADR-0070 decision 2 runs in `DllGetClassObject`: the requested
//! CLSID must be an RBS user class in `HKLM`, and the calling process must run as that
//! user, `LocalService` or `SYSTEM`. Anything else gets `CLASS_E_CLASSNOTAVAILABLE`.

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

use super::activate::Activate;
use super::com::{Alive, guard, idle, lock_server};
use super::config::{self, SourceConfig};
use super::section::TokenSid;

/// The class factory for one user's class.
#[implement(IClassFactory)]
struct Factory {
    config: SourceConfig,
    _alive: Alive,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        guard(|| {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: checked non-null; cleared before anything can fail.
            unsafe { *ppv = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let activate: IUnknown = Activate::new(self.config.clone())?.into();
            // SAFETY: both pointers checked; `query` writes an AddRef'd interface or null.
            unsafe { activate.query(riid, ppv.cast()).ok() }
        })
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        guard(|| {
            lock_server(lock.as_bool());
            Ok(())
        })
    }
}

/// The configuration for `clsid` in this process, or `CLASS_E_CLASSNOTAVAILABLE`.
fn config_for(clsid: &GUID) -> Result<SourceConfig> {
    let caller = TokenSid::current()?.to_string()?;
    config::lookup(config::root(), clsid, &caller).ok_or_else(|| CLASS_E_CLASSNOTAVAILABLE.into())
}

/// The class object for `clsid` (ADR-0070 decision 2).
fn class_object(
    clsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> Result<()> {
    if clsid.is_null() || riid.is_null() || ppv.is_null() {
        return Err(E_POINTER.into());
    }
    // SAFETY: checked non-null; cleared before anything can fail.
    unsafe { *ppv = std::ptr::null_mut() };
    // SAFETY: checked non-null; the caller's GUID.
    let config = config_for(unsafe { &*clsid })?;
    let factory: IClassFactory = Factory {
        config,
        _alive: Alive::new(),
    }
    .into();
    // SAFETY: both pointers checked; `query` writes an AddRef'd interface or null.
    unsafe { factory.query(riid, ppv.cast()).ok() }
}

/// COM's entry point for a class this DLL serves.
///
/// # Safety
///
/// COM's contract: `clsid` and `riid` point to GUIDs and `ppv` to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    match guard(|| class_object(clsid, riid, ppv)) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

/// COM's question whether the DLL may be unloaded: only when no object and no server
/// lock is alive.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if idle() { S_OK } else { S_FALSE }
}
