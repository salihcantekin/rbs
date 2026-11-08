//! Keeping RBS running without a window on macOS (ADR-0056 decisions 6, 7 and 8).
//!
//! - [`route_show`]: a click on the Dock icon (`applicationShouldHandleReopen:`) and the
//!   first activation after RBS was left while its windows were hidden (Cmd+Tab) bring
//!   the main window back. Like [`super::route_quit`], the methods are added to winit's
//!   application delegate class with `class_addMethod`, which never replaces a method
//!   the class already has; the activation methods have RBS's own selectors and are
//!   registered with the notification center.
//! - [`set_badge`] and [`request_attention`]: the live state and problems on the Dock
//!   tile while no window shows.

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{class, msg_send, sel};

/// The app's show path, set once by [`route_show`].
static SHOW: OnceLock<fn()> = OnceLock::new();
/// The windows are hidden by Close (set by the app through [`set_hidden`]).
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// RBS stopped being the active app while hidden: its next activation shows the window.
static LEFT: AtomicBool = AtomicBool::new(false);

/// `NSRequestUserAttentionType.informationalRequest`: the Dock icon bounces once.
const INFORMATIONAL_REQUEST: isize = 10;

/// `BOOL` in Objective-C type encodings: `B` on arm64, `c` (signed char) on x86_64.
#[cfg(target_arch = "aarch64")]
const REOPEN_TYPES: &CStr = c"B@:@B";
#[cfg(not(target_arch = "aarch64"))]
const REOPEN_TYPES: &CStr = c"c@:@c";

fn show() {
    if let Some(show) = SHOW.get() {
        // A panic must not unwind into AppKit: that aborts.
        let _ = rbs_crash::contain(*show);
    }
}

/// `applicationShouldHandleReopen:hasVisibleWindows:`: shows the main window while it is
/// hidden; otherwise AppKit does what it always does.
extern "C" fn should_reopen(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut AnyObject,
    _visible: Bool,
) -> Bool {
    if HIDDEN.load(Ordering::Relaxed) {
        show();
        return Bool::NO;
    }
    Bool::YES
}

/// `NSApplicationDidResignActiveNotification`.
extern "C" fn did_resign(_this: *mut AnyObject, _cmd: Sel, _note: *mut AnyObject) {
    if HIDDEN.load(Ordering::Relaxed) {
        LEFT.store(true, Ordering::Relaxed);
    }
}

/// `NSApplicationDidBecomeActiveNotification`: only the first activation after RBS was
/// left while hidden shows the window, so a system dialog or a notification click does
/// not bring back a window the user just hid.
extern "C" fn did_become_active(_this: *mut AnyObject, _cmd: Sel, _note: *mut AnyObject) {
    if HIDDEN.load(Ordering::Relaxed) && LEFT.swap(false, Ordering::Relaxed) {
        show();
    }
}

/// The shared `NSApplication`, or null.
fn app() -> *mut AnyObject {
    // SAFETY: `sharedApplication` is a plain class getter; callers are on the main
    // thread (the toolkit's event loop thread), where AppKit objects may be used.
    unsafe { msg_send![class!(NSApplication), sharedApplication] }
}

/// An autoreleased `NSString` for `text`, or null.
fn ns_string(text: &CStr) -> *mut AnyObject {
    // SAFETY: `stringWithUTF8String:` copies the NUL-terminated UTF-8 text, which lives
    // for the call; it returns an autoreleased object or nil.
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

/// Adds `imp` for `sel` to `class` with the type encoding `types`; false when the
/// class already has a method for `sel`.
///
/// # Safety
///
/// `imp` must be an `extern "C"` function with exactly the signature `types` describes
/// and must never unwind.
unsafe fn add(class: &AnyClass, sel: Sel, imp: Imp, types: &CStr) -> bool {
    // SAFETY: the caller guarantees `imp` matches `types`; `class_addMethod` only adds
    // a method the class does not define; classes are process-wide and live forever.
    unsafe {
        objc2::ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel,
            imp,
            types.as_ptr().cast::<c_char>(),
        )
    }
    .as_bool()
}

