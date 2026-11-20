//! Input for the page (ADR-0132 decision 6): `rbs_browser_ipc::input` events handed
//! to CEF's windowless browser host, on CEF's UI thread. The key tables are pure
//! functions in `rbs-browser-ipc`, tested without CEF.

use cef::*;
use rbs_browser_ipc::input::{
    self as wire, Button, InputEvent, Key, key_char, mac_key_char, mac_key_code, windows_key_code,
};

/// CEF's event flags for the wire's modifier bits.
fn flags(modifiers: u8) -> u32 {
    let mut out = 0;
    let mut set = |bit: u8, flag: u32| {
        if modifiers & bit != 0 {
            out |= flag;
        }
    };
    set(wire::SHIFT, 2);
    set(wire::CTRL, 4);
    set(wire::ALT, 8);
    set(wire::LEFT_HELD, 16);
    set(wire::MIDDLE_HELD, 32);
    set(wire::RIGHT_HELD, 64);
    set(wire::META, 128);
    out
}

fn mouse(x: i32, y: i32, modifiers: u8) -> MouseEvent {
    MouseEvent {
        x,
        y,
        modifiers: flags(modifiers),
    }
}

/// Sends one event to the page.
pub fn send(host: &BrowserHost, event: &InputEvent) {
    match event {
        InputEvent::Move { x, y, modifiers } => {
            host.send_mouse_move_event(Some(&mouse(*x, *y, *modifiers)), 0);
        }
        InputEvent::Leave { modifiers } => {
            host.send_mouse_move_event(Some(&mouse(0, 0, *modifiers)), 1);
        }
        InputEvent::Button {
            x,
            y,
            button,
            down,
            clicks,
            modifiers,
        } => {
            let kind = match button {
                Button::Left => MouseButtonType::LEFT,
                Button::Middle => MouseButtonType::MIDDLE,
                Button::Right => MouseButtonType::RIGHT,
            };
            host.send_mouse_click_event(
                Some(&mouse(*x, *y, *modifiers)),
                kind,
                i32::from(!*down),
                i32::from(*clicks),
            );
        }
        InputEvent::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => {
            host.send_mouse_wheel_event(Some(&mouse(*x, *y, *modifiers)), *dx, *dy);
        }
        InputEvent::Key {
            key,
            down,
            modifiers,
        } => send_key(host, *key, *down, *modifiers),
        InputEvent::Text(text) => {
            for unit in text.encode_utf16() {
                host.send_key_event(Some(&KeyEvent {
                    type_: KeyEventType::CHAR,
                    windows_key_code: i32::from(unit),
                    character: unit,
                    unmodified_character: unit,
                    ..Default::default()
                }));
            }
        }
    }
}

fn send_key(host: &BrowserHost, key: Key, down: bool, modifiers: u8) {
    let mac = cfg!(target_os = "macos");
    let character = if mac {
        mac_key_char(key)
    } else {
        key_char(key)
    }
    .unwrap_or(0);
    let event = KeyEvent {
        type_: if down {
            KeyEventType::RAWKEYDOWN
        } else {
            KeyEventType::KEYUP
        },
        modifiers: flags(modifiers),
        windows_key_code: windows_key_code(key),
        native_key_code: if mac {
            mac_key_code(key).unwrap_or(0)
        } else {
            0
        },
        character,
        unmodified_character: character,
        ..Default::default()
    };
    host.send_key_event(Some(&event));
    // Enter, Tab, Backspace and Escape also type their character, as a keyboard does
    // (not with a shortcut modifier held).
    if down
        && modifiers & (wire::CTRL | wire::META) == 0
        && let Some(c) = key_char(key)
    {
        host.send_key_event(Some(&KeyEvent {
            type_: KeyEventType::CHAR,
            modifiers: flags(modifiers),
            windows_key_code: i32::from(c),
            native_key_code: event.native_key_code,
            character: c,
            unmodified_character: c,
            ..Default::default()
        }));
    }
}
