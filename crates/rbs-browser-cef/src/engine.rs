//! Loads the downloaded browser engine (ADR-0145 decisions 7 and 8, add-ons spec
//! "Browser engine consumer").
//!
//! The runner ships with the app; the CEF runtime is an add-on. The studio names the
//! installed runtime in `RBS_CEF_RUNTIME`, and every CEF process (browser, renderer,
//! GPU, utility, and the macOS helper binary) inherits it. Each process calls
//! [`load`] before `api_hash` or `execute_process`:
//!
//! - Linux: `libcef.so` is a load-time dependency; the studio puts `<lib>` on
//!   `LD_LIBRARY_PATH`, so the loader already brought it in (or the process ended with
//!   127 before `main`). Release builds have no `rpath=$ORIGIN`.
//! - Windows: `libcef.dll` is delay-loaded. This loads it from `<lib>` before the first
//!   CEF call, so the delay-load helper finds it by name.
//! - macOS: `cef_load_library` with the framework under `<lib>`.
//!
//! Then the library's CEF version is compared with the one this runner was built
//! against and with the marker's; a difference exits with `RUNTIME_MISMATCH`.
//!
//! Debug builds without `RBS_CEF_RUNTIME` keep the engine `cef-dll-sys` copied next
//! to the executable (developer runs); release builds never do.

use std::path::{Path, PathBuf};

use cef::sys;
use rbs_browser_ipc::exit;
use rbs_browser_ipc::runtime::{self, Runtime, RuntimeError};

/// Name of the framework's binary inside `<lib>` on macOS.
#[cfg(target_os = "macos")]
const FRAMEWORK: &str = "Chromium Embedded Framework.framework";

/// Where this process found its engine.
#[derive(Debug)]
pub struct Engine {
    /// The downloaded runtime; `None` for a debug build's engine next to the exe.
    runtime: Option<Runtime>,
    /// Keeps the macOS framework loaded for the life of the process.
    #[cfg(target_os = "macos")]
    _loaded: Loaded,
}

impl Engine {
    /// Resources directory for `Settings.resources_dir_path`.
    pub fn resources_dir(&self) -> Option<&Path> {
        self.runtime.as_ref().map(|rt| rt.resources_dir.as_path())
    }

    /// Locales directory for `Settings.locales_dir_path`.
    pub fn locales_dir(&self) -> Option<PathBuf> {
        self.runtime.as_ref().map(Runtime::locales_dir)
    }

    /// The framework directory for `Settings.framework_dir_path` (macOS).
    #[cfg(target_os = "macos")]
    pub fn framework_dir(&self) -> Option<PathBuf> {
        self.runtime
            .as_ref()
            .map(|rt| rt.library_dir.join(FRAMEWORK))
    }
}

/// Finds, loads and checks the engine for this process. `Err` is the exit code.
/// `helper` is the macOS helper binary (it sits deeper in the bundle).
pub fn load(helper: bool) -> Result<Engine, i32> {
    let _ = helper;
    let runtime = match runtime::from_env() {
        Ok(rt) => Some(rt),
        Err(RuntimeError::NotSet) if cfg!(debug_assertions) => None,
        Err(_) => return Err(exit::RUNTIME_MISSING),
    };
    if let Some(rt) = runtime.as_ref()
        && !rt.is_version(
            built_version(sys::CEF_VERSION_MAJOR),
            built_version(sys::CEF_VERSION_MINOR),
            built_version(sys::CEF_VERSION_PATCH),
        )
    {
        return Err(exit::RUNTIME_MISMATCH);
    }
    #[cfg(target_os = "windows")]
    windows::load(runtime.as_ref())?;
    #[cfg(target_os = "macos")]
    let loaded = mac::load(runtime.as_ref(), helper)?;
    check_library_version()?;
    Ok(Engine {
        runtime,
        #[cfg(target_os = "macos")]
        _loaded: loaded,
    })
}