/// Lets the Dock icon and Cmd+Tab bring a hidden main window back by calling `show`.
/// Call on the main thread once the toolkit's event loop exists (it sets the
/// application delegate). Returns false when the way back could not be set up (no
/// delegate; the delegate already answers one of the selectors, as a later winit might;
/// a second call): closing the window must then quit (ADR-0056 decision 6).
pub fn route_show(show: fn()) -> bool {
    if SHOW.set(show).is_err() {
        return false;
    }
    let app = app();
    if app.is_null() {
        return false;
    }
    // SAFETY: `app` is the non-null shared application, used on the main thread.
    let delegate: *mut AnyObject = unsafe { msg_send![app, delegate] };
    // SAFETY: `delegate` is checked for null; the application keeps it alive for the
    // whole process.
    let Some(delegate) = (unsafe { delegate.as_ref() }) else {
        return false;
    };
    let class: &AnyClass = delegate.class();
    // SAFETY: each function has exactly the signature of its type encoding: the reopen
    // method returns BOOL and takes self, the selector, the application and a BOOL; the
    // notification methods return void and take self, the selector and the
    // notification. None of them unwinds: they only touch atomics and run the show path
    // under `contain`.
    let added = unsafe {
        let reopen = std::mem::transmute::<
            extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool,
            Imp,
        >(should_reopen);
        let note = |f: extern "C" fn(*mut AnyObject, Sel, *mut AnyObject)| {
            std::mem::transmute::<extern "C" fn(*mut AnyObject, Sel, *mut AnyObject), Imp>(f)
        };
        add(
            class,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:),
            reopen,
            REOPEN_TYPES,
        ) && add(class, sel!(rbsDidResignActive:), note(did_resign), c"v@:@")
            && add(
                class,
                sel!(rbsDidBecomeActive:),
                note(did_become_active),
                c"v@:@",
            )
    };
    if !added {
        return false;
    }
    // SAFETY: `defaultCenter` is a plain getter. The observer is the delegate, which
    // lives for the whole process, so it never has to be removed; the selectors were
    // added above with the signature notifications are delivered with. The names are
    // the values of the AppKit constants, which equal their symbol names.
    unsafe {
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        if center.is_null() {
            return false;
        }
        let nil: *mut AnyObject = std::ptr::null_mut();
        let resign = ns_string(c"NSApplicationDidResignActiveNotification");
        let active = ns_string(c"NSApplicationDidBecomeActiveNotification");
        if resign.is_null() || active.is_null() {
            return false;
        }
        let observer: *const AnyObject = delegate;
        let _: () = msg_send![center, addObserver: observer, selector: sel!(rbsDidResignActive:), name: resign, object: nil];
        let _: () = msg_send![center, addObserver: observer, selector: sel!(rbsDidBecomeActive:), name: active, object: nil];
    }
    true
}

/// Whether the main window is hidden by Close. Showing it again by any path calls
/// this with false.
pub fn set_hidden(hidden: bool) {
    HIDDEN.store(hidden, Ordering::Relaxed);
    LEFT.store(false, Ordering::Relaxed);
}

/// The Dock tile's badge: `None` removes it. Main thread only.
pub fn set_badge(text: Option<&CStr>) {
    let app = app();
    if app.is_null() {
        return;
    }
    // SAFETY: `dockTile` is a plain getter on the non-null shared application, on the
    // main thread; `setBadgeLabel:` takes an NSString or nil (removes the badge) and
    // copies it.
    unsafe {
        let tile: *mut AnyObject = msg_send![app, dockTile];
        if tile.is_null() {
            return;
        }
        let label = text.map_or(std::ptr::null_mut(), ns_string);
        let _: () = msg_send![tile, setBadgeLabel: label];
    }
}

/// Sets the Dock icon to the PNG `png` at run time, so it shows the RBS logo also when
/// RBS runs as a bare executable (`cargo run`, no `.app` bundle). Returns false when
/// the image could not be made. Main thread only.
pub fn set_icon(png: &'static [u8]) -> bool {
    let app = app();
    if app.is_null() {
        return false;
    }
    // SAFETY: `dataWithBytes:length:` copies `png.len()` bytes from a live slice;
    // `initWithData:` takes that NSData and returns an owned NSImage or nil (an
    // undecodable image); `setApplicationIconImage:` retains the image, so the one `alloc`
    // reference is released after it. All on the non-null shared application on the
    // main thread.
    unsafe {
        let data: *mut AnyObject = msg_send![class!(NSData), dataWithBytes: png.as_ptr().cast::<std::ffi::c_void>(), length: png.len()];
        if data.is_null() {
            return false;
        }
        let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![image, initWithData: data];
        if image.is_null() {
            return false;
        }
        let _: () = msg_send![app, setApplicationIconImage: image];
        let _: () = msg_send![image, release];
    }
    true
}

/// Sets the application name macOS shows (menu bar, About, Dock) to `name`. A bare
/// executable has no `Info.plist`, so AppKit falls back to the file name (`rbs`); this
/// writes `CFBundleName` into the main bundle's info dictionary and sets the process
/// name. Call before the event loop starts. Main thread only.
pub fn set_app_name(name: &CStr) {
    let name = ns_string(name);
    if name.is_null() {
        return;
    }
    // SAFETY: `mainBundle` and `infoDictionary` are plain getters; the dictionary of a
    // bundle is an `NSMutableDictionary` in practice, checked with `isKindOfClass:`
    // before `setObject:forKey:` (which retains `name`). `processInfo` is a plain
    // getter and `setProcessName:` copies its argument. All on the main thread.
    unsafe {
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        if !bundle.is_null() {
            let info: *mut AnyObject = msg_send![bundle, infoDictionary];
            let mutable: Bool = if info.is_null() {
                Bool::NO
            } else {
                msg_send![info, isKindOfClass: class!(NSMutableDictionary)]
            };
            if mutable.as_bool() {
                let key = ns_string(c"CFBundleName");
                if !key.is_null() {
                    let _: () = msg_send![info, setObject: name, forKey: key];
                }
            }
        }
        let process: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        if !process.is_null() {
            let _: () = msg_send![process, setProcessName: name];
        }
    }
}

/// Bounces the Dock icon once, if RBS is not the active app (macOS does nothing
/// otherwise). Main thread only.
pub fn request_attention() {
    let app = app();
    if app.is_null() {
        return;
    }
    // SAFETY: `requestUserAttention:` takes an NSRequestUserAttentionType (NSUInteger)
    // and returns a request id (NSInteger), on the non-null shared application on the
    // main thread.
    let _: isize = unsafe { msg_send![app, requestUserAttention: INFORMATIONAL_REQUEST as usize] };
}
