//! Opens libva once per process and keeps it loaded.
//!
//! `libva.so.2` and `libva-drm.so.2` through the dynamic loader. libva picks the
//! driver of each render node (`iHD` and `i965` for Intel, `radeonsi` for AMD,
//! `nouveau` has no encoder). A missing library or symbol makes the adapter
//! unavailable; it never panics. `vaSyncBuffer` (libva 2.9) and `vaSyncSurface2`
//! (libva 2.15) are optional.

use std::ffi::CStr;
use std::sync::OnceLock;

use libloading::Library;

use crate::sys;

/// The libva entry points.
pub(crate) struct Api {
    pub get_display_drm: sys::GetDisplayDrmFn,
    pub initialize: sys::InitializeFn,
    pub terminate: sys::TerminateFn,
    pub set_info_callback: sys::SetCallbackFn,
    pub query_vendor_string: sys::QueryVendorStringFn,
    pub max_num_profiles: sys::MaxNumFn,
    pub max_num_entrypoints: sys::MaxNumFn,
    pub max_num_image_formats: sys::MaxNumFn,
    pub query_config_profiles: sys::QueryConfigProfilesFn,
    pub query_config_entrypoints: sys::QueryConfigEntrypointsFn,
    pub get_config_attributes: sys::GetConfigAttributesFn,
    pub create_config: sys::CreateConfigFn,
    pub destroy_config: sys::DestroyConfigFn,
    pub create_surfaces: sys::CreateSurfacesFn,
    pub destroy_surfaces: sys::DestroySurfacesFn,
    pub create_context: sys::CreateContextFn,
    pub destroy_context: sys::DestroyContextFn,
    pub create_buffer: sys::CreateBufferFn,
    pub destroy_buffer: sys::DestroyBufferFn,
    pub map_buffer: sys::MapBufferFn,
    pub unmap_buffer: sys::UnmapBufferFn,
    pub begin_picture: sys::BeginPictureFn,
    pub render_picture: sys::RenderPictureFn,
    pub end_picture: sys::EndPictureFn,
    pub sync_surface: sys::SyncSurfaceFn,
    pub sync_surface2: Option<sys::SyncSurface2Fn>,
    pub sync_buffer: Option<sys::SyncBufferFn>,
    pub query_surface_status: sys::QuerySurfaceStatusFn,
    pub query_image_formats: sys::QueryImageFormatsFn,
    pub create_image: sys::CreateImageFn,
    pub derive_image: sys::DeriveImageFn,
    pub destroy_image: sys::DestroyImageFn,
    pub put_image: sys::PutImageFn,
    pub error_str: sys::ErrorStrFn,
    // Kept loaded for the life of the process: the function pointers above point
    // into these libraries.
    _va: Library,
    _drm: Library,
}

fn open(name: &str) -> Result<Library, String> {
    // SAFETY: loading a shared library runs its initialisers. libva is designed to
    // be loaded by any process with `dlopen`.
    unsafe { Library::new(name) }.map_err(|e| format!("{name}: {e}"))
}

/// Looks up `name` (NUL-terminated) as a `T`.
///
/// # Safety
///
/// `T` must be the function pointer type of the symbol's C prototype.
unsafe fn symbol<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    // SAFETY: the caller guarantees the type.
    unsafe { lib.get::<T>(name) }
        .map(|s| *s)
        .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(name)))
}

fn load() -> Result<Api, String> {
    let va = open("libva.so.2")?;
    let drm = open("libva-drm.so.2")?;
    // SAFETY: every type is the prototype of the symbol in va/va.h, va/va_drm.h.
    unsafe {
        Ok(Api {
            get_display_drm: symbol(&drm, b"vaGetDisplayDRM\0")?,
            initialize: symbol(&va, b"vaInitialize\0")?,
            terminate: symbol(&va, b"vaTerminate\0")?,
            set_info_callback: symbol(&va, b"vaSetInfoCallback\0")?,
            query_vendor_string: symbol(&va, b"vaQueryVendorString\0")?,
            max_num_profiles: symbol(&va, b"vaMaxNumProfiles\0")?,
            max_num_entrypoints: symbol(&va, b"vaMaxNumEntrypoints\0")?,
            max_num_image_formats: symbol(&va, b"vaMaxNumImageFormats\0")?,
            query_config_profiles: symbol(&va, b"vaQueryConfigProfiles\0")?,
            query_config_entrypoints: symbol(&va, b"vaQueryConfigEntrypoints\0")?,
            get_config_attributes: symbol(&va, b"vaGetConfigAttributes\0")?,
            create_config: symbol(&va, b"vaCreateConfig\0")?,
            destroy_config: symbol(&va, b"vaDestroyConfig\0")?,
            create_surfaces: symbol(&va, b"vaCreateSurfaces\0")?,
            destroy_surfaces: symbol(&va, b"vaDestroySurfaces\0")?,
            create_context: symbol(&va, b"vaCreateContext\0")?,
            destroy_context: symbol(&va, b"vaDestroyContext\0")?,
            create_buffer: symbol(&va, b"vaCreateBuffer\0")?,
            destroy_buffer: symbol(&va, b"vaDestroyBuffer\0")?,
            map_buffer: symbol(&va, b"vaMapBuffer\0")?,
            unmap_buffer: symbol(&va, b"vaUnmapBuffer\0")?,
            begin_picture: symbol(&va, b"vaBeginPicture\0")?,
            render_picture: symbol(&va, b"vaRenderPicture\0")?,
            end_picture: symbol(&va, b"vaEndPicture\0")?,
            sync_surface: symbol(&va, b"vaSyncSurface\0")?,
            sync_surface2: symbol(&va, b"vaSyncSurface2\0").ok(),
            sync_buffer: symbol(&va, b"vaSyncBuffer\0").ok(),
            query_surface_status: symbol(&va, b"vaQuerySurfaceStatus\0")?,
            query_image_formats: symbol(&va, b"vaQueryImageFormats\0")?,
            create_image: symbol(&va, b"vaCreateImage\0")?,
            derive_image: symbol(&va, b"vaDeriveImage\0")?,
            destroy_image: symbol(&va, b"vaDestroyImage\0")?,
            put_image: symbol(&va, b"vaPutImage\0")?,
            error_str: symbol(&va, b"vaErrorStr\0")?,
            _va: va,
            _drm: drm,
        })
    }
}

impl Api {
    /// libva's text for `status`.
    pub(crate) fn error(&self, status: sys::VAStatus) -> String {
        // SAFETY: `vaErrorStr` returns a static NUL-terminated string (or null).
        let text = unsafe { (self.error_str)(status) };
        if text.is_null() {
            return format!("VA status {status}");
        }
        // SAFETY: as above.
        let text = unsafe { CStr::from_ptr(text) };
        format!("{} ({status})", text.to_string_lossy())
    }
}

/// libva, loaded on first use. `Err` explains why VA-API is unavailable.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(e) => Err(e.as_str()),
    }
}
