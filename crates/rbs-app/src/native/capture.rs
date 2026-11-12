//! Leave an RBS window out of a screen recording on macOS.
//!
//! `NSWindow.sharingType = NSWindowSharingNone` is what ScreenCaptureKit and the
//! window list honour: the window is not in a screen or window capture. `ReadOnly`
//! (the AppKit default) puts it back. Marked low-level (P11): one AppKit call.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::msg_send;
use objc2::runtime::AnyObject;

/// `NSWindowSharingNone`: the window is omitted from screen capture.
const SHARING_NONE: usize = 0;
/// `NSWindowSharingReadOnly`: the window can be captured (AppKit's default).
const SHARING_READ_ONLY: usize = 1;

/// Sets whether the window that owns `ns_view` is left out of a screen capture.
/// `ns_view` is the live `NSView` of a window this process shows. A view with no
/// window yet changes nothing and returns false, so the caller tries again.
pub fn exclude_from_capture(ns_view: NonNull<c_void>, exclude: bool) -> bool {
    let view = ns_view.as_ptr().cast::<AnyObject>();
    // SAFETY: `ns_view` is the NSView of a window this process created, used on the
    // main thread. `window` is nil until the view is in a window.
    let window: *mut AnyObject = unsafe { msg_send![view, window] };
    if window.is_null() {
        return false;
    }
    let kind = if exclude {
        SHARING_NONE
    } else {
        SHARING_READ_ONLY
    };
    // SAFETY: `window` is non-null and owned by AppKit for this process. The call
    // only sets the sharing flag capture APIs read.
    unsafe {
        let _: () = msg_send![window, setSharingType: kind];
    }
    true
}

/// Drops a saved window frame so the next placement is the one RBS sets, not the
/// place macOS remembered from the last close.
pub fn forget_saved_frame(ns_view: NonNull<c_void>) {
    use objc2::class;
    let view = ns_view.as_ptr().cast::<AnyObject>();
    // SAFETY: same as [`exclude_from_capture`]: the view belongs to this process and
    // this thread. A nil window means the view is not on screen yet.
    let window: *mut AnyObject = unsafe { msg_send![view, window] };
    if window.is_null() {
        return;
    }
    // An empty name clears the autosave frame. A null pointer here has thrown through
    // AppKit while Settings opened from the native menu bar.
    // SAFETY: `window` is this process's NSWindow; `string` is an empty autoreleased
    // NSString from the Foundation class.
    unsafe {
        let empty: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: c"".as_ptr()];
        if !empty.is_null() {
            let _: () = msg_send![window, setFrameAutosaveName: empty];
        }
    }
}
