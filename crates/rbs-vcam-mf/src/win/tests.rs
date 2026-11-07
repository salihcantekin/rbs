//! The two-process ring test (spec, "Conformance tests"): a child process creates and
//! reads a real section, as the media source does; this process maps it through
//! [`WinRing`] and writes with [`RingSender`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_vcam::VcamCounters;
use rbs_vcam_ring::{Layout, Reader, Region, initialize};
use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows_core::HSTRING;

use super::ring::WinRing;
use crate::sender::{RingSender, Sent};

const TEST: &str = "win::tests::a_frame_crosses_processes_through_a_real_section";
const CHILD: &str = "RBS_VCAM_MF_CHILD";
/// Hang guard only: the exchange itself takes milliseconds.
const GUARD: Duration = Duration::from_secs(20);
const W: u32 = 64;
const H: u32 = 36;

fn now_ms() -> u64 {
    // SAFETY: no preconditions.
    unsafe { GetTickCount64() }
}

fn layout() -> Layout {
    Layout::nv12(W, H, W).unwrap()
}

fn frame() -> VideoFrame {
    let format = VideoFormat::new(
        Size {
            width: W,
            height: H,
        },
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let w = W as usize;
    let data: Vec<u8> = (0..w * H as usize * 3 / 2)
        .map(|i| (i % 253) as u8)
        .collect();
    let planes = [
        PlaneLayout {
            offset: 0,
            stride: w,
        },
        PlaneLayout {
            offset: w * H as usize,
            stride: w,
        },
    ];
    let f = SystemFrame::new(format, Bytes::from(data), &planes).unwrap();
    VideoFrame::system(Timestamp::from_nanos(42), f, None).with_sequence(1)
}

/// The child: create the section like the source, read until one frame arrives.
fn child(name: &str) -> ! {
    let l = layout();
    let name = HSTRING::from(name);
    // SAFETY: a pagefile-backed section with default security, closed at exit.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            l.total_len() as u32,
            &name,
        )
    }
    .unwrap();
    // SAFETY: maps the whole section.
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, l.total_len()) };
    let ptr = NonNull::new(view.Value.cast::<u8>()).unwrap();
    // SAFETY: a mapping of `total_len` bytes, alive until the process exits.
    let region = unsafe { Region::from_raw(ptr, l.total_len()) }.unwrap();
    initialize(&region, &l, 1).unwrap();
    let mut reader = Reader::new(region, l).unwrap();
    let expected: Vec<u8> = (0..l.frame_len()).map(|i| (i % 253) as u8).collect();
    let mut out = vec![0u8; l.frame_len()];
    let start = Instant::now();
    let code = loop {
        reader.heartbeat(now_ms());
        if let Ok(Some(info)) = reader.read_newest(now_ms(), &mut out) {
            break if info.timestamp_ns == 42 && out == expected {
                0
            } else {
                3
            };
        }
        if start.elapsed() > GUARD {
            break 4;
        }
        std::thread::yield_now();
    };
    // SAFETY: our own view and handle.
    unsafe {
        let _ = UnmapViewOfFile(view);
        let _ = CloseHandle(handle);
    }
    std::process::exit(code)
}

#[test]
fn a_frame_crosses_processes_through_a_real_section() {
    if let Ok(name) = std::env::var(CHILD) {
        child(&name);
    }
    let name = format!("Local\\org.rbs.vcam.test.{}", std::process::id());
    let mut proc = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD, &name)
        .spawn()
        .unwrap();
    let counters = Arc::new(VcamCounters::default());
    let mut sender = RingSender::new(WinRing::any_owner(&name), layout(), Arc::clone(&counters));
    let f = frame();
    let start = Instant::now();
    let mut written = 0;
    let status = loop {
        if let Some(status) = proc.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < GUARD,
            "hang guard: the child never read a frame"
        );
        if sender.send(&f, now_ms()) == Sent::Written {
            written += 1;
        }
        std::thread::yield_now();
    };
    assert_eq!(status.code(), Some(0), "the child read the frame intact");
    assert!(written >= 1);
}

/// `OWNER RIGHTS`.
const OWNER_RIGHTS: &str = "S-1-3-4";

/// The writer's owner check works with exactly the access the source grants the user:
/// a section whose owner has no implicit rights (an `OWNER RIGHTS` entry) and whose
/// user entry is the source's, opened through `WinRing` with the owner required.
#[test]
fn the_owner_check_works_with_the_access_the_source_grants() {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    let user = super::sys::current_user().unwrap();
    let access = rbs_vcam_mf_source::SECTION_USER_ACCESS;
    // The owner is set to the user (an elevated token would default to Administrators),
    // and its implicit READ_CONTROL is taken away, so only the user entry counts.
    let sddl = HSTRING::from(format!(
        "O:{user}D:P(A;;0x{access:x};;;{user})(A;;0x6;;;OW)"
    ));
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a valid SDDL string; `psd` is freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None)
    }
    .unwrap();
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    let l = layout();
    let name = format!("Local\\org.rbs.vcam.owner-test.{}", std::process::id());
    let wide = HSTRING::from(name.as_str());
    // SAFETY: a pagefile-backed section with the attributes above, closed below.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(&sa),
            PAGE_READWRITE,
            0,
            l.total_len() as u32,
            &wide,
        )
    }
    .unwrap();
    // SAFETY: the descriptor LocalAlloc'd above.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    assert!(
        WinRing::trusted_for_test(&name, &user, &[&user, OWNER_RIGHTS]),
        "owner check failed"
    );
    assert!(!WinRing::trusted_for_test(
        &name,
        "S-1-5-19",
        &["S-1-5-19", &user, OWNER_RIGHTS]
    ));
    // An access list naming another account is refused, whoever owns the section.
    assert!(!WinRing::trusted_for_test(&name, &user, &[&user]));
    // SAFETY: our own handle.
    unsafe {
        let _ = CloseHandle(handle);
    }
}
