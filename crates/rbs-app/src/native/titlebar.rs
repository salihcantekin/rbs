//! The toolbar row as the main window's title bar on macOS (ADR-0180 decision 2).
//!
//! After every show the window gets a full-size content view, a transparent title bar
//! with no title, its three standard buttons hidden and `movable = NO`, so the system's
//! title band neither draws nor drags over the row. It stays a titled, resizable
//! window: shadow, rounded corners, edge resizing, tiling and full screen stay
//! AppKit's. AppKit can show the standard buttons again (after full screen, on
//! activation, when the style mask is set again); three window notifications and each
//! resize hide them again, and the notifications report full screen. Four more mark
//! when the window starts and stops changing size, so frames are presented in step
//! with AppKit while it does (smooth full-screen animation and live resize). The
//! notifications go to an observer object of RBS's own, never to winit's window
//! delegate, whose own subscriptions to the same names must stay.
//!
//! Marked low-level module (P11): AppKit calls through objc2, each `unsafe` block with
//! its invariant. Everything runs on the main thread, which is the UI thread.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, MessageReceiver, Sel};
use objc2::{class, msg_send};
use objc2_core_foundation::CGRect;

/// `NSWindowStyleMaskFullScreen` and `NSWindowStyleMaskFullSizeContentView`.
const MASK_FULL_SCREEN: usize = 1 << 14;
const MASK_FULL_SIZE_CONTENT: usize = 1 << 15;
/// `NSWindowTitleHidden`.
const TITLE_HIDDEN: isize = 1;
/// `NSWindowCloseButton`, `NSWindowMiniaturizeButton`, `NSWindowZoomButton`.
const STANDARD_BUTTONS: [usize; 3] = [0, 1, 2];
/// `NSEventTypeLeftMouseDown`.
const LEFT_MOUSE_DOWN: usize = 1;

thread_local! {
    /// The frame the main window had before RBS zoomed it, for the way back.
    static UNZOOMED: Cell<Option<CGRect>> = const { Cell::new(None) };
    /// RBS's observer object, made once on the main thread and never freed.
    static OBSERVER: Cell<*mut AnyObject> = const { Cell::new(std::ptr::null_mut()) };
}

/// Told when the window enters (`true`) or leaves full screen.
static ON_FULL_SCREEN: OnceLock<fn(bool)> = OnceLock::new();

/// A notification method of the observer: (self, _cmd, the notification).
type Note = extern "C" fn(*mut AnyObject, Sel, *mut AnyObject);

/// The window notifications observed, as (selector of the observer, name, method).
/// The `Will…` ones and the end of a live resize switch presenting in step with
/// AppKit's transaction on and off (see [`present_in_step`]).
const NOTES: [(&CStr, &CStr, Note); 7] = [
    (
        c"rbsTitleEnterFullScreen:",
        c"NSWindowDidEnterFullScreenNotification",
        on_enter,
    ),
    (
        c"rbsTitleExitFullScreen:",
        c"NSWindowDidExitFullScreenNotification",
        on_exit,
    ),
    (
        c"rbsTitleBecomeKey:",
        c"NSWindowDidBecomeKeyNotification",
        on_key,
    ),
    (
        c"rbsTitleWillEnterFullScreen:",
        c"NSWindowWillEnterFullScreenNotification",
        on_will_change,
    ),
    (
        c"rbsTitleWillExitFullScreen:",
        c"NSWindowWillExitFullScreenNotification",
        on_will_change,
    ),
    (
        c"rbsTitleWillLiveResize:",
        c"NSWindowWillStartLiveResizeNotification",
        on_will_change,
    ),
    (
        c"rbsTitleEndLiveResize:",
        c"NSWindowDidEndLiveResizeNotification",
        on_changed,
    ),
];

/// The `NSWindow` of `ns_view`, or null while the view is in no window.
fn window_of(ns_view: NonNull<c_void>) -> *mut AnyObject {
    let view = ns_view.as_ptr().cast::<AnyObject>();
    // SAFETY: `ns_view` is the live NSView of a window this process shows, used on the
    // main thread; `window` is nil until the view is in a window.
    unsafe { msg_send![view, window] }
}

