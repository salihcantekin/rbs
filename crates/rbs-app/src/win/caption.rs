//! The toolbar row is the title bar on Windows (ADR-0071 decision 2): a subclass of the
//! main window's procedure removes the caption, answers the hit test from the regions
//! the UI sends, and gives the maximize button its mouse through the OS, so Windows 11
//! shows its snap layouts there. The arithmetic is in [`crate::caption`].
//!
//! Everything here runs on the UI thread, which owns the window: the state is a
//! `thread_local`, read without allocation for every mouse message (P13).

use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::rc::Rc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::Controls::MARGINS;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed,
    NCCALCSIZE_PARAMS, PostMessageW, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE,
    SM_CXPADDEDBORDER, SM_CYFRAME, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_NOZORDER, SetWindowPos, WM_MOUSEMOVE, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE,
    WM_SYSCOMMAND,
};

use crate::caption::{Hit, Probe, Regions, client_top, hit, to_client};

/// What the window tells the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The pointer is over the maximize button, or left it.
    MaxHover(bool),
    /// The maximize button is held down, or let go.
    MaxPressed(bool),
    /// The window was maximized, or restored.
    Maximized(bool),
}

/// The window buttons' actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Minimize.
    Minimize,
    /// Maximize, or Restore while maximized.
    Maximize,
    /// Close (ADR-0056's close path).
    Close,
}

/// The subclass's id among the window's subclasses.
const SUBCLASS_ID: usize = 0x5242_5354; // "RBST"

struct State {
    hwnd: HWND,
    regions: Regions,
    hover: bool,
    pressed: bool,
    /// The UI's side: sent hover, pressed and maximized changes.
    sink: Rc<dyn Fn(Event)>,
    /// Whether a popup is open in the window now.
    popup_open: Rc<dyn Fn() -> bool>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The window the caption is removed from, if any.
pub fn window() -> Option<HWND> {
    STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|s| s.hwnd))
}

/// Removes the caption of `window` (the main window, as `raw-window-handle` gives it)
/// and keeps it removed until the window is destroyed. Again for the same window does
/// nothing; for a new window (the main window was made again), moves to it. Returns
/// whether the caption is removed. A handle that is not a window of this thread is
/// refused by Windows, and nothing changes.
pub fn install(
    window: NonZeroIsize,
    sink: Rc<dyn Fn(Event)>,
    popup_open: Rc<dyn Fn() -> bool>,
) -> bool {
    let hwnd = window.get() as HWND;
    if self::window() == Some(hwnd) {
        return true;
    }
    let stored = STATE.with(|s| {
        s.try_borrow_mut()
            .map(|mut s| {
                *s = Some(State {
                    hwnd,
                    regions: Regions::default(),
                    hover: false,
                    pressed: false,
                    sink,
                    popup_open,
                });
            })
            .is_ok()
    });
    if !stored {
        return false;
    }
    // SAFETY: Windows checks the handle (a window of another thread or none is
    // refused); `procedure` matches SUBCLASSPROC and lives for the program.
    let ok = unsafe { SetWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID, 0) } != 0;
    if !ok {
        STATE.with(|s| {
            if let Ok(mut s) = s.try_borrow_mut() {
                *s = None;
            }
        });
        return false;
    }
    // Windows 10 draws no border at the top once the caption is gone: a one-pixel frame
    // there keeps its line and shadow. Windows 11 (which knows corner preferences)
    // draws its border around the whole window itself.
    let pref = DWMWCP_DEFAULT;
    // SAFETY: the subclass took, so `hwnd` is a live window of this thread; the attribute value is an i32 that outlives the
    // call, with its size passed.
    let windows_11 = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&raw const pref).cast(),
            std::mem::size_of_val(&pref) as u32,
        )
    } >= 0;
    if !windows_11 {
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 1,
            cyBottomHeight: 0,
        };
        // SAFETY: `hwnd` is live; `margins` outlives the call.
        unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
    }
    // The frame changed: Windows asks WM_NCCALCSIZE again.
    // SAFETY: `hwnd` is live; no position, size or order changes.
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        )
    };
    true
}

/// The regions the hit test answers from (the UI sends them when one moves).
pub fn set_regions(r: Regions) {
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut()
            && let Some(s) = s.as_mut()
        {
            s.regions = r;
        }
    });
}

/// Whether the window is maximized now.
pub fn maximized() -> bool {
    // SAFETY: `IsZoomed` only reads the window's state; a null or stale handle answers 0.
    window().is_some_and(|h| unsafe { IsZoomed(h) } != 0)
}

/// Runs a window button: the same system commands as the OS's own buttons, so Close
/// reaches winit as a close request (ADR-0056's close path).
pub fn press(button: Button) {
    let Some(hwnd) = window() else { return };
    let command = match button {
        Button::Minimize => SC_MINIMIZE,
        Button::Maximize if maximized() => SC_RESTORE,
        Button::Maximize => SC_MAXIMIZE,
        Button::Close => SC_CLOSE,
    };
    // SAFETY: `hwnd` is the live main window; posting a system command is asynchronous.
    unsafe { PostMessageW(hwnd, WM_SYSCOMMAND, command as WPARAM, 0) };
}

/// The resize band's height in physical pixels at the window's DPI.
fn frame_px(hwnd: HWND) -> (i32, f32) {
    // SAFETY: reads the DPI of the live window; a stale handle answers 0.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    // SAFETY: reads system metrics for a DPI; no pointers.
    let frame = unsafe {
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    };
    (frame, dpi as f32 / 96.0)
}

