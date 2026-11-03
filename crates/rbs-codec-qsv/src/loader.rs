//! Opens the oneVPL dispatcher (and on Linux libva) once per process and keeps them
//! loaded.
//!
//! Windows: `libvpl.dll`, from the application directory or the system directory
//! only (never the working directory or `PATH`). Intel graphics drivers install the
//! runtime but not the dispatcher, so applications ship `libvpl.dll` (MIT) next to
//! their executable. Linux: `libvpl.so.2` (the `libvpl2` package of current
//! distributions), plus `libva.so.2` and `libva-drm.so.2` for the display the GPU
//! runtime needs. The dispatcher finds the runtime (`libmfx-gen`, or the older
//! `libmfxhw64`) itself. A missing library or symbol makes the adapter unavailable;
//! it never panics.

use std::sync::OnceLock;

use libloading::Library;

use crate::sys;

/// oneVPL dispatcher entry points.
pub(crate) struct Vpl {
    pub load: sys::MFXLoadFn,
    pub unload: sys::MFXUnloadFn,
    pub create_config: sys::MFXCreateConfigFn,
    pub set_filter: sys::MFXSetConfigFilterPropertyFn,
    pub enum_implementations: sys::MFXEnumImplementationsFn,
    pub release_description: sys::MFXDispReleaseImplDescriptionFn,
    pub create_session: sys::MFXCreateSessionFn,
    pub close: sys::MFXCloseFn,
    /// Gives the Linux runtime its VA display.
    #[cfg(target_os = "linux")]
    pub set_handle: sys::MFXVideoCORE_SetHandleFn,
    pub sync: sys::MFXVideoCORE_SyncOperationFn,
    pub query: sys::MFXVideoENCODE_QueryFn,
    pub init: sys::MFXVideoENCODE_ParamFn,
    pub reset: sys::MFXVideoENCODE_ParamFn,
    pub get_video_param: sys::MFXVideoENCODE_ParamFn,
    pub encode_close: sys::MFXVideoENCODE_CloseFn,
    pub encode_frame: sys::MFXVideoENCODE_EncodeFrameAsyncFn,
}

/// libva entry points (Linux).
#[cfg(target_os = "linux")]
pub(crate) struct Va {
    pub get_display_drm: sys::va::GetDisplayDrmFn,
    pub initialize: sys::va::InitializeFn,
    pub terminate: sys::va::TerminateFn,
}

/// The loaded libraries.
pub(crate) struct Api {
    pub vpl: Vpl,
    #[cfg(target_os = "linux")]
    pub va: Va,
    // Kept loaded for the life of the process: the function pointers above point
    // into these libraries.
    _libs: Vec<Library>,
}

fn open(name: &str) -> Result<Library, String> {
    #[cfg(target_os = "windows")]
    {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            Library as WinLibrary,
        };
        // SAFETY: loading a DLL runs its initialisation routine. The oneVPL
        // dispatcher is designed to be loaded by any process, and only the
        // application and system directories are searched.
        unsafe {
            WinLibrary::load_with_flags(
                name,
                LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map(Library::from)
        .map_err(|e| format!("{name}: {e}"))
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: loading a shared library runs its initialisers. libvpl and libva
        // are designed to be loaded by any process with `dlopen`.
        unsafe { Library::new(name) }.map_err(|e| format!("{name}: {e}"))
    }
}

/// Looks up `name` as a function pointer of type `T`.
///
/// # Safety
///
/// `T` must be the exact function pointer type of the symbol.
unsafe fn symbol<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    // SAFETY: the caller guarantees `T` matches the symbol's type.
    unsafe { lib.get::<T>(name) }
        .map(|s| *s)
        .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(name)))
}

#[cfg(target_os = "windows")]
const VPL_LIB: &str = "libvpl.dll";
#[cfg(target_os = "linux")]
const VPL_LIB: &str = "libvpl.so.2";

fn load() -> Result<Api, String> {
    let vpl_lib = open(VPL_LIB)?;
    // SAFETY: each type is the prototype from the oneVPL headers (mfxdispatcher.h,
    // mfxsession.h, mfxvideo.h).
    let vpl = unsafe {
        Vpl {
            load: symbol(&vpl_lib, b"MFXLoad\0")?,
            unload: symbol(&vpl_lib, b"MFXUnload\0")?,
            create_config: symbol(&vpl_lib, b"MFXCreateConfig\0")?,
            set_filter: symbol(&vpl_lib, b"MFXSetConfigFilterProperty\0")?,
            enum_implementations: symbol(&vpl_lib, b"MFXEnumImplementations\0")?,
            release_description: symbol(&vpl_lib, b"MFXDispReleaseImplDescription\0")?,
            create_session: symbol(&vpl_lib, b"MFXCreateSession\0")?,
            close: symbol(&vpl_lib, b"MFXClose\0")?,
            #[cfg(target_os = "linux")]
            set_handle: symbol(&vpl_lib, b"MFXVideoCORE_SetHandle\0")?,
            sync: symbol(&vpl_lib, b"MFXVideoCORE_SyncOperation\0")?,
            query: symbol(&vpl_lib, b"MFXVideoENCODE_Query\0")?,
            init: symbol(&vpl_lib, b"MFXVideoENCODE_Init\0")?,
            reset: symbol(&vpl_lib, b"MFXVideoENCODE_Reset\0")?,
            get_video_param: symbol(&vpl_lib, b"MFXVideoENCODE_GetVideoParam\0")?,
            encode_close: symbol(&vpl_lib, b"MFXVideoENCODE_Close\0")?,
            encode_frame: symbol(&vpl_lib, b"MFXVideoENCODE_EncodeFrameAsync\0")?,
        }
    };
    #[cfg(target_os = "linux")]
    {
        let va_lib = open("libva.so.2")?;
        let drm_lib = open("libva-drm.so.2")?;
        // SAFETY: the prototypes of va.h and va_drm.h.
        let va = unsafe {
            Va {
                get_display_drm: symbol(&drm_lib, b"vaGetDisplayDRM\0")?,
                initialize: symbol(&va_lib, b"vaInitialize\0")?,
                terminate: symbol(&va_lib, b"vaTerminate\0")?,
            }
        };
        Ok(Api {
            vpl,
            va,
            _libs: vec![vpl_lib, va_lib, drm_lib],
        })
    }
    #[cfg(target_os = "windows")]
    Ok(Api {
        vpl,
        _libs: vec![vpl_lib],
    })
}

/// The libraries, loaded on first use. `Err` explains why QSV is unavailable.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(e) => Err(e.as_str()),
    }
}