/// Whether `ns_view` is in a window yet.
pub fn in_window(ns_view: NonNull<c_void>) -> bool {
    !window_of(ns_view).is_null()
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

extern "C" fn on_will_change(_this: *mut AnyObject, _cmd: Sel, note: *mut AnyObject) {
    on_size_note(note, true);
}

extern "C" fn on_changed(_this: *mut AnyObject, _cmd: Sel, note: *mut AnyObject) {
    on_size_note(note, false);
}

/// The window a notification is about. Must run inside the observer method.
fn note_window<'a>(note: *mut AnyObject) -> Option<&'a AnyObject> {
    // SAFETY: AppKit passes a live NSNotification or nil.
    let note = unsafe { note.as_ref() }?;
    // SAFETY: its `object` is the window the observer was registered for, alive while
    // it posts; checked for null; this process's window on the main thread.
    unsafe {
        let window: *mut AnyObject = msg_send![note, object];
        window.as_ref()
    }
}

/// One observed notification: hides the buttons again and reports full screen; after
/// a full-screen change, presenting goes back to normal. Never unwinds into AppKit.
fn on_note(note: *mut AnyObject, full_screen: Option<bool>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(window) = note_window(note) {
            // SAFETY: a live window of this process on the main thread.
            unsafe { hide_standard_buttons(window) };
            if full_screen.is_some() {
                // SAFETY: as above.
                unsafe { present_in_step(window, false) };
            }
        }
        if let (Some(on), Some(f)) = (full_screen, ON_FULL_SCREEN.get()) {
            f(on);
        }
    }));
}

/// A size change starts (`true`: full screen in or out, a live resize) or a live
/// resize ends. Never unwinds into AppKit.
fn on_size_note(note: *mut AnyObject, starts: bool) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(window) = note_window(note) {
            // SAFETY: a live window of this process on the main thread.
            unsafe { present_in_step(window, starts) };
        }
    }));
}

/// Makes the window's Metal layers present in step with AppKit's transaction (`on`) or
/// as soon as drawn (`off`, the default). While the window changes size, a frame
/// presented on its own reaches the screen after AppKit has drawn the new window size,
/// so the full-screen animation showed the old-size picture with black around it until
/// it ended. In step costs a wait per frame, so it is on only during the change (P13).
///
/// # Safety
/// `window` is a live `NSWindow` of this process, on the main thread.
unsafe fn present_in_step(window: &AnyObject, on: bool) {
    let Some(metal) = AnyClass::get(c"CAMetalLayer") else {
        return;
    };
    // SAFETY: getters of a live window, view and layer, each nil-checked; `sublayers` is
    // an NSArray of CALayer or nil; `isKindOfClass:` and `setPresentsWithTransaction:`
    // take a class and a BOOL. The renderer reads the flag when it takes the next
    // drawable, on this thread.
    unsafe {
        let view: *mut AnyObject = msg_send![window, contentView];
        let Some(view) = view.as_ref() else {
            return;
        };
        let root: *mut AnyObject = msg_send![view, layer];
        let Some(root) = root.as_ref() else {
            return;
        };
        let set = |layer: &AnyObject| {
            let is_metal: bool = msg_send![layer, isKindOfClass: metal];
            if is_metal {
                let _: () = msg_send![layer, setPresentsWithTransaction: on];
            }
        };
        set(root);
        let subs: *mut AnyObject = msg_send![root, sublayers];
        if let Some(subs) = subs.as_ref() {
            let count: usize = msg_send![subs, count];
            for i in 0..count {
                let layer: *mut AnyObject = msg_send![subs, objectAtIndex: i];
                if let Some(layer) = layer.as_ref() {
                    set(layer);
                }
            }
        }
    }
}

/// RBS's observer: an `NSObject` subclass with the notification methods, made
/// on first use. Null if the class cannot be made (a class of that name exists).
fn observer() -> *mut AnyObject {
    OBSERVER.with(|o| {
        if o.get().is_null() {
            o.set(make_observer());
        }
        o.get()
    })
}

