//! The toolbar row as the main window's title bar on macOS (ADR-0180 decision 2).
//!
//! After every show the window gets a full-size content view, a transparent title bar
//! with no title, its three standard buttons hidden and `movable = NO`, so the system's
//! title band neither draws nor drags over the row. It stays a titled, resizable
//! window: shadow, rounded corners, edge resizing, tiling and full screen stay
//! AppKit's. AppKit can show the standard buttons again (after full screen, on
//! activation); three window notifications hide them again and report full screen.
//!
//! Marked low-level module (P11): AppKit calls through objc2, each `unsafe` block with
//! its invariant. Everything runs on the main thread, which is the UI thread.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send};
use objc2_core_foundation::CGRect;

/// `NSWindowStyleMaskFullScreen` and `NSWindowStyleMaskFullSizeContentView`.
const MASK_FULL_SCREEN: usize = 1 << 14;
const MASK_FULL_SIZE_CONTENT: usize = 1 << 15;
/// `NSWindowTitleHidden`.
const TITLE_HIDDEN: isize = 1;
/// `NSWindowCloseButton`, `NSWindowMiniaturizeButton`, `NSWindowZoomButton`.
const STANDARD_BUTTONS: [usize; 3] = [0, 1, 2];

/// Told when the window enters (`true`) or leaves full screen.
static ON_FULL_SCREEN: OnceLock<fn(bool)> = OnceLock::new();

/// The window notifications observed, as (selector added to the delegate, name).
const NOTES: [(&CStr, &CStr); 3] = [
    (
        c"rbsTitleEnterFullScreen:",
        c"NSWindowDidEnterFullScreenNotification",
    ),
    (
        c"rbsTitleExitFullScreen:",
        c"NSWindowDidExitFullScreenNotification",
    ),
    (c"rbsTitleBecomeKey:", c"NSWindowDidBecomeKeyNotification"),
];

/// The `NSWindow` of `ns_view`, or null while the view is in no window.
fn window_of(ns_view: NonNull<c_void>) -> *mut AnyObject {
    let view = ns_view.as_ptr().cast::<AnyObject>();
    // SAFETY: `ns_view` is the live NSView of a window this process shows, used on the
    // main thread; `window` is nil until the view is in a window.
    unsafe { msg_send![view, window] }
}

/// An autoreleased `NSString`, or null.
fn ns_string(text: &CStr) -> *mut AnyObject {
    // SAFETY: `text` is NUL-terminated and lives for the call; the string is copied.
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

/// Hides the close, minimize and zoom buttons of `window`.
///
/// # Safety
/// `window` is a live `NSWindow` of this process, on the main thread.
unsafe fn hide_standard_buttons(window: &AnyObject) {
    for kind in STANDARD_BUTTONS {
        // SAFETY: the caller's window; `standardWindowButton:` returns nil or a button
        // the window owns; `setHidden:` takes a BOOL and returns nothing.
        unsafe {
            let button: *mut AnyObject = msg_send![window, standardWindowButton: kind];
            if let Some(button) = button.as_ref() {
                let _: () = msg_send![button, setHidden: true];
            }
        }
    }
}

extern "C" fn on_enter(_this: *mut AnyObject, _cmd: Sel, note: *mut AnyObject) {
    on_note(note, Some(true));
}

extern "C" fn on_exit(_this: *mut AnyObject, _cmd: Sel, note: *mut AnyObject) {
    on_note(note, Some(false));
}

extern "C" fn on_key(_this: *mut AnyObject, _cmd: Sel, note: *mut AnyObject) {
    on_note(note, None);
}

/// One observed notification: hides the buttons again and reports full screen. Never
/// unwinds into AppKit.
fn on_note(note: *mut AnyObject, full_screen: Option<bool>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: AppKit passes a live NSNotification or nil.
        let Some(note) = (unsafe { note.as_ref() }) else {
            return;
        };
        // SAFETY: its `object` is the window the observer was registered for, alive
        // while it posts.
        let window: *mut AnyObject = unsafe { msg_send![note, object] };
        // SAFETY: checked for null; this process's window on the main thread.
        if let Some(window) = unsafe { window.as_ref() } {
            // SAFETY: as above.
            unsafe { hide_standard_buttons(window) };
        }
        if let (Some(on), Some(f)) = (full_screen, ON_FULL_SCREEN.get()) {
            f(on);
        }
    }));
}