/// Tells the UI, outside the state's borrow (the UI may send regions back).
fn notify(e: Event) {
    let sink = STATE.with(|s| s.try_borrow().ok()?.as_ref().map(|s| s.sink.clone()));
    if let Some(sink) = sink {
        sink(e);
    }
}

/// Updates the maximize button's hover and pressed state, telling the UI on a change.
fn set_max(hover: Option<bool>, pressed: Option<bool>) {
    let mut events = [None, None];
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut()
            && let Some(s) = s.as_mut()
        {
            if let Some(h) = hover
                && h != s.hover
            {
                s.hover = h;
                events[0] = Some(Event::MaxHover(h));
            }
            if let Some(p) = pressed
                && p != s.pressed
            {
                s.pressed = p;
                events[1] = Some(Event::MaxPressed(p));
            }
        }
    });
    for e in events.into_iter().flatten() {
        notify(e);
    }
}

fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
    let (frame, scale) = frame_px(hwnd);
    let mut pt = POINT {
        x: (lparam & 0xffff) as i16 as i32,
        y: ((lparam >> 16) & 0xffff) as i16 as i32,
    };
    let screen = (pt.x, pt.y);
    // SAFETY: `hwnd` is live; `pt` is a valid POINT this function owns.
    if unsafe { ScreenToClient(hwnd, &mut pt) } == 0 {
        return None;
    }
    let origin = (screen.0 - pt.x, screen.1 - pt.y);
    let (x, y) = to_client(screen, origin, scale);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `hwnd` is live; `rect` is a valid RECT this function owns.
    unsafe { GetClientRect(hwnd, &mut rect) };
    let (regions, popup_open) = STATE.with(|s| {
        let s = s.try_borrow().ok()?;
        let s = s.as_ref()?;
        Some((s.regions, s.popup_open.clone()))
    })?;
    let popup_open = popup_open();
    let probe = Probe {
        x,
        y,
        width: (rect.right - rect.left) as f32 / scale,
        frame: frame as f32 / scale,
        maximized: maximized(),
        popup_open,
    };
    Some(match hit(probe, regions) {
        Hit::Client => HTCLIENT,
        Hit::Caption => HTCAPTION,
        Hit::MaxButton => HTMAXBUTTON,
        Hit::Top => HTTOP,
        Hit::TopLeft => HTTOPLEFT,
        Hit::TopRight => HTTOPRIGHT,
    })
}

/// The window procedure in front of winit's. A panic is contained (ADR-0055) and the
/// message goes to the next procedure as if this one were not there.
unsafe extern "system" fn procedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match rbs_crash::contain(|| handle(hwnd, msg, wparam, lparam)) {
        Ok(r) => r,
        // SAFETY: Windows' own arguments for this window, passed on unchanged.
        Err(_) => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

/// Handles one message of the main window, on its own thread, with the window live and
/// the parameters as Windows documents them.
fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: Windows' own arguments for this window, passed on unchanged.
    let pass = || unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    match msg {
        WM_NCCALCSIZE if wparam != 0 => {
            let params = lparam as *mut NCCALCSIZE_PARAMS;
            // SAFETY: with wparam TRUE, lparam points at a NCCALCSIZE_PARAMS Windows owns
            // for this call.
            let top = unsafe { (*params).rgrc[0].top };
            let result = pass();
            let (frame, _) = frame_px(hwnd);
            // SAFETY: only reads the live window's state.
            let zoomed = unsafe { IsZoomed(hwnd) } != 0;
            // SAFETY: as above; the default procedure has set the side and bottom edges,
            // and only the top is put back.
            unsafe { (*params).rgrc[0].top = client_top(top, zoomed, frame) };
            result
        }
        WM_NCHITTEST => {
            let default = pass();
            // The sides and the bottom stay the OS's resize edges.
            if default != HTCLIENT as LRESULT && default != HTCAPTION as LRESULT {
                return default;
            }
            hit_test(hwnd, lparam).map_or(default, |h| h as LRESULT)
        }
        WM_NCMOUSEMOVE => {
            let over = wparam == HTMAXBUTTON as usize;
            if over {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `track` is a valid, sized TRACKMOUSEEVENT for the live window.
                unsafe { TrackMouseEvent(&mut track) };
            }
            set_max(Some(over), if over { None } else { Some(false) });
            pass()
        }
        WM_NCMOUSELEAVE | WM_MOUSEMOVE => {
            set_max(Some(false), Some(false));
            pass()
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTMAXBUTTON as usize => {
            set_max(None, Some(true));
            0
        }
        WM_NCLBUTTONUP if wparam == HTMAXBUTTON as usize => {
            let was = STATE.with(|s| {
                s.try_borrow()
                    .ok()
                    .and_then(|s| s.as_ref().map(|s| s.pressed))
                    .unwrap_or(false)
            });
            set_max(None, Some(false));
            if was {
                press(Button::Maximize);
            }
            0
        }
        WM_SIZE => {
            // SAFETY: only reads the live window's state.
            let zoomed = unsafe { IsZoomed(hwnd) } != 0;
            notify(Event::Maximized(zoomed));
            pass()
        }
        WM_NCDESTROY => {
            // SAFETY: removes this subclass from the window being destroyed.
            unsafe { RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID) };
            STATE.with(|s| {
                if let Ok(mut s) = s.try_borrow_mut()
                    && s.as_ref().is_some_and(|s| s.hwnd == hwnd)
                {
                    *s = None;
                }
            });
            pass()
        }
        _ => pass(),
    }
}
