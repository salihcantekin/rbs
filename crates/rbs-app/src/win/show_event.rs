//! A second launch brings the running RBS forward (ADR-0056 decision 6).
//!
//! While Keep RBS running is set, the running instance owns a named, per-user event
//! (`Local\RBS.Show.<user SID>`) and waits on it in the system's wait pool
//! (`RegisterWaitForSingleObject`): no thread of RBS's own, no polling. A second launch
//! that finds the event lets the first take the foreground, signals it and exits.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW, OpenProcessToken,
    RegisterWaitForSingleObject, SetEvent, UnregisterWaitEx, WT_EXECUTEDEFAULT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

/// The app's show path, set by the first [`ShowEvent::create`].
static SHOW: OnceLock<fn()> = OnceLock::new();

/// The event's name as a NUL-terminated wide string, or `None` when the user's SID
/// cannot be read (then there is no way back through a second launch).
fn name() -> Option<Vec<u16>> {
    let sid = user_sid()?;
    Some(
        format!("Local\\RBS.Show.{sid}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect(),
    )
}

/// The current user's SID as text (`S-1-5-21-…`).
fn user_sid() -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no closing;
    // `token` receives a handle we own and close below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // TOKEN_USER plus the SID it points into; 256 bytes hold any SID (at most 68).
    let mut buf = [0u64; 32];
    let mut len = 0u32;
    // SAFETY: `buf` is 256 writable, 8-byte aligned bytes (TOKEN_USER needs pointer
    // alignment) and its size is passed; `token` is the open token handle.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&buf) as u32,
            &mut len,
        )
    };
    // SAFETY: `token` is a handle we own; closed once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success `buf` starts with a TOKEN_USER whose SID pointer points into
    // `buf`, which lives until the end of this function.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID (above); `text` receives a LocalAlloc'd,
    // NUL-terminated wide string that we free with LocalFree.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return None;
    }
    let mut len = 0;
    // SAFETY: `text` is NUL-terminated (ConvertSidToStringSidW's contract), so every
    // unit up to and including the NUL can be read.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL are initialized.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc; freed once.
    unsafe { LocalFree(text.cast::<c_void>()) };
    Some(s)
}

/// Called when the event is signalled; runs on a wait-pool thread. Never unwinds.
unsafe extern "system" fn signalled(_context: *mut c_void, _timed_out: bool) {
    if let Some(show) = SHOW.get() {
        // The show path only queues work on the UI thread.
        let _ = rbs_crash::contain(*show);
    }
}

/// A second launch calls this first: when an RBS that keeps running exists, it is
/// asked to show its window and `true` is returned (the caller then exits).
pub fn signal_running() -> bool {
    let Some(name) = name() else {
        return false;
    };
    // SAFETY: `name` is NUL-terminated and lives for the call; the handle returned is
    // owned here and closed below.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if event.is_null() {
        return false;
    }
    // SAFETY: plain calls; `event` is a valid handle we own, closed once. The running
    // instance may take the foreground for the window it shows.
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
        let set = SetEvent(event);
        CloseHandle(event);
        set != 0
    }
}

/// The named event and its registered wait. Dropping it unregisters the wait (waiting
/// for a callback in progress, which only queues work) and closes the event.
pub struct ShowEvent {
    event: HANDLE,
    wait: HANDLE,
}

impl ShowEvent {
    /// Creates the event and registers the wait; `show` runs on a wait-pool thread
    /// when a second launch signals it, and must only queue work on the UI thread.
    /// `None` when it cannot be set up, or another RBS of this user already owns it.
    pub fn create(show: fn()) -> Option<Self> {
        let _ = SHOW.set(show);
        let name = name()?;
        // SAFETY: auto-reset, initially not signalled, default security; `name` is
        // NUL-terminated and lives for the call.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if event.is_null() {
            return None;
        }
        // SAFETY: read right after CreateEventW on this thread.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: our handle to the existing event; closed once.
            unsafe { CloseHandle(event) };
            return None;
        }
        let mut wait: HANDLE = std::ptr::null_mut();
        // SAFETY: `event` is a valid handle that outlives the wait (both are released
        // in `drop`, the wait first); `signalled` matches WAITORTIMERCALLBACK and
        // never unwinds; the context is unused.
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                event,
                Some(signalled),
                std::ptr::null(),
                INFINITE,
                WT_EXECUTEDEFAULT,
            )
        };
        if ok == 0 {
            // SAFETY: our handle; closed once.
            unsafe { CloseHandle(event) };
            return None;
        }
        Some(Self { event, wait })
    }
}

impl Drop for ShowEvent {
    fn drop(&mut self) {
        // SAFETY: `wait` came from RegisterWaitForSingleObject and is unregistered
        // once; INVALID_HANDLE_VALUE waits for a running callback, which only queues
        // work and so returns at once. Then the event, owned here, is closed once.
        unsafe {
            UnregisterWaitEx(self.wait, INVALID_HANDLE_VALUE);
            CloseHandle(self.event);
        }
    }
}
