//! Creating and removing the OS camera (ADR-0070 decision 5), without elevation.
//!
//! `MFCreateVirtualCamera` exists from Windows 11 on. It is looked up at run time, so
//! the app still starts on older Windows and reports the camera as unsupported there.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_VERSION,
    MFSTARTUP_LITE, MFShutdown, MFStartup, MFVirtualCameraAccess,
    MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime, MFVirtualCameraLifetime_System,
    MFVirtualCameraType, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, PWSTR, s, w};

use super::sys::{FRIENDLY_NAME, class_id};

type CreateFn = unsafe extern "system" fn(
    MFVirtualCameraType,
    MFVirtualCameraLifetime,
    MFVirtualCameraAccess,
    PCWSTR,
    PCWSTR,
    *const GUID,
    u32,
    *mut *mut core::ffi::c_void,
) -> HRESULT;

/// `MFCreateVirtualCamera`, if this Windows has it.
fn create_fn() -> Option<CreateFn> {
    // SAFETY: loads a system DLL by name (from System32: it is a known system DLL);
    // the module stays loaded for the process, as the function pointer needs.
    let module: HMODULE = unsafe { LoadLibraryW(w!("mfsensorgroup.dll")) }.ok()?;
    // SAFETY: a named export of that module.
    let f = unsafe { GetProcAddress(module, s!("MFCreateVirtualCamera")) }?;
    // SAFETY: the export has exactly this signature (mfvirtualcamera.h).
    Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateFn>(f) })
}

/// Whether this Windows can have the camera.
pub fn supported() -> bool {
    create_fn().is_some()
}

/// Runs `f` on a short-lived thread in the multithreaded apartment with Media
/// Foundation started, so the caller's thread and apartment are left alone.
fn on_mf_thread<T: Send>(
    f: impl FnOnce() -> windows_core::Result<T> + Send,
) -> windows_core::Result<T> {
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            // SAFETY: this new thread joins the MTA and starts MF; both undone below.
            let co = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // SAFETY: as above.
            let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) };
            let out = mf.clone().and_then(|()| f());
            // SAFETY: balanced with the calls above.
            unsafe {
                if mf.is_ok() {
                    let _ = MFShutdown();
                }
                if co.is_ok() {
                    CoUninitialize();
                }
            }
            out
        });
        worker
            .join()
            .unwrap_or_else(|_| Err(windows::Win32::Foundation::E_UNEXPECTED.into()))
    })
}

/// The camera object for `sid`'s class (creating it does not add a device yet).
fn camera(sid: &str) -> windows_core::Result<IMFVirtualCamera> {
    let create = create_fn().ok_or(windows_core::Error::from(
        windows::Win32::Foundation::E_NOTIMPL,
    ))?;
    let name = HSTRING::from(FRIENDLY_NAME);
    let source = HSTRING::from(class_id(sid));
    let mut out = std::ptr::null_mut();
    // SAFETY: valid null-terminated strings, no categories, an out pointer.
    unsafe {
        create(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_CurrentUser,
            PCWSTR(name.as_ptr()),
            PCWSTR(source.as_ptr()),
            std::ptr::null(),
            0,
            &mut out,
        )
        .ok()?;
        // SAFETY: success hands over one reference to an IMFVirtualCamera.
        Ok(IMFVirtualCamera::from_raw(out))
    }
}

/// Adds `sid`'s camera and returns its symbolic link, if Windows gives it.
pub fn add(sid: &str) -> windows_core::Result<Option<String>> {
    on_mf_thread(|| {
        let cam = camera(sid)?;
        // SAFETY: plain calls on our camera object.
        unsafe {
            cam.Start(None)?;
            let link = symbolic_link(&cam);
            // The device stays: its lifetime is `System`.
            let _ = cam.Shutdown();
            Ok(link)
        }
    })
}

/// Removes `sid`'s camera.
pub fn remove(sid: &str) -> windows_core::Result<()> {
    on_mf_thread(|| {
        let cam = camera(sid)?;
        // SAFETY: plain calls on our camera object.
        unsafe {
            let r = cam.Remove();
            let _ = cam.Shutdown();
            r
        }
    })
}

/// The camera's symbolic link attribute.
fn symbolic_link(cam: &IMFVirtualCamera) -> Option<String> {
    let mut s = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: `s` is CoTaskMemAlloc'd by the call and freed below.
    unsafe {
        cam.GetAllocatedString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            &mut s,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: a null-terminated string from the call above.
    let out = unsafe { s.to_string() }.ok();
    // SAFETY: freed once.
    unsafe { CoTaskMemFree(Some(s.0.cast())) };
    out
}

/// Whether the device interface `link` exists.
pub fn device_exists(link: &str) -> bool {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_Interface_PropertyW, CR_BUFFER_SMALL, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties::{DEVPKEY_Device_InstanceId, DEVPROPTYPE};
    let link = HSTRING::from(link);
    let mut kind = DEVPROPTYPE::default();
    let mut size = 0u32;
    // SAFETY: a size query (no buffer) on a null-terminated interface name.
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(
            &link,
            &DEVPKEY_Device_InstanceId,
            &mut kind,
            None,
            &mut size,
            0,
        )
    };
    r == CR_BUFFER_SMALL || r == CR_SUCCESS
}
