//! A second launch asks the running RBS to show its main window (ADR-0181 decision 3).
//! Low-level module: AppKit and Foundation calls, each `unsafe` block with its SAFETY
//! comment. Runs in the launch, before any toolkit exists, on its main thread.

use std::ffi::CStr;
use std::path::Path;

use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};

/// `kCoreEventClass` ('aevt') and `kAEReopenApplication` ('rapp').
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");
/// `kAutoGenerateReturnID`, `kAnyTransactionID`, `NSAppleEventSendNoReply`.
const AUTO_RETURN_ID: i16 = -1;
const ANY_TRANSACTION: i32 = 0;
const SEND_NO_REPLY: usize = 1;
/// Seconds one send may block; the launch retries every 500 ms anyway.
const SEND_TIMEOUT: f64 = 0.5;

/// Asks the app with process id `pid` to reopen, which shows a hidden main window and
/// brings a shown one forward (`applicationShouldHandleReopen:`). Only when that
/// process runs `exe`, so a reused pid is never asked. True when the request was
/// handed to the system. Runs inside its own autorelease pool: the launch calls it
/// every retry, before `NSApplication` exists, so nothing else would drain one.
pub fn reopen_running(pid: u32, exe: &Path) -> bool {
    objc2::rc::autoreleasepool(|_| reopen_in_pool(pid, exe))
}

fn reopen_in_pool(pid: u32, exe: &Path) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: a class method taking a pid_t; returns nil when no app has that pid.
    let app: *mut AnyObject = unsafe {
        msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid]
    };
    // SAFETY: checked for null; the object is autoreleased into the pool of
    // `reopen_running`, which drains after this function returns.
    let Some(app) = (unsafe { app.as_ref() }) else {
        return false;
    };
    // SAFETY: plain getters of a live NSRunningApplication; each may return nil.
    let (exe_url, bundle_url): (*mut AnyObject, *mut AnyObject) =
        unsafe { (msg_send![app, executableURL], msg_send![app, bundleURL]) };
    // Both sides canonical, so a symlink or `/var` vs `/private/var` spelling matches.
    let running = url_path(exe_url).and_then(|p| std::fs::canonicalize(p).ok());
    if running.is_none() || running != std::fs::canonicalize(exe).ok() {
        return false;
    }
    // The reopen Apple event goes to this pid alone. Opening the bundle URL is not
    // enough on its own: after `open -n` the launch is itself an instance of the
    // bundle, and Launch Services may hand the reopen to it, so a hidden or minimized
    // RBS stayed hidden (Mac check, 2026-11-24).
    if send_reopen(pid) {
        return true;
    }
    // Refused (Automation consent): unhide the app, which needs no consent, and let
    // Launch Services reopen it.
    // SAFETY: `unhide` of a live NSRunningApplication returns BOOL.
    let _: Bool = unsafe { msg_send![app, unhide] };
    match url_path(bundle_url) {
        Some(bundle) if bundle.extension().is_some_and(|e| e == "app") => open_url(bundle_url),
        _ => false,
    }
}

/// The file path of an `NSURL`, or `None` for nil or a URL without one.
fn url_path(url: *mut AnyObject) -> Option<std::path::PathBuf> {
    // SAFETY: checked for null; an NSURL passed in by the caller, alive for the call.
    let url = unsafe { url.as_ref() }?;
    // SAFETY: `path` returns an NSString or nil; `UTF8String` a NUL-terminated buffer
    // owned by that string, which lives until the pool of `reopen_running` drains.
    let text: *const std::ffi::c_char = unsafe {
        let path: *mut AnyObject = msg_send![url, path];
        let path = path.as_ref()?;
        msg_send![path, UTF8String]
    };
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated (above).
    let text = unsafe { CStr::from_ptr(text) };
    Some(std::path::PathBuf::from(text.to_str().ok()?))
}

fn open_url(url: *mut AnyObject) -> bool {
    // SAFETY: `sharedWorkspace` is a class getter; `openURL:` takes a non-null NSURL
    // (checked by the caller through `url_path`) and returns BOOL.
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let Some(workspace) = workspace.as_ref() else {
            return false;
        };
        let opened: Bool = msg_send![workspace, openURL: url];
        opened.as_bool()
    }
}

fn send_reopen(pid: i32) -> bool {
    // SAFETY: Foundation class methods with the argument types of their declarations
    // (pid_t, AEEventClass, AEEventID, AEReturnID, AETransactionID, an
    // NSAppleEventDescriptor target); `sendEventWithOptions:timeout:error:` returns nil
    // and sets `error` on failure. Every object is autoreleased into the pool of
    // `reopen_running`. The short timeout keeps a pending Automation prompt from
    // stretching the launch's 15-second wait by much.
    unsafe {
        let target: *mut AnyObject = msg_send![
            class!(NSAppleEventDescriptor),
            descriptorWithProcessIdentifier: pid
        ];
        if target.is_null() {
            return false;
        }
        let event: *mut AnyObject = msg_send![
            class!(NSAppleEventDescriptor),
            appleEventWithEventClass: CORE_EVENT_CLASS,
            eventID: REOPEN_APPLICATION,
            targetDescriptor: target,
            returnID: AUTO_RETURN_ID,
            transactionID: ANY_TRANSACTION
        ];
        let Some(event) = event.as_ref() else {
            return false;
        };
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let reply: *mut AnyObject = msg_send![
            event,
            sendEventWithOptions: SEND_NO_REPLY,
            timeout: SEND_TIMEOUT,
            error: &mut error
        ];
        !reply.is_null() && error.is_null()
    }
}