fn make_observer() -> *mut AnyObject {
    let Some(mut builder) = ClassBuilder::new(c"RBSTitleBarObserver", class!(NSObject)) else {
        return std::ptr::null_mut();
    };
    for (name, _, f) in NOTES {
        // SAFETY: each function is (self, _cmd, one object) -> void, as a notification
        // method is called, and never unwinds (`on_note` catches). The names are new to
        // NSObject, so nothing is overridden.
        unsafe { builder.add_method(Sel::register(name), f) };
    }
    let class = builder.register();
    // SAFETY: `new` on a registered NSObject subclass returns a +1 object or nil.
    let object: Option<Retained<AnyObject>> = unsafe { msg_send![class, new] };
    // Kept for the life of the process: the notification center does not retain it.
    object.map_or(std::ptr::null_mut(), Retained::into_raw)
}

/// Changes the window of `ns_view` as ADR-0180 decision 2 says and observes it (an
/// earlier window's observation ends); `on_full_screen` hears full screen change. Call
/// after every show. Returns the system title band's height in points, which the row
/// must cover, or why it could not be done: then the window is left unchanged and the
/// OS title bar stays.
pub fn attach(ns_view: NonNull<c_void>, on_full_screen: fn(bool)) -> Result<f64, &'static str> {
    autoreleasepool(|_| attach_in_pool(ns_view, on_full_screen))
}

fn attach_in_pool(ns_view: NonNull<c_void>, on_full_screen: fn(bool)) -> Result<f64, &'static str> {
    let _ = ON_FULL_SCREEN.set(on_full_screen);
    let window = window_of(ns_view);
    // SAFETY: checked for null; this process's NSWindow on the main thread.
    let Some(window) = (unsafe { window.as_ref() }) else {
        return Err("the view is in no window yet");
    };
    // Everything that can fail is looked up before the window changes.
    let observer = observer();
    // SAFETY: checked for null; kept alive for the process (`make_observer`).
    let Some(observer) = (unsafe { observer.as_ref() }) else {
        return Err("no notification observer");
    };
    // SAFETY: `defaultCenter` is a class getter returning the shared center.
    let center: *mut AnyObject = unsafe { msg_send![class!(NSNotificationCenter), defaultCenter] };
    // SAFETY: checked for null; the shared center lives for the process.
    let Some(center) = (unsafe { center.as_ref() }) else {
        return Err("no notification center");
    };
    let mut names = [std::ptr::null_mut::<AnyObject>(); NOTES.len()];
    for (slot, (_, name, _)) in names.iter_mut().zip(NOTES) {
        *slot = ns_string(name);
        if slot.is_null() {
            return Err("no notification name");
        }
    }
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
    // SAFETY: the observer is RBS's own object, so removing all of its observations
    // touches nothing of winit's or AppKit's; it ends an earlier window's, so a second
    // attach never doubles one. Each name is a live NSString (checked above), the
    // selector one the observer's class has.
    unsafe {
        let observer: *const AnyObject = observer;
        let object: *const AnyObject = window;
        let _: () = msg_send![center, removeObserver: observer];
        for ((selector, _, _), name) in NOTES.iter().zip(names) {
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

/// Hides the standard buttons of the window of `ns_view` again: AppKit can rebuild
/// them when the style mask is set again (winit does on a resizable change), which no
/// notification reports. Called on each resize.
pub fn hide_buttons_again(ns_view: NonNull<c_void>) {
    let window = window_of(ns_view);
    // SAFETY: checked for null; this process's NSWindow on the main thread.
    if let Some(window) = unsafe { window.as_ref() } {
        // SAFETY: as above.
        unsafe { hide_standard_buttons(window) };
    }
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
            Button::Zoom => zoom(window),
            Button::Close => {
                let _: () = msg_send![window, performClose: nil];
            }
        }
    }
}

