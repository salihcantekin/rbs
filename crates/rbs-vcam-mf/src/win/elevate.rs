//! Starts the setup tool with the `runas` verb (the one UAC prompt) and waits for it.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_core::{HSTRING, PCWSTR, w};

/// Runs `exe params` elevated and returns its exit code. A declined prompt is
/// `ERROR_CANCELLED`. Blocks until the tool ends: control thread only.
pub fn run(exe: &Path, params: &str) -> windows_core::Result<u32> {
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised and its strings outlive the call.
    unsafe { ShellExecuteExW(&mut info)? };
    let process = info.hProcess;
    if process.is_invalid() {
        return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
    }
    // SAFETY: the process handle the call returned, closed below.
    let waited = unsafe { WaitForSingleObject(process, INFINITE) };
    let mut code = 1u32;
    if waited == WAIT_OBJECT_0 {
        // SAFETY: as above.
        unsafe { GetExitCodeProcess(process, &mut code)? };
    }
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(process) };
    Ok(code)
}