/// Adds the observer methods to the delegate's class once per class.
///
/// # Safety
/// `class` is a live Objective-C class.
unsafe fn add_methods(class: &AnyClass) {
    type Note = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject);
    let methods: [(&CStr, Note); 3] = [
        (NOTES[0].0, on_enter),
        (NOTES[1].0, on_exit),
        (NOTES[2].0, on_key),
    ];
    for (name, f) in methods {
        let sel = Sel::register(name);
        // SAFETY: each function has the signature "v@:@" says (void, self, _cmd, one
        // object) and never unwinds (`on_note` catches). `class_addMethod` leaves a
        // method the class already has alone, so a second window adds nothing.
        unsafe {
            let imp = std::mem::transmute::<Note, Imp>(f);
            objc2::ffi::class_addMethod(
                std::ptr::from_ref(class).cast_mut(),
                sel,
                imp,
                c"v@:@".as_ptr().cast::<c_char>(),
            );
        }
    }
}

/// Changes the window of `ns_view` as ADR-0180 decision 2 says and (re)registers the
/// three observers; `on_full_screen` hears full screen change. Call after every show.
/// Returns the system title band's height in points, which the row must cover, or why
/// it could not be done (the OS title bar then stays).
pub fn attach(ns_view: NonNull<c_void>, on_full_screen: fn(bool)) -> Result<f64, &'static str> {
    let _ = ON_FULL_SCREEN.set(on_full_screen);
    let window = window_of(ns_view);
    // SAFETY: checked for null; this process's NSWindow on the main thread.
    let Some(window) = (unsafe { window.as_ref() }) else {
        return Err("the view is in no window yet");
    };
    // SAFETY: plain setters and getters of a live NSWindow with the argument types of
    // their declarations (NSUInteger mask, BOOL, NSInteger title visibility).
    unsafe {
        let mask: usize = msg_send![window, styleMask];
        let _: () = msg_send![window, setStyleMask: mask | MASK_FULL_SIZE_CONTENT];
        let _: () = msg_send![window, setTitlebarAppearsTransparent: true];
        let _: () = msg_send![window, setTitleVisibility: TITLE_HIDDEN];
        let _: () = msg_send![window, setMovable: false];
        hide_standard_buttons(window);
    }
    // SAFETY: `delegate` is a getter; winit's delegate lives as long as the window.
    let delegate: *mut AnyObject = unsafe { msg_send![window, delegate] };
    // SAFETY: checked for null; alive with the window.
    let Some(delegate) = (unsafe { delegate.as_ref() }) else {
        return Err("the window has no delegate");
    };
    // SAFETY: the delegate's class is a live class.
    unsafe { add_methods(delegate.class()) };
    // SAFETY: `defaultCenter` is a getter. For each name the observation of this
    // delegate for this window is removed first, so a second attach never doubles it,
    // then added with a selector added above. Only these names are removed: AppKit's
    // own delegate registrations for other names stay.
    unsafe {
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let Some(center) = center.as_ref() else {
            return Err("no notification center");
        };
        for (selector, name) in NOTES {
            let name = ns_string(name);
            if name.is_null() {
                return Err("no notification name");
            }
            let observer: *const AnyObject = delegate;
            let object: *const AnyObject = window;
            let _: () = msg_send![center, removeObserver: observer, name: name, object: object];
            let _: () = msg_send![
                center,
                addObserver: observer,
                selector: Sel::register(selector),
                name: name,
                object: object
            ];
        }
    }
    Ok(band_height(ns_view))
}

/// The height of the system's title band in points: the window's frame minus the
/// area the title bar does not cover. 0 while the view is in no window.
pub fn band_height(ns_view: NonNull<c_void>) -> f64 {
    let window = window_of(ns_view);
    // SAFETY: checked for null; this process's window on the main thread.
    let Some(window) = (unsafe { window.as_ref() }) else {
        return 0.0;
    };
    // SAFETY: `frame` and `contentLayoutRect` are getters returning NSRect values.
    let (frame, layout): (CGRect, CGRect) = unsafe {
        (
            msg_send![window, frame],
            msg_send![window, contentLayoutRect],
        )
    };
    (frame.size.height - layout.size.height).max(0.0)
}

