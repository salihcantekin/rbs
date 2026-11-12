//! Quitting the app on macOS (ADR-0055 decision 5).
//!
//! The app menu's Quit item (Cmd+Q), Dock > Quit and logout all send `terminate:` to
//! `NSApplication`. Slint 1.18 always builds that menu with the system Quit item, and
//! winit's application delegate does not answer `applicationShouldTerminate:`, so AppKit
//! ends the process with `exit` from inside the event loop: the window's close request,
//! its "End the stream and quit?" question, the last save and the studio's shutdown
//! never run. [`route_quit`] adds `applicationShouldTerminate:` to winit's delegate
//! class: it cancels the system's termination and calls the app's own quit path, which
//! ends the event loop so that `main` returns.

use std::ffi::c_char;
use std::sync::OnceLock;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};

/// `NSApplicationTerminateReply` values.
const TERMINATE_CANCEL: usize = 0;
const TERMINATE_NOW: usize = 1;

/// The app's quit path, set once by [`route_quit`].
static QUIT: OnceLock<fn()> = OnceLock::new();

/// `applicationShouldTerminate:`: runs the app's quit path instead of terminating.
extern "C" fn should_terminate(_this: *mut AnyObject, _cmd: Sel, _sender: *mut AnyObject) -> usize {
    let Some(quit) = QUIT.get() else {
        return TERMINATE_NOW;
    };
    // A panic must not unwind into AppKit: that aborts. When the quit path fails, the
    // system's termination goes ahead as before.
    match rbs_crash::contain(*quit) {
        Ok(()) => TERMINATE_CANCEL,
        Err(_) => TERMINATE_NOW,
    }
}

/// Makes `terminate:` call `quit` instead of ending the process. Call on the main thread
/// once the toolkit's event loop exists (it sets the application delegate), before it
/// runs. `quit` runs inside AppKit's `terminate:`; it should only queue the app's quit
/// path on the event loop. Returns false when nothing was routed: no delegate, the
/// delegate already answers `applicationShouldTerminate:`, or a second call.
pub fn route_quit(quit: fn()) -> bool {
    if QUIT.set(quit).is_err() {
        return false;
    }
    // SAFETY: `NSApplication.sharedApplication` and `-delegate` are plain getters; the
    // caller is on the main thread (the toolkit's event loop thread), where AppKit
    // objects may be used. Both may be nil, checked below.
    let delegate: *mut AnyObject = unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return false;
        }
        msg_send![app, delegate]
    };
    // SAFETY: `delegate` is non-null (checked) and owned by the application, which keeps
    // it alive for the whole process.
    let Some(delegate) = (unsafe { delegate.as_ref() }) else {
        return false;
    };
    let class: &AnyClass = delegate.class();
    let types = c"Q@:@".as_ptr().cast::<c_char>();
    // SAFETY: `should_terminate` has exactly the signature `types` describes (returns
    // NSUInteger `Q`, takes self `@`, the selector `:` and the sender `@`), which is the
    // signature of `applicationShouldTerminate:`. The C ABI function never unwinds (the
    // quit path runs under `contain`). `class_addMethod` only adds the method when the
    // class does not define it, so winit's own methods are untouched; classes are
    // process-wide and live forever.
    let added = unsafe {
        let imp: Imp = std::mem::transmute::<
            extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
            Imp,
        >(should_terminate);
        objc2::ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel!(applicationShouldTerminate:),
            imp,
            types,
        )
    };
    added.as_bool()
}

#[cfg(test)]
mod tests {
    use objc2::sel;

    use super::*;

    #[test]
    fn should_terminate_returns_now_when_the_quit_path_panics() {
        assert!(QUIT.set(|| panic!("quit path")).is_ok());
        let reply = should_terminate(
            std::ptr::null_mut(),
            sel!(applicationShouldTerminate:),
            std::ptr::null_mut(),
        );
        assert_eq!(reply, TERMINATE_NOW);
    }
}
