//! Opens the NVIDIA driver libraries once per process and keeps them loaded.
//!
//! Windows: `nvEncodeAPI64.dll` and `nvcuda.dll`, from the system directory only (no
//! search of the application or working directory, so a planted DLL is never
//! loaded). Linux: `libnvidia-encode.so.1` and `libcuda.so.1` through the dynamic
//! loader. A missing library, a missing symbol or a driver older than SDK 12.1 makes
//! the adapter unavailable; it never panics.

use std::sync::OnceLock;

use libloading::Library;

use crate::sys::{self, NV_ENCODE_API_FUNCTION_LIST, cuda};

/// The NVENC function table. Filled once by `NvEncodeAPICreateInstance` and only read
/// afterwards.
pub(crate) struct Functions(pub NV_ENCODE_API_FUNCTION_LIST);

// SAFETY: the table holds function pointers into the driver (callable from any
// thread) and reserved null pointers. It is written once before it is shared and
// never mutated afterwards.
unsafe impl Send for Functions {}
// SAFETY: as above; shared access only reads plain pointers.
unsafe impl Sync for Functions {}

/// CUDA driver entry points.
pub(crate) struct Cuda {
    pub init: cuda::InitFn,
    pub device_get_count: cuda::DeviceGetCountFn,
    pub device_get: cuda::DeviceGetFn,
    pub device_get_name: cuda::DeviceGetNameFn,
    pub ctx_create: cuda::CtxCreateFn,
    pub ctx_destroy: cuda::CtxDestroyFn,
    pub ctx_push: cuda::CtxPushCurrentFn,
    pub ctx_pop: cuda::CtxPopCurrentFn,
}

/// The loaded driver.
pub(crate) struct Api {
    pub nvenc: Functions,
    pub cuda: Cuda,
    // Kept loaded for the life of the process: the function pointers above point
    // into these libraries.
    _nvenc_lib: Library,
    _cuda_lib: Library,
}

#[cfg(target_os = "windows")]
const NVENC_LIB: &str = "nvEncodeAPI64.dll";
#[cfg(target_os = "windows")]
const CUDA_LIB: &str = "nvcuda.dll";
#[cfg(target_os = "linux")]
const NVENC_LIB: &str = "libnvidia-encode.so.1";
#[cfg(target_os = "linux")]
const CUDA_LIB: &str = "libcuda.so.1";

fn open(name: &str) -> Result<Library, String> {
    #[cfg(target_os = "windows")]
    {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library as WinLibrary};
        // SAFETY: loading a system DLL runs its initialisation routine. The NVIDIA
        // driver DLLs are designed to be loaded this way by any process, and only the
        // system directory is searched.
        unsafe { WinLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map(Library::from)
            .map_err(|e| format!("{name}: {e}"))
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: loading a shared library runs its initialisers. The NVIDIA driver
        // libraries are designed to be loaded by any process with `dlopen`.
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

fn load() -> Result<Api, String> {
    let nvenc_lib = open(NVENC_LIB)?;
    let cuda_lib = open(CUDA_LIB)?;
    // SAFETY: each type is the prototype from nvEncodeAPI.h / cuda.h (the `_v2` CUDA
    // entry points are the ones the unversioned macros name since CUDA 4.0).
    let (max_version, create_instance, cuda) = unsafe {
        (
            symbol::<sys::MaxVersionFn>(&nvenc_lib, b"NvEncodeAPIGetMaxSupportedVersion\0")?,
            symbol::<sys::CreateInstanceFn>(&nvenc_lib, b"NvEncodeAPICreateInstance\0")?,
            Cuda {
                init: symbol(&cuda_lib, b"cuInit\0")?,
                device_get_count: symbol(&cuda_lib, b"cuDeviceGetCount\0")?,
                device_get: symbol(&cuda_lib, b"cuDeviceGet\0")?,
                device_get_name: symbol(&cuda_lib, b"cuDeviceGetName\0")?,
                ctx_create: symbol(&cuda_lib, b"cuCtxCreate_v2\0")?,
                ctx_destroy: symbol(&cuda_lib, b"cuCtxDestroy_v2\0")?,
                ctx_push: symbol(&cuda_lib, b"cuCtxPushCurrent_v2\0")?,
                ctx_pop: symbol(&cuda_lib, b"cuCtxPopCurrent_v2\0")?,
            },
        )
    };
    let mut driver_api = 0u32;
    // SAFETY: the out-pointer is a valid `u32`.
    let status = unsafe { max_version(&mut driver_api) };
    if status != sys::NV_ENC_SUCCESS {
        return Err(format!(
            "NvEncodeAPIGetMaxSupportedVersion failed ({status})"
        ));
    }
    if driver_api < sys::MIN_DRIVER_API {
        return Err(format!(
            "the NVIDIA driver supports NVENC API {}.{}, the adapter needs 12.1 \
             (driver 531 or newer)",
            driver_api >> 4,
            driver_api & 0xf
        ));
    }
    let mut list = NV_ENCODE_API_FUNCTION_LIST {
        version: sys::NV_ENCODE_API_FUNCTION_LIST_VER,
        ..Default::default()
    };
    // SAFETY: `list` is a zeroed table with its version set, as the API requires.
    let status = unsafe { create_instance(&mut list) };
    if status != sys::NV_ENC_SUCCESS {
        return Err(format!("NvEncodeAPICreateInstance failed ({status})"));
    }
    // SAFETY: `cuInit(0)` has no preconditions and may be called repeatedly.
    let status = unsafe { (cuda.init)(0) };
    if status != cuda::CUDA_SUCCESS {
        return Err(format!("cuInit failed ({status})"));
    }
    Ok(Api {
        nvenc: Functions(list),
        cuda,
        _nvenc_lib: nvenc_lib,
        _cuda_lib: cuda_lib,
    })
}

/// The driver, loaded on first use. `Err` explains why NVENC is unavailable.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    match API.get_or_init(load) {
        Ok(api) => Ok(api),
        Err(e) => Err(e.as_str()),
    }
}
