//! Opens the AMF runtime once per process, checks its version and keeps it loaded.
//!
//! Windows: `amfrt64.dll` from the system directory only (the AMD driver installs it
//! there; no search of the application or working directory, so a planted DLL is
//! never loaded). Linux: `libamfrt64.so.1` through the dynamic loader (AMD's
//! proprietary driver stack; Mesa has no AMF runtime). A missing library or
//! symbol, or a runtime older than the bindings, makes the adapter unavailable; it
//! never panics.

use std::sync::OnceLock;

use libloading::Library;

use crate::sys;

/// The loaded runtime and its factory.
pub(crate) struct Api {
    /// The process-wide factory. Owned by the runtime (never released).
    pub factory: *mut sys::AMFFactory,
    // Kept loaded for the life of the process: the factory and every object it
    // creates live in this library.
    _lib: Library,
}

// SAFETY: the factory is a process-wide object of the runtime that may be used from
// any thread; it is only read after initialisation.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

#[cfg(target_os = "windows")]
const LIB: &str = "amfrt64.dll";
#[cfg(target_os = "linux")]
const LIB: &str = "libamfrt64.so.1";

fn open(name: &str) -> Result<Library, String> {
    #[cfg(target_os = "windows")]
    {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library as WinLibrary};
        // SAFETY: loading a system DLL runs its initialisation routine. The AMF
        // runtime is designed to be loaded this way by any process, and only the
        // system directory is searched.
        unsafe { WinLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map(Library::from)
            .map_err(|e| format!("{name}: {e}"))
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: loading a shared library runs its initialisers. The AMF runtime is
        // designed to be loaded by any process with `dlopen`.
        unsafe { Library::new(name) }.map_err(|e| format!("{name}: {e}"))
    }
}

fn load() -> Result<Api, String> {
    let lib = open(LIB)?;
    // SAFETY: the prototypes of `AMFQueryVersion_Fn` and `AMFInit_Fn` (core/Factory.h).
    let (query_version, init) = unsafe {
        (
            *lib.get::<sys::AMFQueryVersionFn>(b"AMFQueryVersion\0")
                .map_err(|e| format!("AMFQueryVersion: {e}"))?,
            *lib.get::<sys::AMFInitFn>(b"AMFInit\0")
                .map_err(|e| format!("AMFInit: {e}"))?,
        )
    };
    let mut version = 0u64;
    // SAFETY: a valid out-pointer.
    let r = unsafe { query_version(&mut version) };
    if r != sys::AMF_OK {
        return Err(format!("AMFQueryVersion failed ({r})"));
    }
    if version < sys::AMF_FULL_VERSION {
        return Err(format!(
            "the AMF runtime is {}.{}.{}, the adapter needs {}.{}.{}",
            version >> 48,
            (version >> 32) & 0xffff,
            (version >> 16) & 0xffff,
            sys::AMF_VERSION_MAJOR,
            sys::AMF_VERSION_MINOR,
            sys::AMF_VERSION_RELEASE
        ));
    }
    let mut factory: *mut sys::AMFFactory = std::ptr::null_mut();
    // SAFETY: the version the bindings follow, and a valid out-pointer.
    let r = unsafe { init(sys::AMF_FULL_VERSION, &mut factory) };
    if r != sys::AMF_OK || factory.is_null() {
        return Err(format!("AMFInit failed ({r})"));
    }
    Ok(Api { factory, _lib: lib })
}

/// The runtime, loaded on first use. `Err` explains why AMF is unavailable.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(e) => Err(e.as_str()),
    }
}