/// A window button of the row (decision 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// `miniaturize:`.
    Minimize,
    /// `zoom:` (zooms or restores).
    Zoom,
    /// `performClose:`: a close request, ADR-0056's close path.
    Close,
}

/// Runs a window button on the window of `ns_view`.
pub fn press(ns_view: NonNull<c_void>, button: Button) {
    let window = window_of(ns_view);
    // SAFETY: checked for null; each action takes a sender, which may be nil.
    let Some(window) = (unsafe { window.as_ref() }) else {
        return;
    };
    let nil = std::ptr::null_mut::<AnyObject>();
    // SAFETY: as above; these return nothing.
    unsafe {
        match button {
            Button::Minimize => {
                let _: () = msg_send![window, miniaturize: nil];
            }
            Button::Zoom => {
                let _: () = msg_send![window, zoom: nil];
            }
            Button::Close => {
                let _: () = msg_send![window, performClose: nil];
            }
        }
    }
}

/// Whether the window of `ns_view` is zoomed now.
pub fn zoomed(ns_view: NonNull<c_void>) -> bool {
    let window = window_of(ns_view);
    // SAFETY: checked for null; `isZoomed` returns BOOL.
    unsafe { window.as_ref().is_some_and(|w| msg_send![w, isZoomed]) }
}

/// Whether the window of `ns_view` is in full screen now.
pub fn full_screen(ns_view: NonNull<c_void>) -> bool {
    let window = window_of(ns_view);
    // SAFETY: checked for null; `styleMask` returns NSUInteger.
    unsafe {
        window.as_ref().is_some_and(|w| {
            let mask: usize = msg_send![w, styleMask];
            mask & MASK_FULL_SCREEN != 0
        })
    }
}

/// The press being handled now is the second of a double-click (`clickCount` of the
/// event AppKit is dispatching).
pub fn is_double_click() -> bool {
    // SAFETY: `sharedApplication` and `currentEvent` are getters on the main thread;
    // `currentEvent` is nil outside event dispatch; `clickCount` returns NSInteger and is
    // valid for mouse events, which is what is being dispatched when this is asked.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app.as_ref() else {
            return false;
        };
        let event: *mut AnyObject = msg_send![app, currentEvent];
        let Some(event) = event.as_ref() else {
            return false;
        };
        let count: isize = msg_send![event, clickCount];
        count == 2
    }
}

/// The system's "Double-click a window's title bar to" values: `AppleActionOnDoubleClick`
/// and the older `AppleMiniaturizeOnDoubleClick` (see `caption::double_click`).
pub fn double_click_setting() -> (Option<String>, Option<bool>) {
    // SAFETY: `standardUserDefaults` is a getter; `stringForKey:` returns nil or an
    // NSString whose UTF-8 buffer lives until the autorelease pool drains, after this
    // call; `objectForKey:` returns nil or an object, `boolValue` a BOOL for NSNumber and
    // NSString, the only types the defaults system stores for this key.
    unsafe {
        let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let Some(defaults) = defaults.as_ref() else {
            return (None, None);
        };
        let key = ns_string(c"AppleActionOnDoubleClick");
        let action = if key.is_null() {
            None
        } else {
            let value: *mut AnyObject = msg_send![defaults, stringForKey: key];
            value.as_ref().and_then(|v| {
                let text: *const c_char = msg_send![v, UTF8String];
                (!text.is_null()).then(|| CStr::from_ptr(text).to_string_lossy().into_owned())
            })
        };
        let key = ns_string(c"AppleMiniaturizeOnDoubleClick");
        let miniaturize = if key.is_null() {
            None
        } else {
            let value: *mut AnyObject = msg_send![defaults, objectForKey: key];
            value.as_ref().map(|v| msg_send![v, boolValue])
        };
        (action, miniaturize)
    }
}