fn built_version(part: i32) -> u32 {
    u32::try_from(part).unwrap_or(u32::MAX)
}

/// The loaded library's own version against the one this runner was built for.
fn check_library_version() -> Result<(), i32> {
    let same = [
        (0, sys::CEF_VERSION_MAJOR),
        (1, sys::CEF_VERSION_MINOR),
        (2, sys::CEF_VERSION_PATCH),
    ]
    .into_iter()
    // SAFETY: `cef_version_info` takes an index and returns an int; it reads only
    // constants of the loaded library, which `load` made sure is loaded (Linux:
    // by the OS loader before `main`).
    .all(|(entry, want)| unsafe { cef_version_info(entry) } == want);
    if same {
        Ok(())
    } else {
        Err(exit::RUNTIME_MISMATCH)
    }
}

// The bindings of the `cef` crate leave out `cef_version_info.h`; the library and the
// macOS loader shim export it (CEF 156: entries 0..2 are major, minor and patch).
unsafe extern "C" {
    fn cef_version_info(entry: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(target_os = "windows")]
mod windows {
    //! `libcef.dll` is delay-loaded (`build.rs`); load it from the runtime first.

    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        AddDllDirectory, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW, SetDefaultDllDirectories,
    };

    use super::{Runtime, exit};

    fn wide(path: &std::path::Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub fn load(runtime: Option<&Runtime>) -> Result<(), i32> {
        // SAFETY: plain flag setters with no pointers.
        unsafe {
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
            SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
        }
        let dir = match runtime {
            Some(rt) => rt.library_dir.clone(),
            // A debug build's engine next to the executable.
            None => {
                let mut exe = std::env::current_exe().map_err(|_| exit::RUNTIME_MISSING)?;
                exe.pop();
                exe
            }
        };
        let dir_w = wide(&dir);
        let dll_w = wide(&dir.join("libcef.dll"));
        // `DLL_LOAD_DIR` searches the folder that holds `libcef.dll` for its
        // siblings (`chrome_elf.dll` and the rest). `DEFAULT_DIRS` alone does not.
        let flags = LOAD_LIBRARY_SEARCH_DEFAULT_DIRS | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR;
        // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the calls.
        // The directory cookie stays for the process; nothing removes it.
        let loaded = unsafe {
            !AddDllDirectory(dir_w.as_ptr()).is_null()
                && !LoadLibraryExW(dll_w.as_ptr(), std::ptr::null_mut(), flags).is_null()
        };
        if loaded {
            Ok(())
        } else {
            Err(exit::RUNTIME_MISSING)
        }
    }
}

/// The macOS framework, unloaded when the process ends.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct Loaded;

#[cfg(target_os = "macos")]
impl Drop for Loaded {
    fn drop(&mut self) {
        let _ = cef::unload_library();
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::os::unix::ffi::OsStrExt;

    use super::{FRAMEWORK, Loaded, Runtime, exit};

    pub fn load(runtime: Option<&Runtime>, helper: bool) -> Result<Loaded, i32> {
        let path = match runtime {
            Some(rt) => rt.library_dir.join(FRAMEWORK),
            // A debug build's bundle: the framework sits in `Contents/Frameworks`.
            None => {
                let exe = std::env::current_exe().map_err(|_| exit::RUNTIME_MISSING)?;
                let up = if helper { "../../.." } else { "../Frameworks" };
                exe.parent()
                    .ok_or(exit::RUNTIME_MISSING)?
                    .join(up)
                    .join(FRAMEWORK)
            }
        }
        .join("Chromium Embedded Framework");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| exit::RUNTIME_MISSING)?;
        // SAFETY: `name` is a NUL-terminated path that outlives the call.
        if unsafe { cef::load_library(Some(&*name.as_ptr().cast())) } == 1 {
            Ok(Loaded)
        } else {
            Err(exit::RUNTIME_MISSING)
        }
    }
}