/// Zooms `window` to its screen's visible area, or back to the frame it had, without
/// AppKit's animation: during `zoom:`'s animated frame change the toolkit draws no new
/// frame, so the old-size picture sat in the growing window with black around it
/// (owner's Mac test, 2026-11-24). A window zoomed some other way (the Window menu,
/// tiling) is restored by `zoom:` itself.
///
/// # Safety
/// `window` is a live `NSWindow` of this process, on the main thread.
unsafe fn zoom(window: &AnyObject) {
    // SAFETY: getters and setters of a live window and its screen with their declared
    // types (BOOL, NSRect, an NSScreen or nil); `zoom:` takes a nil sender.
    unsafe {
        let zoomed: bool = msg_send![window, isZoomed];
        if zoomed {
            match UNZOOMED.with(Cell::take) {
                Some(frame) => {
                    let _: () = msg_send![window, setFrame: frame, display: true, animate: false];
                }
                None => {
                    let _: () = msg_send![window, zoom: std::ptr::null_mut::<AnyObject>()];
                }
            }
            return;
        }
        let screen: *mut AnyObject = msg_send![window, screen];
        let Some(screen) = screen.as_ref() else {
            let _: () = msg_send![window, zoom: std::ptr::null_mut::<AnyObject>()];
            return;
        };
        let target: CGRect = msg_send![screen, visibleFrame];
        let frame: CGRect = msg_send![window, frame];
        UNZOOMED.with(|u| u.set(Some(frame)));
        let _: () = msg_send![window, setFrame: target, display: true, animate: false];
    }
}

/// Puts the window of `ns_view` into the system's full screen, or out of it.
pub fn toggle_full_screen(ns_view: NonNull<c_void>) {
    let window = window_of(ns_view);
    // SAFETY: checked for null; `toggleFullScreen:` takes a sender, which may be nil,
    // and returns nothing.
    if let Some(window) = unsafe { window.as_ref() } {
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![window, toggleFullScreen: std::ptr::null_mut::<AnyObject>()];
        }
    }
}

/// Turns off AppKit's own "Enter Full Screen" menu item, since View has RBS's (ADR-0180
/// decision 5): registers `NSFullScreenMenuItemEverywhere` as NO. Call before the
/// event loop starts. Apple does not document the key; a macOS that ignores it shows
/// a second, working item.
pub fn hide_system_full_screen_item() {
    autoreleasepool(|_| {
        let key = ns_string(c"NSFullScreenMenuItemEverywhere");
        if key.is_null() {
            return;
        }
        // SAFETY: class methods returning autoreleased objects or nil, each checked;
        // `numberWithBool:` takes a BOOL, `dictionaryWithObject:forKey:` two objects,
        // `registerDefaults:` a dictionary and returns nothing.
        unsafe {
            let no: *mut AnyObject = msg_send![class!(NSNumber), numberWithBool: false];
            if no.is_null() {
                return;
            }
            let dict: *mut AnyObject =
                msg_send![class!(NSDictionary), dictionaryWithObject: no, forKey: key];
            let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
            if let (Some(dict), Some(defaults)) = (dict.as_ref(), defaults.as_ref()) {
                let _: () = msg_send![defaults, registerDefaults: dict];
            }
        }
    });
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
/// event AppKit is dispatching). False when that event is not a left mouse down: winit
/// can deliver a press late, and `clickCount` raises on other events.
pub fn is_double_click() -> bool {
    // SAFETY: `sharedApplication` and `currentEvent` are getters on the main thread;
    // `currentEvent` is nil outside event dispatch; `type` returns NSEventType
    // (NSUInteger), sent by a dynamic selector because `type` is a Rust keyword;
    // `clickCount` returns NSInteger and is asked only of a mouse down.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app.as_ref() else {
            return false;
        };
        let event: *mut AnyObject = msg_send![app, currentEvent];
        let Some(event) = event.as_ref() else {
            return false;
        };
        let kind: usize = event.send_message(Sel::register(c"type"), ());
        if kind != LEFT_MOUSE_DOWN {
            return false;
        }
        let count: isize = msg_send![event, clickCount];
        count == 2
    }
}

/// The system's "Double-click a window's title bar to" values: `AppleActionOnDoubleClick`
/// and the older `AppleMiniaturizeOnDoubleClick` (see `caption::double_click`).
pub fn double_click_setting() -> (Option<String>, Option<bool>) {
    autoreleasepool(|_| double_click_in_pool())
}

fn double_click_in_pool() -> (Option<String>, Option<bool>) {
    // SAFETY: `standardUserDefaults` is a getter; `stringForKey:` returns nil or an
    // NSString (nil for a value of another type) whose UTF-8 buffer lives until the
    // pool of `double_click_setting` drains; `objectForKey:` returns nil or an object,
    // only tested for presence; `boolForKey:` returns BOOL for a value of any type.
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
            (!value.is_null()).then(|| msg_send![defaults, boolForKey: key])
        };
        (action, miniaturize)
    }
}
