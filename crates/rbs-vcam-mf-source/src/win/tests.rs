//! The in-process COM test (ADR-0066 Consequences, ADR-0070 Tests): loads the source
//! through its class factory, as Frame Server does, against a private copy of `HKLM`.
//!
//! One test function: `HKEY_LOCAL_MACHINE` is redirected for the whole process, and
//! nextest runs each test in its own process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ptr::NonNull;

use rbs_vcam_ring::{Region, WriteOutcome, Writer};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CloseHandle, ERROR_SUCCESS, HANDLE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IClassFactory};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
    UnmapViewOfFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, REG_OPTION_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOverridePredefKey, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows_core::{GUID, HSTRING, Interface};

use super::config::{braced, from_bytes};
use super::dll::DllGetClassObject;
use super::section::{self, TokenSid};
use crate::announced::Announced;
use crate::{clsid, pump, still};

/// A volatile key under `HKCU` standing in for `HKLM` while it lives.
struct FakeHklm {
    name: HSTRING,
    key: HKEY,
}

impl FakeHklm {
    fn new() -> Self {
        let name = HSTRING::from(format!("Software\\RBS\\vcam-test-{}", std::process::id()));
        let mut key = HKEY::default();
        // SAFETY: creates (or opens) our own volatile key; closed on drop.
        let r = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &name,
                None,
                None,
                REG_OPTION_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut key,
                None,
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        // SAFETY: redirects `HKLM` for this process to our key, undone on drop.
        let r = unsafe { RegOverridePredefKey(HKEY_LOCAL_MACHINE, Some(key)) };
        assert_eq!(r, ERROR_SUCCESS);
        Self { name, key }
    }

    /// Writes a class key for `clsid` naming `user`.
    fn class(&self, clsid: &GUID, user: &str) {
        let path = HSTRING::from(format!("SOFTWARE\\Classes\\CLSID\\{}", braced(clsid)));
        let mut key = HKEY::default();
        // SAFETY: creates a subkey of our own key; closed below.
        let r = unsafe {
            RegCreateKeyExW(
                self.key,
                &path,
                None,
                None,
                REG_OPTION_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut key,
                None,
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        let value: Vec<u8> = user
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: a `REG_SZ` with its terminating null, on our key.
        let r = unsafe {
            RegSetValueExW(
                key,
                &HSTRING::from("RbsVcamUser"),
                None,
                REG_SZ,
                Some(&value),
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        // SAFETY: closed once.
        let _ = unsafe { RegCloseKey(key) };
    }
}

impl Drop for FakeHklm {
    fn drop(&mut self) {
        // SAFETY: undoes the redirection, then removes and closes our own key.
        unsafe {
            let _ = RegOverridePredefKey(HKEY_LOCAL_MACHINE, None);
            let _ = RegCloseKey(self.key);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &self.name);
        }
    }
}

fn factory(clsid: &GUID) -> windows_core::Result<IClassFactory> {
    let mut ppv = std::ptr::null_mut();
    // SAFETY: valid GUIDs and out pointer, as COM passes them.
    unsafe { DllGetClassObject(clsid, &IClassFactory::IID, &mut ppv).ok()? };
    // SAFETY: `DllGetClassObject` succeeded, so `ppv` holds an AddRef'd IClassFactory.
    Ok(unsafe { IClassFactory::from_raw(ppv) })
}

fn event(generator: &IMFMediaEventGenerator) -> (u32, IMFMediaEvent) {
    // SAFETY: a non-blocking read of a queue we just filled.
    let e = unsafe { generator.GetEvent(MF_EVENT_FLAG_NO_WAIT) }.expect("an event is queued");
    // SAFETY: a valid event.
    (unsafe { e.GetType() }.unwrap(), e)
}

/// The NV12 bytes of the sample in a `MEMediaSample` event, tightly packed.
fn pixels(e: &IMFMediaEvent, len: usize) -> Vec<u8> {
    // SAFETY: a `MEMediaSample` event carries the sample as its value.
    let sample: IMFSample = unsafe {
        let v: PROPVARIANT = e.GetValue().unwrap();
        windows_core::IUnknown::try_from(&v)
            .unwrap()
            .cast()
            .unwrap()
    };
    // SAFETY: the sample's one buffer.
    let buffer = unsafe { sample.GetBufferByIndex(0) }.unwrap();
    let mut out = vec![0u8; len];
    if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
        // SAFETY: `out` holds the contiguous length, checked first.
        unsafe {
            assert_eq!(two_d.GetContiguousLength().unwrap() as usize, len);
            two_d.ContiguousCopyTo(&mut out).unwrap();
        }
    } else {
        let mut ptr = std::ptr::null_mut();
        let mut cur = 0u32;
        // SAFETY: locks the buffer; copies its current length (checked); unlocks.
        unsafe {
            buffer.Lock(&mut ptr, None, Some(&mut cur)).unwrap();
            assert_eq!(cur as usize, len);
            std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
            buffer.Unlock().unwrap();
        }
    }
    out
}

#[test]
fn the_source_runs_in_process_through_its_class_factory() {
    // SAFETY: this thread joins the multithreaded apartment; MF starts for the test.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
    }
    let user = TokenSid::current().unwrap().to_string().unwrap();
    let hklm = FakeHklm::new();

    // Not an RBS class: refused.
    let unknown = GUID::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    assert_eq!(
        factory(&unknown).err().map(|e| e.code()),
        Some(CLASS_E_CLASSNOTAVAILABLE)
    );
    // A key naming a group SID: refused.
    let group = from_bytes(&clsid::for_sid("S-1-5-32-544"));
    hklm.class(&group, "S-1-5-32-544");
    assert_eq!(
        factory(&group).err().map(|e| e.code()),
        Some(CLASS_E_CLASSNOTAVAILABLE)
    );
    // Another user's class: refused in this process (ADR-0070 decision 2).
    let other_sid = "S-1-5-21-1-2-3-1001";
    let other = from_bytes(&clsid::for_sid(other_sid));
    hklm.class(&other, other_sid);
    if user != other_sid {
        assert_eq!(
            factory(&other).err().map(|e| e.code()),
            Some(CLASS_E_CLASSNOTAVAILABLE)
        );
    }
    // A key whose CLSID is not the one derived from its SID: refused.
    hklm.class(&unknown, &user);
    assert_eq!(
        factory(&unknown).err().map(|e| e.code()),
        Some(CLASS_E_CLASSNOTAVAILABLE)
    );

    // This user's class: the source, as Frame Server gets it.
    let mine = from_bytes(&clsid::for_sid(&user));
    hklm.class(&mine, &user);
    let f = factory(&mine).unwrap();
    // SAFETY: plain COM calls on the objects the DLL returned.
    let source: IMFMediaSourceEx = unsafe {
        let activate: IMFActivate = f.CreateInstance(None).unwrap();
        activate.ActivateObject().unwrap()
    };
    // No format file in ProgramData for this user: the default format.
    let format = Announced::default();
    let frame_len = format.frame_len();

    // SAFETY: plain COM calls on our source.
    let stream: IMFMediaStream = unsafe {
        let pd = source.CreatePresentationDescriptor().unwrap();
        source
            .Start(&pd, &GUID::zeroed(), &PROPVARIANT::default())
            .unwrap();
        let events: IMFMediaEventGenerator = source.cast().unwrap();
        let (t, e) = event(&events);
        assert_eq!(t, MENewStream.0 as u32);
        let v: PROPVARIANT = e.GetValue().unwrap();
        let s = windows_core::IUnknown::try_from(&v)
            .unwrap()
            .cast()
            .unwrap();
        assert_eq!(event(&events).0, MESourceStarted.0 as u32);
        s
    };
    let sgen: IMFMediaEventGenerator = stream.cast().unwrap();
    assert_eq!(event(&sgen).0, MEStreamStarted.0 as u32);

    // Nothing writes the ring yet: the still picture.
    // SAFETY: a sample request on a running stream.
    unsafe { stream.RequestSample(None).unwrap() };
    let (t, e) = event(&sgen);
    assert_eq!(t, MEMediaSample.0 as u32);
    assert!(
        pixels(&e, frame_len) == still::picture(&format),
        "the still picture"
    );

    // The user's writer side: open the section by name and publish one frame.
    let name = HSTRING::from(section::name(&user));
    // SAFETY: opens the section the stream created, read and write as the user.
    let handle: HANDLE =
        unsafe { OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, &name) }
            .expect("the stream created the section (needs SeCreateGlobalPrivilege)");
    let layout = pump::layout(&format).unwrap();
    // SAFETY: maps the whole section; unmapped below after the writer is gone.
    let view: MEMORY_MAPPED_VIEW_ADDRESS = unsafe {
        MapViewOfFile(
            handle,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            layout.total_len(),
        )
    };
    let ptr = NonNull::new(view.Value.cast::<u8>()).unwrap();
    {
        // SAFETY: a mapping of `total_len` bytes that outlives this block.
        let region = unsafe { Region::from_raw(ptr, layout.total_len()) }.unwrap();
        let mut writer = Writer::attach(region, &layout).unwrap();
        let frame: Vec<u8> = (0..frame_len).map(|i| (i % 251) as u8).collect();
        // SAFETY: no preconditions.
        let now = unsafe { GetTickCount64() };
        assert!(matches!(
            writer.write(now, 0, &frame).unwrap(),
            WriteOutcome::Written { .. }
        ));
        // SAFETY: a sample request on a running stream.
        unsafe { stream.RequestSample(None).unwrap() };
        let (t, e) = event(&sgen);
        assert_eq!(t, MEMediaSample.0 as u32);
        assert!(
            pixels(&e, frame_len) == frame,
            "the frame the writer published"
        );
    }
    // SAFETY: our view and handle, released once.
    unsafe {
        let _ = UnmapViewOfFile(view);
        let _ = CloseHandle(handle);
    }

    // Stop and shut down.
    // SAFETY: plain COM calls on our source.
    unsafe {
        source.Stop().unwrap();
        assert_eq!(event(&sgen).0, MEStreamStopped.0 as u32);
        source.Shutdown().unwrap();
        let pd = MFCreatePresentationDescriptor(None).unwrap();
        assert!(
            source
                .Start(&pd, &GUID::zeroed(), &PROPVARIANT::default())
                .is_err()
        );
    }
    drop((stream, sgen, source, f));
    // SAFETY: matches `MFStartup` above.
    unsafe { MFShutdown().unwrap() };
    drop(hklm);
}
